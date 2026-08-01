#!/usr/bin/env node
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { bundleWasm } from "./toolchain.mjs";

bundleWasm(join(dirname(fileURLToPath(import.meta.url)), ".."));
