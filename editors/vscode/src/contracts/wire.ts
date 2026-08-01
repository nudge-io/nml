/** Shared wire-skepticism helpers — trust nothing off JSON-RPC. */

export function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

export function readString(
  obj: Record<string, unknown>,
  key: string
): string | undefined {
  const v = obj[key];
  return typeof v === "string" ? v : undefined;
}

export function readStringArray(
  obj: Record<string, unknown>,
  key: string
): string[] | undefined {
  const v = obj[key];
  if (!Array.isArray(v)) return undefined;
  const out: string[] = [];
  for (const item of v) {
    if (typeof item !== "string") return undefined;
    out.push(item);
  }
  return out;
}

export function readBoolean(
  obj: Record<string, unknown>,
  key: string
): boolean | undefined {
  const v = obj[key];
  return typeof v === "boolean" ? v : undefined;
}
