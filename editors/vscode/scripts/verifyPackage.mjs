#!/usr/bin/env node
// What the VSIX must contain, checked against the VSIX — not against the
// working tree it was built from.
//
// The launch supervisor is a plain CommonJS file that esbuild does NOT bundle
// (it is run by `process.execPath` as a script, not imported), so it reaches
// the package only through the copy step in `toolchain.mjs`. Nothing else
// would notice its absence: the extension activates, the language client
// starts, and every native server launch fails with ENOENT on a machine that
// is not the one that built it. The failure belongs here, at packaging time.
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { SUPERVISOR_FILE } from "./toolchain.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Every path the packager says it would ship. */
function packagedFiles() {
  const listed = spawnSync("pnpm", ["exec", "vsce", "ls", "--no-dependencies"], {
    cwd: packageDir,
    encoding: "utf8",
  });
  if (listed.status !== 0) {
    console.error(`verify-package: \`vsce ls\` failed\n${listed.stderr ?? ""}`);
    process.exit(1);
  }
  return listed.stdout
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
}

const REQUIRED = [
  "package.json",
  "dist/extension.js",
  `dist/${SUPERVISOR_FILE}`,
  "server/nml-lsp.wasm",
  "syntaxes/nml.tmLanguage.json",
];

const files = packagedFiles();
const missing = REQUIRED.filter((required) => !files.includes(required));
if (missing.length > 0) {
  console.error(
    `verify-package: the VSIX would ship without ${missing.join(", ")}.\n` +
      `  ${files.length} files would be packaged.\n` +
      "  Such a VSIX installs and activates: nothing fails until a user's\n" +
      "  machine reaches the missing file, and then every native server launch\n" +
      "  fails with ENOENT. That is why this is a packaging failure and not a\n" +
      "  warning.\n" +
      "  dist/ is produced by `pnpm run bundle:js` (esbuild + the supervisor copy);\n" +
      "  server/nml-lsp.wasm by `pnpm run bundle:wasm`. `vscode:prepublish` runs both."
  );
  process.exit(1);
}

// A .vsix in the package directory is the artifact CI uploads; if one was
// just built, say which.
const built = readdirSync(packageDir).filter((f) => f.endsWith(".vsix"));
console.log(
  `verify-package: ${files.length} files, all ${REQUIRED.length} required present` +
    (built.length > 0 ? ` (${built.join(", ")})` : "")
);
