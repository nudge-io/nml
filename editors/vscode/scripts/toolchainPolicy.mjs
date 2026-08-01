import { compareSemver, parseSemverTriple } from "./vscodeEnginePolicy.mjs";

/**
 * @param {string} spec e.g. "pnpm@11.16.0"
 * @returns {{ name: string; version: string } | null}
 */
export function parsePackageManagerPin(spec) {
  if (typeof spec !== "string") return null;
  const m = spec.trim().match(/^([a-z]+)@(\d+\.\d+\.\d+)$/);
  if (!m) return null;
  return { name: m[1], version: m[2] };
}

/**
 * @param {string} engineSpec e.g. ">=22" or ">=22.0.0"
 * @param {string} actualVersion e.g. "22.14.0"
 */
export function satisfiesNodeEngine(engineSpec, actualVersion) {
  const actual = parseSemverTriple(actualVersion);
  if (!actual) {
    return {
      ok: false,
      reason: `node version is not recognizable: ${JSON.stringify(actualVersion)}`,
    };
  }

  const minMatch = engineSpec.match(/>=\s*(\d+)(?:\.(\d+))?(?:\.(\d+))?/);
  if (minMatch) {
    const min = [
      Number(minMatch[1]),
      Number(minMatch[2] ?? 0),
      Number(minMatch[3] ?? 0),
    ];
    if (compareSemver(actual, min) < 0) {
      return {
        ok: false,
        reason: `node ${actualVersion} is below engines.node ${engineSpec}.`,
      };
    }
    return { ok: true };
  }

  const floor = parseSemverTriple(engineSpec);
  if (!floor) {
    return {
      ok: false,
      reason: `engines.node is not a recognizable semver range: ${JSON.stringify(engineSpec)}`,
    };
  }
  if (compareSemver(actual, floor) < 0) {
    return {
      ok: false,
      reason: `node ${actualVersion} is below engines.node ${engineSpec}.`,
    };
  }
  return { ok: true };
}

/**
 * @param {string} engineSpec e.g. ">=11 <12"
 * @param {string} actualVersion e.g. "11.16.0"
 */
export function satisfiesPnpmEngine(engineSpec, actualVersion) {
  const actual = parseSemverTriple(actualVersion);
  if (!actual) {
    return {
      ok: false,
      reason: `pnpm version is not recognizable: ${JSON.stringify(actualVersion)}`,
    };
  }

  const minMatch = engineSpec.match(/>=\s*(\d+)/);
  const maxMatch = engineSpec.match(/<\s*(\d+)/);
  if (!minMatch) {
    return {
      ok: false,
      reason: `engines.pnpm is not a recognizable range: ${JSON.stringify(engineSpec)}`,
    };
  }

  const min = [Number(minMatch[1]), 0, 0];
  if (compareSemver(actual, min) < 0) {
    return {
      ok: false,
      reason: `pnpm ${actualVersion} is below engines.pnpm ${engineSpec}.`,
    };
  }

  if (maxMatch) {
    const max = [Number(maxMatch[1]), 0, 0];
    if (compareSemver(actual, max) >= 0) {
      return {
        ok: false,
        reason: `pnpm ${actualVersion} is outside engines.pnpm ${engineSpec}.`,
      };
    }
  }

  return { ok: true };
}

/**
 * @param {string} packageManagerField e.g. "pnpm@11.16.0"
 * @param {string} pnpmVersion e.g. "11.16.0"
 */
export function validatePackageManagerPin(packageManagerField, pnpmVersion) {
  const pin = parsePackageManagerPin(packageManagerField);
  if (!pin) {
    return {
      ok: false,
      reason: `root package.json packageManager is not recognizable: ${JSON.stringify(packageManagerField)}`,
    };
  }
  if (pin.name !== "pnpm") {
    return {
      ok: false,
      reason: `packageManager must pin pnpm, got ${JSON.stringify(pin.name)}.`,
    };
  }

  const actual = parseSemverTriple(pnpmVersion);
  const expected = parseSemverTriple(pin.version);
  if (!actual || !expected) {
    return {
      ok: false,
      reason: `cannot compare pnpm version ${JSON.stringify(pnpmVersion)} to packageManager ${JSON.stringify(packageManagerField)}.`,
    };
  }
  if (compareSemver(actual, expected) !== 0) {
    return {
      ok: false,
      reason:
        `pnpm ${pnpmVersion} does not match root packageManager ${packageManagerField}. ` +
        "Run corepack enable && corepack prepare from the repo root.",
    };
  }
  return { ok: true };
}
