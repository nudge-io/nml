// Route `require("vscode")` to the stub for the rest of this process. The
// unit harness runs under plain Node — the real module only exists inside the
// extension host — so any test that (transitively) loads a module importing
// `vscode` must import THIS module first. Import order is preserved in the
// CJS emit, so a leading `import "../support/installVscodeStub"` suffices.

import Module = require("node:module");

interface ModuleResolveInternals {
  _resolveFilename(this: unknown, request: string, ...rest: unknown[]): string;
}

const internals = Module as unknown as ModuleResolveInternals;
const original = internals._resolveFilename;
internals._resolveFilename = function (request: string, ...rest: unknown[]): string {
  if (request === "vscode") return require.resolve("./vscodeStub");
  return original.call(this, request, ...rest);
};
