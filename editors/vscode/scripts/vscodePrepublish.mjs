#!/usr/bin/env node
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { runInPackage, bundleWasm, bundleJs, compileTsc } from "./toolchain.mjs";

const packageDir = join(dirname(fileURLToPath(import.meta.url)), "..");

runInPackage(packageDir, process.execPath, ["scripts/check-toolchain.mjs"]);
bundleWasm(packageDir);
bundleJs(packageDir);
compileTsc(packageDir);
