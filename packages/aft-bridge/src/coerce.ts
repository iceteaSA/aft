/**
 * Coerce a tool argument that is contractually a string array into a real
 * `string[]`, tolerating the shapes models/MCP clients send in practice.
 *
 * Some hosts deliver an array-typed param as a bare string (`"a.ts"`) or a
 * JSON-stringified array (`'["a.ts","b.ts"]'`) despite the declared schema.
 * A plain `args.files as string[]` cast then lies, and the first `.map`/
 * iteration throws (`inputs.map is not a function`) before any validation can
 * report a clean error. This normalizes at the boundary so callers get a real
 * array (possibly empty) and never crash on a mistyped argument.
 *
 * Accepts:
 *  - a string[] (non-string entries dropped, empties trimmed out)
 *  - a JSON-stringified array of strings (`'["a","b"]'`)
 *  - a single non-empty string (treated as a one-element array)
 *
 * Returns `[]` for null/undefined/empty/other shapes; the caller enforces any
 * "at least one" requirement and produces the user-facing error.
 */
export function coerceStringArray(value: unknown): string[] {
  if (Array.isArray(value)) {
    return value.filter((entry): entry is string => typeof entry === "string" && entry.length > 0);
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (trimmed.length === 0) return [];
    // JSON-stringified array, e.g. '["a.ts","b.ts"]'
    if (trimmed.startsWith("[") && trimmed.endsWith("]")) {
      try {
        const parsed = JSON.parse(trimmed);
        if (Array.isArray(parsed)) {
          return parsed.filter(
            (entry): entry is string => typeof entry === "string" && entry.length > 0,
          );
        }
      } catch {
        // Not valid JSON; fall through to single-string handling.
      }
    }
    // A single path. Do NOT split on spaces/commas: paths may contain spaces.
    return [value];
  }
  return [];
}
