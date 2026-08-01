#!/usr/bin/env node
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { compileTsc } from "./toolchain.mjs";

compileTsc(join(dirname(fileURLToPath(import.meta.url)), ".."));
