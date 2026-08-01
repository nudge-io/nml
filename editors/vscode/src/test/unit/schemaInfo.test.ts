import * as assert from "node:assert";
import {
  formatHashShort,
  hash8,
  parseSchemaInfo,
  parseSchemaInfoResult,
} from "../../contracts/schemaInfo";
import { buildStatusPresentation } from "../../statusPresentation";

/** Fixture aligned with tutorial `09-ship-schemas-to-your-users.md` demo hash. */
const BOUND_PINNED_STORE = {
  bound: true,
  package: "demo",
  version: "0.1.0",
  contentHash:
    "blake3:27ff6038e8ab7bbaf17eb72d2cd526d57b1199e7272959b37d0706d5236fad4c",
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
      hash8("blake3:27ff6038e8ab7bbaf17eb72d2cd526d57b1199e7272959b37d0706d5236fad4c"),
      "27ff6038"
    );
    assert.strictEqual(
      hash8("27ff6038e8ab7bbaf17eb72d2cd526d57b1199e7272959b37d0706d5236fad4c"),
      "27ff6038"
    );
  });

  test("formatHashShort matches server hover convention", () => {
    assert.strictEqual(
      formatHashShort(BOUND_PINNED_STORE.contentHash),
      "blake3:27ff6038"
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

  test("parseSchemaInfoResult maps bound payload to ok", () => {
    const result = parseSchemaInfoResult(BOUND_PINNED_STORE);
    assert.strictEqual(result.kind, "ok");
    if (result.kind !== "ok") return;
    assert.strictEqual(result.info.bound && result.info.package, "demo");
  });
});

suite("statusPresentation/buildStatusPresentation", () => {
  test("bound tooltip includes content hash", () => {
    const info = parseSchemaInfo(BOUND_PINNED_STORE);
    assert.ok(info);
    const p = buildStatusPresentation(
      "running",
      "neutral nml-lsp (wasm)",
      { kind: "ok", info: info! },
      true
    );
    assert.match(p.tooltip, /blake3:27ff6038/);
    assert.match(
      p.tooltip,
      /27ff6038e8ab7bbaf17eb72d2cd526d57b1199e7272959b37d0706d5236fad4c/
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

  test("lifecycle absent shows no server", () => {
    const p = buildStatusPresentation("absent", "", { kind: "skipped" }, true);
    assert.strictEqual(p.text, "$(circle-slash) nml: no server");
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
