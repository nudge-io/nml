# Build and publish schema packages

A schema package is your tool's schemas + manifest (bindings, modifiers,
directive vocabulary) as one content-addressed unit. **Identity is the
content hash** — versions are human labels — so "which schema is my editor
using" always has an exact answer.

```rust source=docs/guides/examples/cookbook/examples/packages_and_store.rs
    // Content-addressed identity: same bytes, same hash, everywhere.
    let hash = package.content_hash();
    println!("skylight {} @ {}", package.manifest.version, hash8(&hash));
```

```rust source=docs/guides/examples/cookbook/examples/packages_and_store.rs
    store.publish(&package)?;
    // Idempotent: re-publishing identical content is a no-op, not an error.
    store.publish(&package)?;

    // What a consumer does: read the current slot by package name.
    let current = store.read_current("skylight")?;
    assert_eq!(current.package.content_hash(), hash);
```

Full program: [`packages_and_store.rs`](examples/cookbook/examples/packages_and_store.rs)
— `cargo run -p nml-cookbook --example packages_and_store`.

Construction: `SchemaPackage::from_dir` (a directory holding
`<name>.package.nml` plus sources — the committed-file channel) or
`from_parts` (manifest text plus a source resolver — the embedded channel,
usually `include_str!`). Publishing: `Store::user()` is the per-user store
your users' editors read (your tool's `schema sync` publishes there);
`Store::at(dir)` for tests and hermetic setups.

**How it reaches users:** commit a `<name>.package.nml` in their project
(zero-config editor validation), publish to the store, or [embed the whole
server](embed-the-lsp.md) so the schema ships inside your binary and can
never be stale. The file stem *is* the declared `name`: a workspace
manifest whose stem differs from its `name` (`evil.package.nml` declaring
`name = "demo"`) is a load error naming both — it never becomes a second
`demo` that ambiguates yours.

## How a file finds its binding (workspace resolution)

**Closed-workspace quick start.** Put a manifest at the root of the tree
you govern, claim the files by glob, and pin the root in CI:

```nml fragment
// demo.package.nml — beside the schemas it declares
package demo:
    version = "0.1.0"
    formatVersion = 1

[]schema schemas:
    - core:
        file = "core.model.nml"

[]validator validators:
    - tenantFlows:
        files:
            - "tenants/**/*.flow.nml"
        schemas:
            - core
        strict = true
```

```bash
nml binding --root . tenants/cu/member-lookup.flow.nml   # who governs it, and why
nml check   --root . tenants/                            # every .nml file under tenants/, under its binding
```

(A directory target is walked — links and dot-directories skipped. A
shell glob `tenants/**/*.flow.nml` is expanded by the shell, and bash
without `globstar` reads `**` as `*`: one level, silently.)

A manifest names each entry once — `files:` beside `files = […]` is one
entry twice (NML2093, refused where the manifest is parsed, before a
glob is read) — and each `[]schema` source and `[]validator` binding
once; a second `package` block or `[]` array under the same name is
NML1000, and a second `[]validator` array under another name is refused
too, never a silent replacement.

The manifest's full syntax (`[]schema`, `[]validator`, `rootMarkers`,
the store) is the subject of [tutorial 9 — Ship schemas to your
users](../tutorial/09-ship-schemas-to-your-users.md); the `layers:`
grant block a binding carries is [documented below](#the-layers-grant).
Only `check`, `validate`, `fix` and `binding` resolve through the
workspace and are safe to run over a tree an untrusted author commits
to; `parse` and `fmt` read and write the path as given.

`nml check`, `nml validate`, `nml fix` and `nml binding` resolve a file
through one core (`nml_validate::workspace` — the core the editor runs
too, so the CLI and the editor give one answer: the same binding, the
same denials, the same codes) in four steps.

**1. The root is fixed once per invocation.** `--root <dir>` names the
universe explicitly (CI should always pass it). Without it the root is
*derived*: the outermost directory holding a `<name>.package.nml` or
`nml-project.nml` between the file's directory and the nearest `.git`
entry (a file or a directory — linked worktrees and submodules carry a
`.git` file). With no `.git` anywhere, the file's own directory is the
universe. Under a `.git` fence derivation never follows a symlink an
author could have committed: below the fence the walk stops before the
first symlink component, whether or not the link points anywhere. With
no fence at all the chain *above* the file's own directory is the
operator's and is followed like any prefix (a link there is resolved,
and whether it resolves is observable — nothing above a no-VCS universe
can govern the file; CI passes `--root`), while the file's own
directory being a symlink is refused with a `--root` hint. The
derivation is fail-closed in three more ways: once the fence is fixed,
the bounded walk continues ABOVE it — a manifest or `nml-project.nml`
there refuses the derivation outright when the fence is NO directory
(``` the root marker `<marker>` sits above the fence at `<dir>/.git`, and
that fence is no directory … — pass --root ```: a submodule's or linked
worktree's `.git` file, or a planted entry, below the operator's
manifest can never shrink its universe silently), while above a `.git`
DIRECTORY — a nested checkout, a stray manifest in a parent, shapes no
commit produces — the fence holds and the marker is reported as
shadowing the derived root, as is another `.git` there with no marker
between (`root.shadowed` names the entry on the `--json` rows and the
`binding` line, a one-line `note:` on stderr in human mode — as is a
`.git` fence that is not a directory, `root.fence`); a shadow check the
64-directory bound cuts short refuses rather than deriving unchecked;
and a walk that meets no `.git` within 64 directories derives nothing
(exit 2, `pass --root`) rather than an open universe at the target's
own directory. The root is never re-derived per file — a tenant's own
`nml-project.nml` cannot move it.

**2. Every input under the root is discovered, then settled live or
inert — by ancestry, never by order.** Manifests, project configs and
root markers are read in depth order; an input is *inert* when it sits
inside content that a **live manifest in a strictly shallower directory
claims** (R3′ — a binding whose `files` glob can match a path under the
input's directory claims that directory). An inert input is content,
not configuration: its pins, `autoAssociate`, bindings and marker names
are ignored, it is never loaded, and `nml check` reports it as
NML2080. Anchors are manifest-derived (R4): a manifest's globs are
relative to the nearest live project config or own-marker directory at
or above the manifest's directory, else that directory; a marker
committed deeper than the manifest never moves the anchor. A manifest
governs only its own subtree.

**3. The file's key is minted.** Keys are workspace-relative, `/`-only,
byte-exact spellings — the vocabulary every diagnostic `source`, every
`files` glob and every layer grant speaks. In a *closed* universe (one
that holds a workspace manifest) the walk is `lstat`-first: a symlink
component — an ancestor, or the file itself, dangling or not — is
rejected as NML2083 **before its target is resolved**, so the message
is byte-identical whether the target exists (a planted link is not an
existence oracle). `..` resolves against the real prefix, and a path
that leaves the root is rejected as authored, never as resolved. In an
open universe links are followed and their targets must still lie
under the root.

Two edges here are fail-closed by design rather than resolved. A `..`
that leaves the root from strictly inside it and re-enters
(`<root>/tenants/../../<root>/x.nml`) is rejected as outside the root —
spell the path from the root instead; only a `..` *at* the root
(`nml check ../<root>/x.nml` from the root's own directory) is folded
through the root's canonical parent. And an operator-side link — in
`--root`, or in the prefix above the root — resolves to the
filesystem's real path, so a link on *its* target path is the
operator's responsibility (`nml binding` prints the resolved root and
key); once the walk is inside the root no symlink is resolved. `nml
fix` classifies its path arguments by the same rule: only a real
directory the spelling reaches through no link at or below the root
is expanded — the classification locates the root by the operator's
realpath (a link above the root, or a `..` through one, is followed
exactly as `check` follows it) and `lstat`s prefix by prefix below
it, halting at the first symlink — so a link, a spelling that
continues past one (`lib/`, `lib/.`, `lib/..` and `lib/sub` are
judged like `lib`) or an absent argument is judged by this walk,
never by an early stat, and a directory outside the root is the
invocation's mistake (exit 2, nothing runs); inside a walked directory
only regular files are collected (a FIFO is skipped like a link).

**4. The governing binding is chosen** — pins from the nearest live
config first, then unambiguous auto-association, first glob match in
declaration order. Two live manifests claiming one file is *ambiguous*:
an error (NML2087) naming both claimants; the file validates under no binding —
`check`, `validate`, `fix` and `binding` all exit 1 (`fix` writes nothing:
`1 path(s) could not be fixed`) — never a nearest-wins shadow, never a
parse-only pass; a workspace manifest shadows a same-named store or
builtin package, and store/builtin packages bind files but never close
a universe. `nml binding <file>` prints all of
it: the key, the root and how it was fixed, the binding with the glob
index that matched, the effective `layers:` grant, and every inert
input on the file's path — plus, in an *open* universe only, an `info`
row when the path was resolved through a symlink (`resolved through a
symlink at component N of the root-relative path …`; the key names the
target — a closed binding would have rejected the path with NML2083
instead). Exit codes: 0 bound, 1 unbound or ambiguous, 2 error.

```text
$ nml binding tenants/cu/member-lookup.flow.nml
file      tenants/cu/member-lookup.flow.nml
root      /srv/app  (derived within the .git fence — pass --root to pin)
binding   tenantFlows   demo blake3:be06ba4b, workspace manifest (demo.package.nml)
anchor    .   matched files[0] = "tenants/**/*.flow.nml"   (auto-associated)
layers    none — composition denied (NML2064)
notes     tenants/cu/nml-project.nml: warning[NML2080]: project config … is inert …
```

### The `layers:` grant

Composition (`uses`, RFC 0019) is an import capability the *binding*
grants; a binding without a `layers:` block denies it (NML2064), and no
content file can grant itself one. The block is ordinary schema on the
builtin meta-package:

```nml fragment
[]validator validators:
    - vendorFlows:
        files:
            - "vendor/**/*.flow.nml"
        schemas:
            - core
        layers:
            allowRefs:
                - "vendor/**"          // globs over the referenced instance's file
            denyRefs:
                - "vendor/vetoed/**"   // deny wins over allow (NML2065, by index)
            maxStackDepth = 4          // distinct instances per linearized stack (NML2066)
```

| field | meaning |
|---|---|
| `allowRefs []string` | the target allowlist: globs over the referenced instance's defining file path (the same matcher as the binding's `files`); empty = deny all |
| `denyRefs []string?` | vetoes inside the allowlist; a denial names the vetoing rule by index (`denyRefs[0]`) — `nml binding` prints the same indices |
| `maxStackDepth number(min = 1, multipleOf = 1)?` | the most distinct instances one linearized stack may hold, the declaring instance included; absent = the language cap (16) alone |

There is no `allowComposition` boolean (an empty allowlist already
means deny) and no second selection rule: whichever binding governs a
file for validation governs its composition authority. The block's
shape is meta-validated like every other manifest field; its own rules
— a glob the matcher rejects, a stack cap past the language's, a veto
beside an empty allowlist — are refused at load as NML2081, at the
item. Reading the grant as it applies to a file is `nml binding
<file>`'s `layers` rows, by the indices a denial cites:

```text transcript=tests/fixtures/workspace-grant
$ nml binding --root . vendor/vetoed/v.flow.nml
file      vendor/vetoed/v.flow.nml
root      .  (--root)
binding   vendorFlows   demo blake3:186aeb08, workspace manifest (demo.package.nml)
anchor    .   matched files[0] = "vendor/**/*.flow.nml"   (auto-associated)
layers    granted
          allowRefs[0] = "vendor/**"
          denyRefs[0] = "vendor/vetoed/**"
          maxStackDepth = 4
```

A binding without the block denies composition (NML2064): the denial's
`note:` lands at the binding in the manifest, naming the key to admit,
and the `help:` beneath it is the block to paste — resolved against the
manifest, at the binding body's own indentation, naming the line it
goes after, so copy its lines as printed and paste them there (`--json`
carries the same edit in the row's `suggestions[]`; the editor offers
it as a quick-fix on the manifest). On the manifest above, `tenantFlows`
carries no block and `vendorFlows` carries the one shown — the denial,
then the pass:

```text transcript=tests/fixtures/workspace-grant
$ nml check --root . tenants/cu/member-lookup.flow.nml
tenants/cu/member-lookup.flow.nml:4:7: error[NML2064]: composition not permitted: binding 'tenantFlows' (demo.package.nml) carries no `layers:` grant — an operator change, not fixable from a content file; run `nml binding tenants/cu/member-lookup.flow.nml` to see the effective grant
demo.package.nml:10:7: note: to permit it, give this binding a `layers:` grant whose `allowRefs` admits "tenants/cu/member-lookup.flow.nml"
help: the block to add after line 15 of demo.package.nml — paste it as printed:
        layers:
            allowRefs:
                - "tenants/cu/member-lookup.flow.nml"
for more information, run: nml explain NML2064
error: 1 error(s)
$ nml check --root . vendor/base.flow.nml
vendor/base.flow.nml: ok (2 declaration(s))
```

**Bounds are errors, never silent.** Discovery charges its bounds per
*budget unit* — each directory a binding glob reaches at the start of
its last run of wildcard directory segments (`tenants/<x>` under
`tenants/**/*.flow.nml`; a catch-all root binding beside it does not
widen the unit, and a manifest a tenant commits inside claimed content
— under a unit, or in a gap of your glob — mints no unit of its own;
two globs whose units nest without naming a directory of your own,
`tenants/*/plugins/*` inside `tenants/*`, would let every tenant mint
units beneath itself — NML2092's nested form under inference, refused
as a `budgetUnits` declaration),
everything else to the root — and never descends past 64 components
(nothing deeper can be keyed). A unit that spends its
65,536-entry bound, its own 64 MiB of live manifests, project configs
and declared sources, or holds a directory that cannot be read is denied
in full: every file under it fails with an NML2089 naming the unit, the
bound and where the walk stopped, and nothing outside the unit is
affected — reduce the entries or the live inputs under the unit (`--root`
is not the remedy; rooting inside the unit leaves your manifest outside
the universe). The root unit under any of the three, or more than
1,048,576 entries or 1 GiB of live inputs in all, truncates the whole
universe: an *error* naming where the walk stopped, the universe
closed and denying every file, every verb exiting 1 — pass `--root` to
a smaller tree, or shrink the live inputs when a byte budget is what
was spent. The isolation covers content at or below a unit root: with
a literal after the wildcard (`tenants/*/flows/**`) the unit is
`tenants/<x>/flows/<y>` and `tenants/<x>` itself is the root's, so a
tenant's entries or live inputs outside `flows/` deny everyone;
`tenants/**/flows/**` and `**/tenants/*/**` make every reached
directory at the boundary depth a unit and leave the tenant directory
to the root the same way. What one tenant's flood (a directory past
65,536 entries, an unreadable directory, 64 MiB of live inputs) can
deny, by the shape of your glob:

| `files` glob | budget unit | a flood at `tenants/cu/spam` denies | a flood at `tenants/cu/flows/spam` denies |
|---|---|---|---|
| `tenants/**/*.flow.nml` | `tenants/<x>` | `tenants/cu` only | `tenants/cu` only |
| `tenants/*/flows/*.flow.nml` | `tenants/<x>` | `tenants/cu` only | `tenants/cu` only |
| `tenants/*/flows/**/*.flow.nml` | `tenants/<x>/flows/<y>` | **the whole universe** (the root's own content) | `tenants/cu/flows/spam` only |
| `**/*.flow.nml` | the root | **the whole universe** | **the whole universe** |

A manifest names its units explicitly with `budgetUnits` — anchor-relative
directory patterns under the package block (`*` within a segment, no
`**`: a unit has one depth) that REPLACE the inference for that manifest:

```nml
package demo:
    version = "0.1.0"
    formatVersion = 1
    budgetUnits = ["tenants/*"]
```

(the one line NML2092's remedy prints; a block `budgetUnits:` with
`- "tenants/*"` items is the same entry in the other spelling)

makes `tenants/<x>` the unit whatever the globs' shape, so a flood
anywhere under one tenant denies that tenant alone. The loader refuses
a declaration NARROWER than the inference (one that would leave content
a glob reaches in the root unit — `["tenants/*/flows"]` under
`tenants/**`), names the glob and the unit to declare, and a
declaration shallower than the inference (`["tenants"]`, one unit for
every tenant) is your coarser blast radius, allowed. Under inference, a
glob whose first wildcard directory run is not its last
(`tenants/*/flows/**`) is a **warning, NML2092**, once per run on the
manifest at the glob — it spells the declaration both ways; a `**`
before the boundary (`tenants/**/flows/**`) has no fixed depth to
declare, so respell it with `*` first. Executed, on a manifest whose
`tenants/*/flows/**/*.flow.nml` delegates at `tenants/<x>` and infers
its unit at `tenants/<x>/flows/<y>`:

```text transcript=tests/fixtures/workspace-gap
$ nml check --root . tenants/cu/flows/plain.flow.nml
demo.package.nml:12:15: warning[NML2092]: binding 'tenantFlows' files[0] = "tenants/*/flows/**/*.flow.nml": the inferred budget unit is `tenants/*/flows/*` — content under `tenants/*` outside it stays in the root unit, where one tenant's flood denies everyone; declare budgetUnits = ["tenants/*"] to isolate each delegated subtree, or ["tenants/*/flows/*"] to keep the inferred boundary
tenants/cu/flows/plain.flow.nml: ok (1 declaration(s))
for more information, run: nml explain NML2092
```

and the same manifest with `budgetUnits = ["tenants/*"]` declared under
its package block — the unit is `tenants/<x>`, and nothing is said:

```text transcript=tests/fixtures/workspace-gap-declared
$ nml check --root . tenants/cu/flows/plain.flow.nml
tenants/cu/flows/plain.flow.nml: ok (1 declaration(s))
```

An older `nml` refuses a
manifest carrying `budgetUnits` at load (an unknown property in a
closed vocabulary — never silently a different unit shape). `nml
binding --json` and every `summary` row list the units the walk stopped
inside under `truncatedUnits`. A manifest,
project config or declared source is read only when it is a regular file
(never through a link) and only up to a cap that names itself when
exceeded: 256 KiB for a manifest or project config, 4 MiB for a declared
schema source (a cap bounds memory, not parse time, and a live
resolution input is parsed on every invocation of every verb); a
declared source is read once and shared by every manifest that declares
it. The checked file itself is the operator's own input and is read up to
16 MiB — `nml-cli::workspace::MAX_TARGET_BYTES`, published by `nml limits`,
which names itself when exceeded (`a check target is read only up to 16 MiB`).
