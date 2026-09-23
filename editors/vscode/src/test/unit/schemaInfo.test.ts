import * as assert from "node:assert";
import {
  formatHashShort,
  hash8,
  parseSchemaInfo,
  parseSchemaInfoResult,
} from "../../contracts/schemaInfo";
import { buildStatusPresentation, describeLayers, describeRoot } from "../../statusPresentation";

/** Fixture aligned with tutorial `09-ship-schemas-to-your-users.md` demo hash. */
const BOUND_PINNED_STORE = {
  bound: true,
  package: "demo",
  version: "0.1.0",
  contentHash:
    "blake3:de541008f76adef7f8494994888e749680f46ea7671b0ae6766ba86e1b9e30f2",
  binding: "demo",
  source: "store current",
  step: "pinned",
  root: ".",
  shadowsStore: false,
  actions: [],
  notes: [],
};

suite("contracts/schemaInfo", () => {
  test("hash8 strips blake3 prefix and takes eight characters", () => {
    assert.strictEqual(
      hash8("blake3:de541008f76adef7f8494994888e749680f46ea7671b0ae6766ba86e1b9e30f2"),
      "de541008"
    );
    assert.strictEqual(
      hash8("de541008f76adef7f8494994888e749680f46ea7671b0ae6766ba86e1b9e30f2"),
      "de541008"
    );
  });

  test("formatHashShort matches server hover convention", () => {
    assert.strictEqual(
      formatHashShort(BOUND_PINNED_STORE.contentHash),
      "blake3:de541008"
    );
  });

  test("parseSchemaInfo accepts bound pinned store fixture", () => {
    const info = parseSchemaInfo(BOUND_PINNED_STORE);
    assert.ok(info?.bound);
    if (!info?.bound) return;
    assert.strictEqual(info.package, "demo");
    assert.strictEqual(info.step, "pinned");
    assert.strictEqual(info.notes.length, 0);
  });

  test("parseSchemaInfo accepts unbound with notes", () => {
    const info = parseSchemaInfo({
      bound: false,
      notes: [{ message: "no pin", severity: "info" }],
    });
    assert.ok(info && !info.bound);
    assert.strictEqual(info.notes.length, 1);
  });

  test("parseSchemaInfo rejects malformed wire", () => {
    assert.strictEqual(parseSchemaInfo(null), undefined);
    assert.strictEqual(parseSchemaInfo({ bound: true }), undefined);
  });

  test("parseSchemaInfoResult surfaces server error wire", () => {
    const result = parseSchemaInfoResult({ error: "missing or invalid 'uri'" });
    assert.deepStrictEqual(result, {
      kind: "error",
      message: "missing or invalid 'uri'",
    });
  });

  test("parseSchemaInfo keeps the root facts it knows and drops the rest", () => {
    const info = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      rootOrigin: "derivedVcsFence",
      rootFence: "file",
      rootShadowed: "../../demo.package.nml",
    });
    assert.ok(info?.bound);
    assert.strictEqual(info?.rootOrigin, "derivedVcsFence");
    assert.strictEqual(info?.rootFence, "file");
    assert.strictEqual(info?.rootShadowed, "../../demo.package.nml");
    const unknown = parseSchemaInfo({
      bound: false,
      notes: [],
      rootOrigin: "teleported",
      rootFence: null,
      rootShadowed: null,
    });
    assert.ok(unknown && !unknown.bound);
    assert.strictEqual(unknown?.rootOrigin, undefined);
    assert.strictEqual(unknown?.rootFence, undefined);
    assert.strictEqual(unknown?.rootShadowed, undefined);
    assert.strictEqual(parseSchemaInfo(BOUND_PINNED_STORE)?.rootOrigin, undefined);
  });

  test("parseSchemaInfoResult maps bound payload to ok", () => {
    const result = parseSchemaInfoResult(BOUND_PINNED_STORE);
    assert.strictEqual(result.kind, "ok");
    if (result.kind !== "ok") return;
    assert.strictEqual(result.info.bound && result.info.package, "demo");
  });
});

suite("statusPresentation/describeRoot", () => {
  test("a workspace folder and a factless payload disclose nothing", () => {
    assert.strictEqual(describeRoot({ rootOrigin: "editor" }), undefined);
    assert.strictEqual(describeRoot({}), undefined);
  });

  test("a derived root says the fence, the shadow and the way out", () => {
    assert.strictEqual(
      describeRoot({ rootOrigin: "derivedVcsFence", rootFence: "dir" }),
      "derived within the .git fence — open the workspace folder to choose the root"
    );
    assert.strictEqual(
      describeRoot({
        rootOrigin: "derivedVcsFence",
        rootFence: "file",
        rootShadowed: "../../demo.package.nml",
      }),
      "derived within a .git FILE fence — a linked worktree's, a submodule's or a planted entry; " +
        "shadowed by `../../demo.package.nml` above it — open the workspace folder to choose the root"
    );
    // The CLI's `note: workspace root …` line keeps `universe` for the
    // open/closed world and says "workspace root" here; the tooltip says
    // the same, with the editor's remedy in place of `--root`.
    const noVcs = describeRoot({ rootOrigin: "derivedTargetDir" }) ?? "";
    assert.match(noVcs, /no \.git fence found, so the file's own directory is the workspace root/);
    assert.match(noVcs, /open the workspace folder to choose one$/);
    assert.doesNotMatch(noVcs, /universe/);
  });
});

suite("statusPresentation/buildStatusPresentation", () => {
  test("a derived root is said beside the root, bound and unbound", () => {
    const bound = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      rootOrigin: "derivedVcsFence",
      rootFence: "file",
    });
    assert.ok(bound);
    const p = buildStatusPresentation("running", "srv", { kind: "ok", info: bound! }, true);
    assert.match(p.tooltip, /Root: \. \(derived within a \.git FILE fence/);
    const folder = buildStatusPresentation(
      "running",
      "srv",
      { kind: "ok", info: parseSchemaInfo(BOUND_PINNED_STORE)! },
      true
    );
    assert.match(folder.tooltip, /Root: \.\n/);
    assert.doesNotMatch(folder.tooltip, /derived/);
    const unbound = buildStatusPresentation(
      "running",
      "srv",
      {
        kind: "ok",
        info: { bound: false, notes: [], rootOrigin: "derivedTargetDir" },
      },
      true
    );
    assert.match(unbound.tooltip, /Root: derived: no \.git fence found/);
  });

  test("bound tooltip includes content hash", () => {
    const info = parseSchemaInfo(BOUND_PINNED_STORE);
    assert.ok(info);
    const p = buildStatusPresentation(
      "running",
      "neutral nml-lsp (wasm)",
      { kind: "ok", info: info! },
      true
    );
    assert.match(p.tooltip, /blake3:de541008/);
    assert.match(
      p.tooltip,
      /de541008f76adef7f8494994888e749680f46ea7671b0ae6766ba86e1b9e30f2/
    );
    assert.strictEqual(p.text, "$(check) nml: demo 0.1.0");
  });

  test("warning notes elevate status background on unbound", () => {
    const p = buildStatusPresentation("running", "srv", {
      kind: "ok",
      info: {
        bound: false,
        notes: [{ message: "degraded", severity: "warning" }],
      },
    }, true);
    assert.strictEqual(p.backgroundColorId, "statusBarItem.warningBackground");
    assert.match(p.tooltip, /degraded/);
  });

  test("bound warning notes elevate status background", () => {
    const info = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      notes: [{ message: "store stale", severity: "warning" }],
    });
    assert.ok(info);
    const p = buildStatusPresentation(
      "running",
      "srv",
      { kind: "ok", info: info! },
      true
    );
    assert.strictEqual(p.backgroundColorId, "statusBarItem.warningBackground");
    assert.match(p.tooltip, /store stale/);
  });

  test("shadowsStore note appears in bound tooltip", () => {
    const info = parseSchemaInfo({ ...BOUND_PINNED_STORE, shadowsStore: true });
    assert.ok(info);
    const p = buildStatusPresentation(
      "running",
      "srv",
      { kind: "ok", info: info! },
      true
    );
    assert.match(p.tooltip, /shadows the store copy/);
  });

  test("pin action hint when schemaInfo.actions includes pin", () => {
    const info = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      step: "auto-associated",
      actions: ["pin", "disableAutoAssociation"],
    });
    const p = buildStatusPresentation(
      "running",
      "srv",
      { kind: "ok", info: info! },
      true
    );
    assert.match(p.tooltip, /Pin available via lightbulb/);
  });

  test("schema lookup error wire elevates warning", () => {
    const p = buildStatusPresentation(
      "running",
      "srv",
      { kind: "error", message: "missing or invalid 'uri'" },
      true
    );
    assert.strictEqual(p.backgroundColorId, "statusBarItem.warningBackground");
    assert.match(p.tooltip, /missing or invalid/);
  });

  test("hides when no active nml editor", () => {
    const p = buildStatusPresentation(
      "running",
      "srv",
      { kind: "skipped" },
      false
    );
    assert.strictEqual(p.hidden, true);
  });

  test("lifecycle starting shows spinner", () => {
    const p = buildStatusPresentation("starting", "wasm", { kind: "skipped" }, true);
    assert.strictEqual(p.text, "$(sync~spin) nml: starting…");
  });

  test("lifecycle failed shows error state", () => {
    const p = buildStatusPresentation("failed", "bad-path", { kind: "skipped" }, true);
    assert.strictEqual(p.text, "$(error) nml: server failed");
    assert.match(p.tooltip, /Show Language Server Log/);
  });

  test("lifecycle absent shows no server, and how to get one", () => {
    const p = buildStatusPresentation("absent", "", { kind: "skipped" }, true);
    assert.strictEqual(p.text, "$(circle-slash) nml: no server");
    // Every unhappy state carries a next step: this one is reached between
    // one server being retired and the next one starting, and it is where
    // the bar stays if nothing replaces it.
    assert.match(p.tooltip, /NML: Restart Language Server/);
    assert.match(p.tooltip, /NML: Show Language Server Log/);
  });

  test("every unhappy lifecycle state names a command the operator can run", () => {
    // The set, not the case: a state added without a way out fails here.
    for (const lifecycle of ["absent", "failed", "disconnected"] as const) {
      const p = buildStatusPresentation(lifecycle, "some server", { kind: "skipped" }, true);
      assert.match(
        p.tooltip,
        /NML: (Restart Language Server|Show Language Server Log)/,
        `the ${lifecycle} tooltip offers the operator nothing to do`
      );
    }
  });

  test("lifecycle disconnected shows reconnect guidance", () => {
    const p = buildStatusPresentation(
      "disconnected",
      "neutral nml-lsp (wasm)",
      { kind: "skipped" },
      true
    );
    assert.strictEqual(p.text, "$(debug-disconnect) nml: disconnected");
    assert.match(p.tooltip, /Restart Language Server/);
  });
});

suite("statusPresentation/refusals and shadows", () => {
  const REFUSAL =
    "2 manifests claim this file: demo.package.nml (shared, files[0] = \"shared/**/*.flow.nml\"), " +
    "other.package.nml (sharedToo, files[0] = \"shared/**/*.flow.nml\") — an ambiguously-claimed " +
    "file is denied: it validates under no binding and nothing runs against it; remove or narrow one claim";

  test("parseSchemaInfo keeps an error note, and every note beside it", () => {
    const info = parseSchemaInfo({
      bound: false,
      notes: [
        { message: REFUSAL, severity: "error" },
        { message: "aside", severity: "info" },
      ],
    });
    assert.ok(info && !info.bound);
    assert.deepStrictEqual(
      info?.notes.map((n) => n.severity),
      ["error", "info"]
    );
  });

  test("the two UNBOUND states get opposite remedies, from the universe word", () => {
    const of = (universe?: "open" | "closed") =>
      buildStatusPresentation(
        "running",
        "srv",
        { kind: "ok", info: parseSchemaInfo({ bound: false, notes: [], universe })! },
        true
      ).tooltip;

    // CLOSED: a manifest already exists. Committing another is not the
    // fix — the file has to enter a `files` glob. The bar used to give
    // the open remedy here, which is why this test exists.
    const closed = of("closed");
    assert.match(closed, /no glob claims this one/);
    assert.match(closed, /Add this file to the `files` glob/);
    // The wire carries only `open`/`closed`, never a manifest COUNT, so
    // the sentence must not assert that there is exactly one manifest.
    assert.doesNotMatch(closed, /none of its `files` globs/);
    assert.doesNotMatch(closed, /Commit a <name>\.package\.nml/);

    // OPEN: no manifest within the fence, so committing one IS the fix.
    const open = of("open");
    assert.match(open, /No package manifest was found in this workspace root/);
    assert.match(open, /Commit a <name>\.package\.nml/);
    assert.doesNotMatch(open, /`files` glob/);

    // An OLDER SERVER sends no `universe`: say only what is known —
    // never guess a remedy that may be the wrong one.
    const unknown = of(undefined);
    assert.match(unknown, /Commit a <name>\.package\.nml/);
    assert.doesNotMatch(unknown, /workspace root (is|has)/);
  });

  test("a refused document says so, the note as the reason, never the no-schema advice", () => {
    const p = buildStatusPresentation(
      "running",
      "srv",
      { kind: "ok", info: { bound: false, notes: [{ message: REFUSAL, severity: "error" }] } },
      true
    );
    assert.strictEqual(p.text, "$(error) nml: not validated");
    assert.ok(p.tooltip.startsWith(`Not validated: ${REFUSAL}`), p.tooltip);
    assert.doesNotMatch(p.tooltip, /No schema package governs|Commit a <name>/);
    assert.strictEqual(p.backgroundColorId, "statusBarItem.errorBackground");
  });

  test("a bound document with an error note keeps its package and turns error-coloured", () => {
    const info = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      notes: [{ message: "project config failed to load: too large", severity: "error" }],
    });
    assert.ok(info);
    const p = buildStatusPresentation("running", "srv", { kind: "ok", info: info! }, true);
    assert.strictEqual(p.text, "$(check) nml: demo 0.1.0");
    assert.match(p.tooltip, /project config failed to load/);
    assert.strictEqual(p.backgroundColorId, "statusBarItem.errorBackground");
  });

  test("a shadowed derived root colours the item as a warning, bound and unbound", () => {
    const shadowed = {
      rootOrigin: "derivedVcsFence",
      rootFence: "dir",
      rootShadowed: "../../demo.package.nml",
    } as const;
    const unbound = buildStatusPresentation(
      "running",
      "srv",
      { kind: "ok", info: { bound: false, notes: [], ...shadowed } },
      true
    );
    assert.strictEqual(unbound.backgroundColorId, "statusBarItem.warningBackground");
    const bound = parseSchemaInfo({ ...BOUND_PINNED_STORE, ...shadowed });
    assert.ok(bound);
    const p = buildStatusPresentation("running", "srv", { kind: "ok", info: bound! }, true);
    assert.strictEqual(p.backgroundColorId, "statusBarItem.warningBackground");
    const plain = parseSchemaInfo(BOUND_PINNED_STORE);
    assert.ok(plain);
    const q = buildStatusPresentation("running", "srv", { kind: "ok", info: plain! }, true);
    assert.strictEqual(q.backgroundColorId, undefined);
  });
});

suite("contracts/schemaInfo layers", () => {
  test("parseSchemaInfo keeps a well-formed layers object and drops a malformed one", () => {
    const granted = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      layers: { granted: true, allowRefs: ["tenants/**"], denyRefs: [], maxStackDepth: null },
    });
    assert.ok(granted?.bound);
    assert.deepStrictEqual(granted?.layers, {
      granted: true,
      allowRefs: ["tenants/**"],
      denyRefs: [],
      maxStackDepth: null,
    });
    const denied = parseSchemaInfo({ ...BOUND_PINNED_STORE, layers: { granted: false } });
    assert.deepStrictEqual(denied?.layers, { granted: false });
    // An older server: no field, no grant — the payload still parses.
    assert.strictEqual(parseSchemaInfo(BOUND_PINNED_STORE)?.layers, undefined);
    // A malformed grant is absent, never a rejected payload.
    const malformed = parseSchemaInfo({ ...BOUND_PINNED_STORE, layers: { granted: "yes" } });
    assert.ok(malformed?.bound);
    assert.strictEqual(malformed?.layers, undefined);
    const badRule = parseSchemaInfo({
      ...BOUND_PINNED_STORE,
      layers: { granted: true, allowRefs: [1] },
    });
    assert.strictEqual(badRule?.layers, undefined);
    const unbound = parseSchemaInfo({ bound: false, notes: [], layers: { granted: false } });
    assert.ok(unbound && !unbound.bound);
    assert.deepStrictEqual(unbound?.layers, { granted: false });
  });

  test("describeLayers speaks in nml binding's words", () => {
    assert.strictEqual(describeLayers(undefined), undefined);
    assert.strictEqual(describeLayers({ granted: false }), "none — composition denied (NML2064)");
    assert.strictEqual(
      describeLayers({ granted: true, allowRefs: ["tenants/**"], denyRefs: ["tenants/cu/**"], maxStackDepth: 4 }),
      'granted — allowRefs[0] = "tenants/**", denyRefs[0] = "tenants/cu/**", maxStackDepth = 4'
    );
    assert.strictEqual(describeLayers({ granted: true, allowRefs: [], maxStackDepth: null }), "granted");
  });

  test("every tooltip says what clicking the item does, exactly once", () => {
    // `statusBar.ts` makes the item run `nml.restartServer`. Before this,
    // the action appeared in the walkthrough and nowhere the operator
    // reads at the moment they are looking at the bar.
    const info = parseSchemaInfo(BOUND_PINNED_STORE)!;
    const states: [string, string][] = [];
    for (const lifecycle of ["starting", "running", "failed", "disconnected", "absent"] as const) {
      for (const lookup of [
        { kind: "ok", info } as const,
        { kind: "error", message: "refused" } as const,
        { kind: "unavailable" } as const,
      ]) {
        const p = buildStatusPresentation(lifecycle, "srv", lookup, true);
        states.push([`${lifecycle}/${lookup.kind}`, p.tooltip]);
      }
    }
    for (const [name, tooltip] of states) {
      const named = /NML: Restart Language Server/.test(tooltip);
      const clicked = /Click here to restart the NML language server\./.test(tooltip);
      assert.ok(
        named !== clicked,
        `${name}: the restart is named ${named && clicked ? "twice" : "nowhere"}: ${tooltip}`
      );
    }
    // A hidden item has no tooltip to carry it.
    assert.strictEqual(
      buildStatusPresentation("running", "srv", { kind: "ok", info }, false).tooltip,
      ""
    );
  });

  test("the status tooltip names the schema's origin in the kernel's word", () => {
    // `ClaimClass::label` (nml-validate) calls this string "the human-facing
    // source label — the ONE vocabulary the CLI's `binding` line and the
    // editor's `nml/schemaInfo` `source` share". The tooltip has to spell it
    // the same way; `Channel:` was a fourth word for it, and "channel" in
    // this extension already means an output channel the same tooltips send
    // the operator to.
    const info = parseSchemaInfo(BOUND_PINNED_STORE);
    assert.ok(info);
    const p = buildStatusPresentation("running", "srv", { kind: "ok", info: info! }, true);
    const lines = p.tooltip.split("\n");
    assert.strictEqual(lines[2], `Source: ${BOUND_PINNED_STORE.source}`);
    assert.doesNotMatch(p.tooltip, /Channel:/);
  });

  test("the status tooltip carries the grant line after the root", () => {
    const info = parseSchemaInfo({ ...BOUND_PINNED_STORE, layers: { granted: false } });
    assert.ok(info);
    const p = buildStatusPresentation("running", "srv", { kind: "ok", info: info! }, true);
    const lines = p.tooltip.split("\n");
    assert.strictEqual(lines[4], "Root: .");
    assert.strictEqual(lines[5], "Layers: none — composition denied (NML2064)");
    assert.strictEqual(lines[6], "Server: srv");
    const plain = parseSchemaInfo(BOUND_PINNED_STORE);
    assert.ok(plain);
    const q = buildStatusPresentation("running", "srv", { kind: "ok", info: plain! }, true);
    assert.ok(!q.tooltip.includes("Layers:"));
  });
});
