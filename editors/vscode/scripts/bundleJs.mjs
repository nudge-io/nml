#!/usr/bin/env node
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { bundleJs } from "./toolchain.mjs";

bundleJs(join(dirname(fileURLToPath(import.meta.url)), ".."));
