import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  parsePackageManagerPin,
  satisfiesNodeEngine,
  satisfiesPnpmEngine,
  validatePackageManagerPin,
} from "./toolchainPolicy.mjs";

describe("parsePackageManagerPin", () => {
  it("parses pnpm pins", () => {
    assert.deepEqual(parsePackageManagerPin("pnpm@11.16.0"), {
      name: "pnpm",
      version: "11.16.0",
    });
  });

  it("rejects garbage", () => {
    assert.equal(parsePackageManagerPin("npm"), null);
  });
});

describe("validatePackageManagerPin", () => {
  it("accepts an exact Corepack pin match", () => {
    assert.deepEqual(
      validatePackageManagerPin("pnpm@11.16.0", "11.16.0"),
      { ok: true }
    );
  });

  it("rejects a drifted pnpm version", () => {
    const result = validatePackageManagerPin("pnpm@11.16.0", "11.15.0");
    assert.equal(result.ok, false);
    if (result.ok) return;
    assert.match(result.reason, /packageManager/);
  });
});

describe("satisfiesPnpmEngine", () => {
  it("accepts pnpm 11.x", () => {
    assert.deepEqual(satisfiesPnpmEngine(">=11 <12", "11.16.0"), { ok: true });
  });

  it("rejects pnpm 10", () => {
    const result = satisfiesPnpmEngine(">=11 <12", "10.12.0");
    assert.equal(result.ok, false);
  });

  it("rejects pnpm 12", () => {
    const result = satisfiesPnpmEngine(">=11 <12", "12.0.0");
    assert.equal(result.ok, false);
  });
});

describe("satisfiesNodeEngine", () => {
  it("accepts node 22+", () => {
    assert.deepEqual(satisfiesNodeEngine(">=22", "22.14.0"), { ok: true });
  });

  it("rejects node 20", () => {
    const result = satisfiesNodeEngine(">=22", "20.19.0");
    assert.equal(result.ok, false);
  });
});
