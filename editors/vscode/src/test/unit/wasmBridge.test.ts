import * as assert from "node:assert";
import {
  buildUriMapping,
  duplicateFolderNames,
  hostToWasi,
  isPanicShapedStderr,
  wasiToHost,
  WasmFolder,
} from "../../wasmBridge";

suite("wasmBridge/buildUriMapping", () => {
  const single: WasmFolder[] = [
    { name: "app", uriString: "file:///Users/dev/ws" },
  ];
  const multi: WasmFolder[] = [
    { name: "app", uriString: "file:///Users/dev/app" },
    { name: "lib", uriString: "file:///Users/dev/lib" },
  ];
  // Nested folders: `pkg` lives inside `repo`.
  const nested: WasmFolder[] = [
    { name: "repo", uriString: "file:///Users/dev/repo" },
    { name: "pkg", uriString: "file:///Users/dev/repo/pkg" },
  ];

  test("single folder mounts at /workspace", () => {
    const mapping = buildUriMapping(single);
    assert.strictEqual(
      hostToWasi(mapping, "file:///Users/dev/ws/app.nml"),
      "file:///workspace/app.nml"
    );
    assert.strictEqual(
      hostToWasi(mapping, "file:///Users/dev/ws"),
      "file:///workspace"
    );
  });

  test("multi-root folders mount at /workspaces/<name>", () => {
    const mapping = buildUriMapping(multi);
    const cases: Array<[string, string]> = [
      ["file:///Users/dev/app/a.nml", "file:///workspaces/app/a.nml"],
      ["file:///Users/dev/lib/deep/b.nml", "file:///workspaces/lib/deep/b.nml"],
      ["file:///Users/dev/app", "file:///workspaces/app"],
    ];
    for (const [host, wasi] of cases) {
      assert.strictEqual(hostToWasi(mapping, host), wasi);
      assert.strictEqual(wasiToHost(mapping, wasi), host);
    }
  });

  test("nested roots: the most specific mount wins in both directions", () => {
    const mapping = buildUriMapping(nested);
    // host→wasi: a file under repo/pkg belongs to pkg, not repo.
    assert.strictEqual(
      hostToWasi(mapping, "file:///Users/dev/repo/pkg/x.nml"),
      "file:///workspaces/pkg/x.nml"
    );
    assert.strictEqual(
      hostToWasi(mapping, "file:///Users/dev/repo/other/x.nml"),
      "file:///workspaces/repo/other/x.nml"
    );
    // wasi→host: each mount maps back to its own folder.
    assert.strictEqual(
      wasiToHost(mapping, "file:///workspaces/pkg/x.nml"),
      "file:///Users/dev/repo/pkg/x.nml"
    );
    assert.strictEqual(
      wasiToHost(mapping, "file:///workspaces/repo/other/x.nml"),
      "file:///Users/dev/repo/other/x.nml"
    );
  });

  test("prefix-similar sibling folders do not cross-match", () => {
    // `app` vs `app2`: bare startsWith would misfile app2's files under app.
    const mapping = buildUriMapping([
      { name: "app", uriString: "file:///d/app" },
      { name: "app2", uriString: "file:///d/app2" },
    ]);
    assert.strictEqual(
      hostToWasi(mapping, "file:///d/app2/x.nml"),
      "file:///workspaces/app2/x.nml"
    );
    assert.strictEqual(
      wasiToHost(mapping, "file:///workspaces/app2/x.nml"),
      "file:///d/app2/x.nml"
    );
  });

  test("trailing slash on a folder URI is normalized away", () => {
    const mapping = buildUriMapping([
      { name: "app", uriString: "file:///Users/dev/ws/" },
    ]);
    assert.strictEqual(
      hostToWasi(mapping, "file:///Users/dev/ws/a.nml"),
      "file:///workspace/a.nml"
    );
  });

  test("non-workspace URIs pass through unchanged in both directions", () => {
    const mapping = buildUriMapping(multi);
    const outside = "file:///etc/hosts";
    assert.strictEqual(hostToWasi(mapping, outside), outside);
    assert.strictEqual(wasiToHost(mapping, outside), outside);
    const unmounted = "file:///workspaces/unknown/x.nml";
    assert.strictEqual(wasiToHost(mapping, unmounted), unmounted);
  });

  test("wasiToHost ∘ hostToWasi is the identity on a sample", () => {
    for (const folders of [single, multi, nested]) {
      const mapping = buildUriMapping(folders);
      const samples = [
        ...folders.map((f) => f.uriString.replace(/\/$/, "")),
        ...folders.map((f) => `${f.uriString.replace(/\/$/, "")}/deep/dir/x.nml`),
        "file:///outside/of/everything.nml",
      ];
      for (const s of samples) {
        assert.strictEqual(wasiToHost(mapping, hostToWasi(mapping, s)), s);
      }
    }
  });
});

suite("wasmBridge/duplicateFolderNames", () => {
  test("reports each duplicated name once", () => {
    const dupes = duplicateFolderNames([
      { name: "app", uriString: "file:///a/app" },
      { name: "app", uriString: "file:///b/app" },
      { name: "app", uriString: "file:///c/app" },
      { name: "lib", uriString: "file:///a/lib" },
      { name: "lib", uriString: "file:///b/lib" },
      { name: "solo", uriString: "file:///a/solo" },
    ]);
    assert.deepStrictEqual(dupes.sort(), ["app", "lib"]);
  });

  test("empty when all names are unique", () => {
    const dupes = duplicateFolderNames([
      { name: "app", uriString: "file:///a/app" },
      { name: "lib", uriString: "file:///a/lib" },
    ]);
    assert.deepStrictEqual(dupes, []);
  });
});

suite("wasmBridge/isPanicShapedStderr", () => {
  test("matches panic lines", () => {
    assert.strictEqual(
      isPanicShapedStderr("thread 'main' panicked at src/resolver.rs:41:9:"),
      true
    );
    assert.strictEqual(
      isPanicShapedStderr("note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"),
      true
    );
  });

  test("benign lines do not match", () => {
    assert.strictEqual(isPanicShapedStderr(""), false);
    assert.strictEqual(isPanicShapedStderr("indexing workspace: 14 files"), false);
    assert.strictEqual(
      isPanicShapedStderr("warning: schema package fell back to cache"),
      false
    );
  });
});
