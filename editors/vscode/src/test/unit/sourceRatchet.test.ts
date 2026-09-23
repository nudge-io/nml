import * as assert from "node:assert";
import { readFileSync, readdirSync, statSync } from "node:fs";
import * as path from "node:path";

// ─────────────────────────────────────────────────────────────────────────
// Ratchets over the extension's own source.
//
// Spawning is this extension's sharpest edge: a working directory, an
// environment, a process group and a way to end what was started. It is
// reviewable only while it happens in ONE file — and "we agreed to keep it
// there" is not a mechanism. These are.
// ─────────────────────────────────────────────────────────────────────────

/** `editors/vscode/` — `out/test/unit/` is three levels down from it. */
const packageDir = path.resolve(__dirname, "..", "..", "..");
const sourceDir = path.join(packageDir, "src");

/** Every production `.ts` under `src/` (the test tree is not shipped). */
function productionSources(dir: string): string[] {
  const found: string[] = [];
  for (const entry of readdirSync(dir)) {
    const full = path.join(dir, entry);
    if (statSync(full).isDirectory()) {
      if (entry === "test") continue;
      found.push(...productionSources(full));
    } else if (entry.endsWith(".ts")) {
      found.push(full);
    }
  }
  return found;
}

/** Source with its comments removed.
 *
 *  A ratchet that reads comments is a ratchet that fires on the paragraph
 *  EXPLAINING the rule — which is exactly what happened the first time this
 *  file was written. Line comments run to the end of the line, so an import
 *  statement can never be inside one; the crude regex is safe for what is
 *  asked of it here. */
function withoutComments(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/[^\n]*/g, "$1");
}

/** Module specifiers a file pulls in at RUNTIME: `import … from "x"`,
 *  `import "x"` and `require("x")`. `import type` is erased by the compiler
 *  and is not a dependency of the shipped bundle. */
function runtimeImports(rawSource: string): string[] {
  const source = withoutComments(rawSource);
  const specifiers: string[] = [];
  const importFrom = /(^|\n)\s*import\s+(?!type\s)(?:[^;'"]*?\sfrom\s+)?["']([^"']+)["']/g;
  const required = /\brequire\(\s*["']([^"']+)["']\s*\)/g;
  for (const match of source.matchAll(importFrom)) specifiers.push(match[2]);
  for (const match of source.matchAll(required)) specifiers.push(match[1]);
  return specifiers;
}

function isChildProcess(specifier: string): boolean {
  return specifier === "child_process" || specifier === "node:child_process";
}

suite("sourceRatchet/one spawn site", () => {
  test("only processLaunch.ts imports child_process", () => {
    const offenders = productionSources(sourceDir)
      .filter((file) => runtimeImports(readFileSync(file, "utf8")).some(isChildProcess))
      .map((file) => path.relative(packageDir, file))
      .filter((file) => file !== path.join("src", "processLaunch.ts"));
    assert.deepStrictEqual(
      offenders,
      [],
      "a second spawn site: every process this extension runs is launched, sandboxed and ended " +
        "in processLaunch.ts, and a launch that goes around it goes around the process group, " +
        "the death pipe and the staged termination"
    );
  });

  test("processLaunch.ts is still the one that does", () => {
    // A ratchet that would also pass if the mechanism were deleted is a
    // comment. This is the other half.
    const source = readFileSync(path.join(sourceDir, "processLaunch.ts"), "utf8");
    assert.ok(runtimeImports(source).some(isChildProcess), "processLaunch.ts spawns nothing");
  });
});

/** Every production module that reaches the extension host AT RUNTIME —
 *  `vscode` itself, or a package that imports it (`vscode-languageclient`,
 *  `@vscode/wasm-wasi`). Pinned as an exact set, both ways: a new name here
 *  is a module that can no longer be unit-tested under plain mocha or driven
 *  by the real-process suite, and a name that should have LEFT the set is a
 *  module whose purity nobody noticed.
 *
 *  Reaching the host is TRANSITIVE — a module is impure the moment one of
 *  its own imports is — so this list is computed the way the runtime works,
 *  not from each file's first line. That is the property that makes "pure"
 *  mean anything: `serverResolution.ts` is the resolution vocabulary
 *  precisely so that `processLaunch.ts` and the real-process suite can
 *  CONSTRUCT a resolution, and an `import { Uri } from "vscode"` added to it
 *  one day would take that away with nothing to notice. */
const REACHES_THE_HOST = [
  "clientManager.ts",
  "explain.ts",
  "extension.ts",
  "logging.ts",
  "providerDiscovery.ts",
  "serverAcquisition.ts",
  "serverSession.ts",
  "statusBar.ts",
];

/** A package that is, or pulls in, the extension host API. */
function isHostPackage(specifier: string): boolean {
  return (
    specifier === "vscode" ||
    specifier.startsWith("vscode-") ||
    specifier.startsWith("@vscode/")
  );
}

suite("sourceRatchet/the pure core stays pure", () => {
  test("exactly the declared modules reach the extension host at runtime", () => {
    const files = productionSources(sourceDir);
    const imports = new Map<string, string[]>();
    for (const file of files) {
      imports.set(file, runtimeImports(readFileSync(file, "utf8")));
    }
    /** Resolve a relative specifier to the file it names, or `undefined`. */
    const local = (from: string, specifier: string): string | undefined => {
      if (!specifier.startsWith(".")) return undefined;
      const resolved = path.resolve(path.dirname(from), `${specifier}.ts`);
      return imports.has(resolved) ? resolved : undefined;
    };
    const verdict = new Map<string, boolean>();
    /** Transitive: a module reaches the host if it imports a host package, or
     *  imports a module that does. A cycle answers `false` until one of its
     *  members finds a host package, which is the same answer the runtime
     *  gives (a cycle with no host import in it loads without one). */
    const reaches = (file: string, seen: Set<string>): boolean => {
      const already = verdict.get(file);
      if (already !== undefined) return already;
      if (seen.has(file)) return false;
      seen.add(file);
      const answer = (imports.get(file) ?? []).some(
        (specifier) =>
          isHostPackage(specifier) ||
          ((resolved) => resolved !== undefined && reaches(resolved, seen))(
            local(file, specifier)
          )
      );
      verdict.set(file, answer);
      return answer;
    };
    const impure = files
      .filter((file) => reaches(file, new Set()))
      .map((file) => path.relative(sourceDir, file))
      .sort();
    assert.deepStrictEqual(
      impure,
      REACHES_THE_HOST,
      "the pure/extension-host split moved: a module that newly reaches the host cannot be " +
        "unit-tested without an extension host (and cannot be reached from the real-process " +
        "suite); a module that no longer reaches it belongs out of REACHES_THE_HOST"
    );
  });
});

suite("sourceRatchet/a case that does not run here says why", () => {
  // The extension workflow runs the real-process suite on ubuntu, macOS AND
  // Windows and states that its cases "skip themselves WITH A REASON where a
  // mechanism does not exist". `this.skip()` alone cannot keep that promise:
  // a reporter prints the TITLE of a pending case and has no field for a
  // reason, so on the leg that skips, four bare lines were indistinguishable
  // from four tests somebody disabled. This is what makes the claim true.
  const realSuite = path.join(sourceDir, "test", "real", "processLaunch.test.ts");

  test("every `this.skip()` in the real-process suite is preceded by its reason", () => {
    // Comment lines are blanked, never dropped: a ratchet that read the
    // paragraph EXPLAINING the rule would fire on it (the bug this file's
    // own `withoutComments` exists for), and dropping lines would make the
    // numbers in the failure message point at the wrong place.
    const lines = readFileSync(realSuite, "utf8")
      .split("\n")
      .map((line) => (/^\s*(\/\/|\/\*|\*)/.test(line) ? "" : line));
    const unexplained: string[] = [];
    lines.forEach((line, index) => {
      if (!/\bthis\.skip\(\)/.test(line)) return;
      const before = lines.slice(0, index).filter((l) => l.trim() !== "");
      const previous = before[before.length - 1] ?? "";
      if (!previous.includes("whyNotHere(")) unexplained.push(`line ${index + 1}: ${line.trim()}`);
    });
    assert.deepStrictEqual(
      unexplained,
      [],
      "a platform case that skips itself without printing why: on the leg that skips, " +
        "the reason IS the result, and a bare pending line cannot be told from a disabled test"
    );
  });

  test("and the reason is actually printed, not only computed", () => {
    // The other half: a `whyNotHere` that wrote nowhere would satisfy the
    // ratchet above and still leave the CI log bare.
    const source = withoutComments(readFileSync(realSuite, "utf8"));
    assert.match(source, /function whyNotHere\(reason: string\): void \{\s*console\.log\(/);
  });
});

suite("sourceRatchet/an e2e suite title names the lane that is speaking", () => {
  // `.vscode-test.mjs` runs the SAME two files under four launches — two
  // wasm, two native — and every suite title read "WASM neutral server", so
  // the log a maintainer reads named the wasm backend four times and the
  // native lane appeared nowhere. The launch states its intent in
  // `NML_TEST_EXPECT_BACKEND`; a title has to take it from there, because
  // the titles are the only thing the log shows.
  const e2eSuites = [
    path.join(sourceDir, "test", "extension.test.ts"),
    path.join(sourceDir, "test", "multiroot.test.ts"),
  ];

  test("no e2e suite title hard-codes a backend", () => {
    const wrong: string[] = [];
    for (const file of e2eSuites) {
      const source = withoutComments(readFileSync(file, "utf8"));
      for (const match of source.matchAll(/suite\(\s*([`"'])([^`"']*)\1/g)) {
        const title = match[2];
        if (/\b(wasm|native)\b/i.test(title)) wrong.push(`${path.basename(file)}: ${title}`);
        if (!title.includes("${configuredBackend()}")) {
          wrong.push(`${path.basename(file)}: ${title} — does not name its lane`);
        }
      }
    }
    assert.deepStrictEqual(
      wrong,
      [],
      "an e2e suite title that names one backend is wrong on half the launches, and a " +
        "log that cannot tell the lanes apart cannot show that the native lane ran"
    );
  });

  test("and the helper reads the launch's own declaration", () => {
    const source = withoutComments(readFileSync(path.join(sourceDir, "test", "util.ts"), "utf8"));
    assert.match(source, /function configuredBackend\(\)[^}]*NML_TEST_EXPECT_BACKEND/s);
  });
});

suite("sourceRatchet/the launch supervisor stays dependency-free", () => {
  const supervisor = path.join(sourceDir, "launchSupervisor.js");

  test("it imports child_process and net, and nothing else", () => {
    // It is run by `process.execPath` as a bare script — inside VS Code that
    // is the Electron helper with ELECTRON_RUN_AS_NODE=1. There is no
    // bundler, no extension, no `node_modules` beside it: anything it cannot
    // get from Node's own core is not there.
    const allowed = new Set(["child_process", "node:child_process", "net", "node:net"]);
    const used = runtimeImports(readFileSync(supervisor, "utf8"));
    assert.ok(used.length > 0, "the supervisor spawns nothing");
    for (const specifier of used) {
      assert.ok(allowed.has(specifier), `the supervisor requires ${specifier}`);
    }
  });

  test("the control channel is a socket, never a file read", () => {
    // MEASURED: `fs.createReadStream(null, {fd: 3})` parks a libuv threadpool
    // thread in a blocking read(2), and the supervisor then cannot exit after
    // the provider exits by itself — so the editor never saw the normal
    // shutdown. The socket is the fix, and this is what stops it coming back.
    const source = withoutComments(readFileSync(supervisor, "utf8"));
    assert.ok(/new net\.Socket\(\{\s*fd:\s*3/.test(source), "fd 3 is not a socket");
    assert.ok(!/createReadStream/.test(source), "fd 3 is read as a file again");
  });
});
