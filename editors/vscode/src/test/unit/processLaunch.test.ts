import * as assert from "node:assert";
import {
  MAX_CONTROL_LINE,
  parseControlLine,
  takeControlLines,
  taskkillProgram,
} from "../../processLaunch";

// ─────────────────────────────────────────────────────────────────────────
// The launch's two edges that are pure enough to pin here: the program the
// Windows branch runs, and the control channel READ AS DATA.
//
// The rest of this file's surface is a process, and processes are settled in
// `test/real/processLaunch.test.ts` — including the one fact these tests
// stand on: the provider does not inherit the control descriptor.
// ─────────────────────────────────────────────────────────────────────────

suite("processLaunch/the Windows tree-killer is named absolutely", () => {
  test("it is under %SystemRoot%\\System32, never a bare name", () => {
    // A bare `taskkill` is resolved by libuv's own search, which looks in the
    // process's CURRENT DIRECTORY before `PATH` (`src/win/process.c`,
    // `search_path`) and appends `.com` before `.exe`. The extension host's
    // cwd is inherited from whatever started the editor — `code .` in a
    // repository makes that repository the first place looked.
    const program = taskkillProgram({ SystemRoot: "C:\\Windows" });
    assert.strictEqual(program, "C:\\Windows\\System32\\taskkill.exe");
    assert.ok(!program.includes("/"), "a Windows path, whatever host built it");
  });

  test("a %SystemRoot% that is not an absolute path cannot redirect it", () => {
    for (const root of [".", "", "System32", "..\\..\\tmp"]) {
      assert.strictEqual(
        taskkillProgram({ SystemRoot: root }),
        "C:\\Windows\\System32\\taskkill.exe",
        `SystemRoot=${JSON.stringify(root)} must not decide the program`
      );
    }
    assert.strictEqual(taskkillProgram({}), "C:\\Windows\\System32\\taskkill.exe");
  });

  test("an operator's own Windows directory is honoured", () => {
    assert.strictEqual(
      taskkillProgram({ SystemRoot: "D:\\Win" }),
      "D:\\Win\\System32\\taskkill.exe"
    );
  });
});

suite("processLaunch/the control channel is parsed as untrusted data", () => {
  test("a pid that names no PROCESS is not read as one", () => {
    // What a pid becomes is `process.kill(-pid)`: `-1` is every process this
    // account may signal, `-0` is the editor's OWN process group, and a
    // negative pid inverts into a single arbitrary process. None of them is a
    // provider, so none of them is a pid.
    for (const pid of [1, 0, -1, -4242, 1.5, Number.NaN, Number.MAX_SAFE_INTEGER + 2]) {
      const report = parseControlLine(JSON.stringify({ pid }));
      assert.strictEqual(report?.pid, undefined, `pid ${String(pid)} must be dropped`);
    }
    assert.strictEqual(parseControlLine('{"pid":"4242"}')?.pid, undefined);
    assert.strictEqual(parseControlLine('{"pid":4242}')?.pid, 4242);
  });

  test("an exit record with the wrong shapes still reads as an exit", () => {
    assert.deepStrictEqual(parseControlLine('{"exit":{"code":7,"signal":null}}')?.exit, {
      code: 7,
      signal: null,
    });
    assert.deepStrictEqual(parseControlLine('{"exit":{"signal":"SIGKILL"}}')?.exit, {
      code: null,
      signal: "SIGKILL",
    });
    assert.deepStrictEqual(parseControlLine('{"exit":{"code":"7","signal":9}}')?.exit, {
      code: null,
      signal: null,
    });
  });

  test("anything that is not the contract reports nothing", () => {
    for (const line of ["", "not json", "[1,2,3]", "null", '"pid"', "7"]) {
      const report = parseControlLine(line);
      assert.ok(
        report === undefined || (report.pid === undefined && report.exit === undefined),
        `${JSON.stringify(line)} must report nothing`
      );
    }
  });

  test("whole lines come out in order, and the remainder is kept", () => {
    const first = takeControlLines("", '{"pid":11}\n{"pid":2');
    assert.deepStrictEqual(first.lines, ['{"pid":11}']);
    assert.strictEqual(first.rest, '{"pid":2');
    const second = takeControlLines(first.rest, '2}\n{"exit":{"code":0}}\n');
    assert.deepStrictEqual(second.lines, ['{"pid":22}', '{"exit":{"code":0}}']);
    assert.strictEqual(second.rest, "");
  });

  test("a line that never ends is DROPPED, not accumulated", () => {
    // Otherwise a writer on this descriptor grows the extension host's heap
    // without bound, one chunk at a time, and nothing ever frees it.
    let rest = "";
    for (let i = 0; i < 64; i += 1) {
      rest = takeControlLines(rest, "x".repeat(8 * 1024)).rest;
      assert.ok(
        rest.length <= MAX_CONTROL_LINE,
        `the buffer reached ${rest.length} bytes with no newline`
      );
    }
    // …and the next newline resynchronises the stream.
    const resumed = takeControlLines(rest, '\n{"pid":4242}\n');
    assert.deepStrictEqual(resumed.lines[resumed.lines.length - 1], '{"pid":4242}');
    assert.strictEqual(resumed.rest, "");
  });
});
