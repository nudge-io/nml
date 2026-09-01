use std::borrow::Cow;

use crate::ast::{
    ArmTarget, Body, BodyEntry, BodyEntryKind, File, ListItem, ListItemKind, Modifier,
    ModifierValue,
};
use crate::diagnostic::{Code, Diagnostic};
use crate::schema_index::SchemaIndex;
use crate::span::Span;
use crate::types::Value;

use super::decide::*;
use super::entries::*;
use super::grants::*;
use super::instances::*;
use super::policy::*;
use super::seal::*;
use super::*;

mod grants;
mod items;
mod linearize;
mod merge;
mod normalize;
mod oneof;
mod order;
mod perf;
mod policy;
mod seal;
mod union;

fn index_from(schema: &str) -> SchemaIndex {
    let mut ex = crate::cst::extract_schema(schema).0;
    crate::schema::resolve_model_inheritance(&mut ex);
    SchemaIndex::build(ex.models, ex.enums, ex.oneofs)
}

fn file_of(src: &str) -> File {
    let (file, diags) = crate::cst::parse_to_ast_all(src);
    assert!(diags.is_empty(), "parse diags: {diags:?}");
    file
}

/// Compose the named block in `src` under `schema`, open context.
fn compose(
    schema: &str,
    src: &str,
    root: &str,
    name: &str,
) -> (Option<ResolvedInstance>, Vec<Diagnostic>) {
    compose_with(schema, src, root, name, &OpenContext)
}

fn compose_with(
    schema: &str,
    src: &str,
    root: &str,
    name: &str,
    grants: &dyn LayerGrantProvider,
) -> (Option<ResolvedInstance>, Vec<Diagnostic>) {
    // RFC 0025 Phase 1 — the corpus harvest: under
    // `NML_CORPUS_DUMP=<dir>`, every battery composition writes its
    // inputs for the two-binary oracle comparison. Keyed by thread
    // name PLUS a per-thread call counter — one test composes many
    // times, and thread names repeat across processes.
    if let Ok(dir) = std::env::var("NML_CORPUS_DUMP") {
        use std::cell::Cell;
        thread_local! {
            static CALLS: Cell<u32> = const { Cell::new(0) };
        }
        let n = CALLS.with(|c| {
            let v = c.get();
            c.set(v + 1);
            v
        });
        let thread = std::thread::current();
        let test = thread.name().unwrap_or("anon").to_string();
        let entry = serde_json::json!({
            "test": test,
            "schema": schema,
            "source": src,
            "root": root,
            "declaration": name,
        });
        let file = format!("{}.{n}.json", test.replace(':', "_"));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            std::path::Path::new(&dir).join(file),
            serde_json::to_string_pretty(&entry).expect("corpus entry serializes"),
        );
    }
    let index = index_from(schema);
    let file = file_of(src);
    let instances = InstanceIndex::from_file("main.nml", &file);
    let declaring = instances.resolve_ref(name).expect("declaring indexed");
    let block = instances.get(declaring).unwrap();
    let refs: Vec<InstanceId> = block
        .uses
        .iter()
        .map(|r| instances.resolve_ref(&r.name).expect("ref resolves"))
        .collect();
    let local = block.body.clone();
    resolve_layers(&index, &instances, declaring, root, &refs, &local, grants)
}

/// The layer stack of a single-file instance index, by name — for
/// tests that drive the merger directly.
fn layers_of<'i>(instances: &'i InstanceIndex, names: &[&str]) -> Vec<(InstanceId<'i>, Body)> {
    names
        .iter()
        .map(|n| {
            let id = instances.resolve_ref(n).unwrap();
            (id, instances.get(id).unwrap().body.clone())
        })
        .collect()
}

/// The first NAMED item's body of a list body.
fn first_named_item_body(list: &Body) -> &Body {
    list.entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::ListItem(ListItem {
                kind: ListItemKind::Named { body, .. },
                ..
            }) => Some(body),
            _ => None,
        })
        .expect("a named item")
}

fn codes_of(diags: &[Diagnostic]) -> Vec<Code> {
    diags.iter().filter_map(|d| d.code).collect()
}

fn scalar<'r>(body: &'r Body, name: &str) -> Option<&'r Value> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::Property(p) if p.name.name == name => Some(&p.value.value),
        _ => None,
    })
}

fn list_names(body: &Body, field: &str) -> Vec<String> {
    body.entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == field => Some(
                nb.body
                    .entries
                    .iter()
                    .filter_map(|e| match &e.kind {
                        BodyEntryKind::ListItem(item) => Some(match &item.kind {
                            ListItemKind::Named { name, .. } => name.name.clone(),
                            ListItemKind::Shorthand { value, .. } => {
                                format!("{:?}", value.value)
                            }
                            ListItemKind::Reference(id) => id.name.clone(),
                            ListItemKind::Role(r) => format!("@{r}"),
                        }),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

const FLOW_SCHEMA: &str = "\
model step:
    name string+
    action string #sealed
    locator string

model flow:
    entrypoint string #sealed
    steps []step #identity
";

const SUMMARY: &str = "\
flow memberLookup:
    entrypoint = \"search\"
    steps:
        - search:
            action = \"type\"
            locator = \"#q\"
        - submitSearch:
            action = \"click\"
            locator = \"#submit\"

flow cuXyz uses memberLookup:
    steps:
        - submitSearch:
            locator = \"#search-button\"
";

// ── the RFC Summary example, end to end ──────────────────────────────

const APPEND_SCHEMA: &str = "\
model step:
    name string+
    action string

model flow:
    steps []step #append
";

const PAIR_SCHEMA: &str = "\
model step:
    name string+
    action string #sealed
    locator string

model flow:
    steps []step #identity #append
";

const DENY_SCHEMA: &str = "\
model policy:
    denyHosts []string #append
    label string
";

const LIN_SCHEMA: &str = "\
model thing:
    v string
";

struct TestGrants {
    lookup: fn(&str) -> GrantLookup<'static>,
}

impl LayerGrantProvider for TestGrants {
    fn grant_for(&self, source_path: &str) -> GrantLookup<'_> {
        (self.lookup)(source_path)
    }
    fn ref_decision(&self, grant: &LayerGrant, target_path: &str) -> RefDecision {
        if let Some(i) = grant
            .deny_refs
            .iter()
            .position(|d| target_path.starts_with(d.as_str()))
        {
            return RefDecision::DenyVeto(i);
        }
        if grant
            .allow_refs
            .iter()
            .any(|a| target_path.starts_with(a.as_str()))
        {
            RefDecision::Allowed
        } else {
            RefDecision::AllowMiss
        }
    }
}

const BASE_AND_T: &str = "\
thing base:
    v = \"b\"

thing t uses base:
    v = \"t\"
";

const MODIFIER_SCHEMA: &str = "\
model policy:
    label string
    |deny []string #append
";

const ONEOF_SCHEMA: &str = "\
model gcp:
    kind string
    path string

model az:
    kind string
    azureUrl string
    azureKey string #sealed

model sns:
    kind string
    topicArn string

oneof notify by kind = \"gcp\":
    \"gcp\" -> gcp
    \"az\" -> az
    \"sns\" -> sns
";

fn nested_scalar<'r>(body: &'r Body, block: &str, name: &str) -> Option<&'r Value> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::NestedBlock(nb) if nb.name.name == block => scalar(&nb.body, name),
        _ => None,
    })
}

const NESTED_ONEOF_SCHEMA: &str = "\
model gcpAuth:
    keyPath string #sealed

model snsAuth:
    topicArn string

oneof auth by kind = \"gcp\":
    \"gcp\" -> gcpAuth
    \"sns\" -> snsAuth

model azArm:
    kind string
    cred auth

model snsArm:
    kind string
    topicArn string

oneof notify by kind = \"az\":
    \"az\" -> azArm
    \"sns\" -> snsArm
";

const STEP_FLOW_SCHEMA: &str = "\
model step:
    name string+
    action string
    tags []string #append

model flow:
    steps []step #identity
";

const ONEOF_ROOT_LIST_SCHEMA: &str = "\
model azR:
    kind string
    hosts []string #append

model snsR:
    kind string
    topicArn string

oneof relay by kind = \"az\":
    \"az\" -> azR
    \"sns\" -> snsR
";

const MODIFIER_SEAL_SCHEMA: &str = "\
model policy:
    label string
    |deny []string #sealed
";

const ITEM_ONEOF_SCHEMA: &str = "\
model spArm:
    ikind string
    secret string #sealed

model ptArm:
    ikind string
    port string

oneof istep by ikind = \"sp\":
    \"sp\" -> spArm
    \"pt\" -> ptArm

model armX:
    steps []istep #identity

model armY:
    note string

oneof svc by kind = \"x\":
    \"x\" -> armX
    \"y\" -> armY
";

const NESTED_UNDER_PARENT_SCHEMA: &str = "\
model subA:
    sk string
    w string

model subX:
    sk string
    v string #sealed

oneof subo by sk = \"sa\":
    \"sa\" -> subA
    \"sx\" -> subX

model armA:
    pk string
    note string

model armB:
    pk string
    sub subo

oneof po by pk = \"a\":
    \"a\" -> armA
    \"b\" -> armB
";

// ── round-9 review pins ──────────────────────────────────────────────

const UNION_SCHEMA: &str = "\
model ua:
    x string
    secret string #sealed

model ub:
    y string

model holder:
    slot (ua | ub)
    label string
";

fn slot_annotation(body: &Body) -> Option<String> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::NestedBlock(nb) if nb.name.name == "slot" => {
            nb.body.type_annotation.as_ref().map(|i| i.name.clone())
        }
        _ => None,
    })
}

const ARM_SET_SCHEMA: &str = "\
model handler:
    note string
    token string #sealed

model router:
    route (string -> handler)
    label string
";

fn sub_block<'b>(body: &'b Body, name: &str) -> Option<&'b Body> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::NestedBlock(nb) if nb.name.name == name => Some(&nb.body),
        _ => None,
    })
}

const NESTED_UNION_SCHEMA: &str = "\
model leafA:
    p string
    s string #sealed

model leafB:
    q string

model mid:
    inner (leafA | leafB)

model other:
    z string

model holder2:
    slot (mid | other)
";

const ONEOF_WITH_UNION_SCHEMA: &str = "\
model leafA:
    p string
    s string #sealed

model leafB:
    q string

model armX:
    kind string
    inner (leafA | leafB)

model armY:
    kind string
    w string

oneof pay by kind:
    \"x\" -> armX
    \"y\" -> armY
";

const ONEOF_ARM_SET_SCHEMA: &str = "\
model card:
    kind string
    pan string #sealed

model cash:
    kind string
    amount string

oneof pay2 by kind:
    \"card\" -> card
    \"cash\" -> cash

model router2:
    route (string -> pay2)
";

const LIST_UNION_SCHEMA: &str = "\
model ua:
    x string
    secret string #sealed

model ub:
    y string

model holder3:
    xs [](ua | ub) #identity
";

const SCALAR_UNION_SCHEMA: &str = "\
model ua:
    x string

model holder4:
    slot (ua | string)
";

const LIST_VARIANT_SCHEMA: &str = "\
model ua:
    x string

model ub:
    kind string
    secret string #sealed

model holder7:
    slot (ua | []ub)
";

const UNION_ELEMENT_SCHEMA: &str = "\
model leafA:
    p string
    s string #sealed

model leafB:
    q string

model bigv:
    ys [](leafA | leafB)

model other:
    z string

model holder8:
    slot (bigv | other)
";

const LIST_VARIANT_SHARED_SCHEMA: &str = "\
model ua:
    x string

model ub:
    name string+ #sealed
    secret string? #sealed
    note string?

model holder13:
    slot (ua | []ub)
";

fn slot_annotation_named(body: &Body, name: &str) -> Option<String> {
    sub_block(body, name).and_then(|b| b.type_annotation.as_ref().map(|i| i.name.clone()))
}

const MODIFIER_ITEM_SEAL_SCHEMA: &str = "\
model mstep:
    name string+
    act string #sealed

model wsa:
    skind string
    steps []mstep #identity

model wsb:
    skind string
    other string

oneof wcfg by skind = \"a\":
    \"a\" -> wsa
    \"b\" -> wsb

model box:
    cfg wcfg
";

/// Find a composed list's items: `field` is the list-typed entry.
fn composed_items<'b>(body: &'b Body, field: &str) -> Vec<&'b ListItem> {
    body.entries
        .iter()
        .find_map(|e| match &e.kind {
            BodyEntryKind::NestedBlock(nb) if nb.name.name == field => Some(&nb.body),
            _ => None,
        })
        .map(|b| {
            b.entries
                .iter()
                .filter_map(|e| match &e.kind {
                    BodyEntryKind::ListItem(item) => Some(item),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A property's rendered value inside an item body, if present.
fn item_prop(body: &Body, name: &str) -> Option<String> {
    body.entries.iter().find_map(|e| match &e.kind {
        BodyEntryKind::Property(pr) if pr.name.name == name => {
            Some(format!("{:?}", pr.value.value))
        }
        _ => None,
    })
}

const K_SCHEMA: &str = "\
model stepA:
    kind string
    name string+
    xs []string

model stepB:
    kind string
    id string

oneof step by kind = \"a\":
    \"a\" -> stepA
    \"b\" -> stepB

model appK:
    steps []step #identity
";

fn perf_fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/layers/perf")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn perf_compose(name: &str, bound: std::time::Duration) {
    let src = perf_fixture(name);
    let (file, parse_diags) = crate::cst::parse_to_ast_all(&src);
    assert!(
        parse_diags.is_empty(),
        "{name}: parse must be clean: {parse_diags:?}"
    );
    let index = index_from(&src);
    let started = std::time::Instant::now();
    let composed = compose_file(&index, name, &file, &OpenContext);
    let elapsed = started.elapsed();
    assert!(
        composed
            .diagnostics
            .iter()
            .all(|d| d.severity != crate::diagnostic::Severity::Error),
        "{name}: compose must be clean: {:?}",
        composed.diagnostics
    );
    assert!(
        elapsed < bound,
        "{name}: composed in {elapsed:?}, over the {bound:?} tripwire \
             — a complexity trap regressed (RFC 0025 §8)"
    );
}
