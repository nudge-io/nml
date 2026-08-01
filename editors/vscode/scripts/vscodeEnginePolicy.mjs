/**
 * VS Code API floor policy — shared by the CI guard and its unit tests.
 *
 * `@types/vscode` must exactly match the semver floor of `engines.vscode` and
 * must never be newer. This mirrors `@vscode/vsce package` validation so
 * failures surface before the expensive extension pipeline runs.
 */

/** @returns {[number, number, number] | null} */
export function parseSemverTriple(spec) {
  if (typeof spec !== "string") return null;
  const m = spec.trim().match(/(\d+)\.(\d+)\.(\d+)/);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

/** @param {[number, number, number]} a @param {[number, number, number]} b */
export function compareSemver(a, b) {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return 0;
}

/**
 * @param {string} enginesVscode e.g. "^1.91.0"
 * @param {string} typesVscode e.g. "1.91.0"
 * @returns {{ ok: true } | { ok: false; reason: string }}
 */
export function validateVscodeEnginePolicy(enginesVscode, typesVscode) {
  const engineMin = parseSemverTriple(enginesVscode);
  const typesVer = parseSemverTriple(typesVscode);
  if (!engineMin) {
    return {
      ok: false,
      reason: `engines.vscode is not a recognizable semver range: ${JSON.stringify(enginesVscode)}`,
    };
  }
  if (!typesVer) {
    return {
      ok: false,
      reason: `@types/vscode is not a recognizable semver version: ${JSON.stringify(typesVscode)}`,
    };
  }
  if (compareSemver(typesVer, engineMin) > 0) {
    return {
      ok: false,
      reason:
        `@types/vscode ${typesVscode} is newer than engines.vscode ${enginesVscode}. ` +
        "Either lower @types/vscode or raise engines.vscode in the same intentional PR. " +
        "See editors/vscode/VSCODE-API.md.",
    };
  }
  if (compareSemver(typesVer, engineMin) < 0) {
    return {
      ok: false,
      reason:
        `@types/vscode ${typesVscode} is below the engines.vscode floor ${enginesVscode}. ` +
        "Raise @types/vscode to match the floor in the same intentional PR. " +
        "See editors/vscode/VSCODE-API.md.",
    };
  }
  return { ok: true };
}

/**
 * Ensure package.json @types/vscode matches the lockfile resolved version.
 * @param {string} typesVscode
 * @param {string} lockfileTypesVersion
 */
export function validateTypesLockfileAlignment(typesVscode, lockfileTypesVersion) {
  const declaredVer = parseSemverTriple(typesVscode);
  const lockedVer = parseSemverTriple(lockfileTypesVersion);
  if (!declaredVer || !lockedVer) {
    return {
      ok: false,
      reason:
        "cannot compare package.json @types/vscode with pnpm-lock.yaml resolved version.",
    };
  }
  if (compareSemver(declaredVer, lockedVer) !== 0) {
    return {
      ok: false,
      reason:
        `package.json @types/vscode ${typesVscode} does not match pnpm-lock.yaml resolved ${lockfileTypesVersion}. ` +
        "Run pnpm install from the repo root and commit the lockfile.",
    };
  }
  return { ok: true };
}

/**
 * @param {unknown} packageJson parsed package.json
 * @param {string} lockfileTypesVersion resolved @types/vscode from lockfile (required)
 */
export function validatePackageManifest(packageJson, lockfileTypesVersion) {
  if (!lockfileTypesVersion) {
    return {
      ok: false,
      reason:
        "lockfile @types/vscode version is required — run check-toolchain from the repo root after pnpm install.",
    };
  }

  const enginesVscode = packageJson?.engines?.vscode;
  const typesVscode = packageJson?.devDependencies?.["@types/vscode"];
  if (!enginesVscode || !typesVscode) {
    return {
      ok: false,
      reason:
        "package.json must declare engines.vscode and devDependencies['@types/vscode'].",
    };
  }
  const declared = validateVscodeEnginePolicy(enginesVscode, typesVscode);
  if (!declared.ok) return declared;

  const locked = validateVscodeEnginePolicy(enginesVscode, lockfileTypesVersion);
  if (!locked.ok) {
    return {
      ok: false,
      reason: `pnpm-lock.yaml @types/vscode ${lockfileTypesVersion}: ${locked.reason}`,
    };
  }

  return validateTypesLockfileAlignment(typesVscode, lockfileTypesVersion);
}
