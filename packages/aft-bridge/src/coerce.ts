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

/**
 * Coerce a tool argument that is contractually a boolean into a real boolean,
 * tolerating the shapes models send in practice.
 *
 * Like array and integer params (see `coerceStringArray` / `coerceOptionalInt`),
 * hosts deliver a boolean-typed param as the model's raw emitted value WITHOUT
 * coercing it to the declared schema type — and models non-deterministically
 * emit `true` as the string `"true"` (or `1` / `"1"`). A strict `args.x === true`
 * check then reads a stringified `"true"` as false, silently dropping the flag.
 * This bit `aft_delete`'s `recursive`: an agent passing `recursive: true` got
 * "is a directory, pass recursive: true" because the wire value was `"true"`.
 *
 * Conservative by design: only values that UNAMBIGUOUSLY mean true coerce to
 * true (`true`, case-insensitive `"true"`, `1`, `"1"`). Everything else —
 * `false`, `"false"`, `0`, `""`, `null`, `undefined`, objects — is `false`. A
 * false-negative just re-surfaces the original "pass the flag" error (safe); a
 * false-positive on a destructive gate like `recursive` would not be, so the
 * truthy set is kept tight rather than accepting arbitrary truthy values.
 */
export function coerceBoolean(value: unknown): boolean {
  if (typeof value === "boolean") return value;
  if (typeof value === "number") return value === 1;
  if (typeof value === "string") {
    const normalized = value.trim().toLowerCase();
    return normalized === "true" || normalized === "1";
  }
  return false;
}

/**
 * Runtime coercion for agent-friendly sentinel handling.
 *
 * Some agents emit null / "" / 0 when they mean "param not provided".
 * Use this inside tool handlers BEFORE relying on the value. Returns
 * `undefined` for all empty sentinels; rejects out-of-bounds with a
 * clear message.
 *
 * Tool handlers that want sentinel tolerance must pass args through
 * this AFTER schema validation has accepted the value (or for fields
 * declared as `unknown`/`any` that bypass type validation). Host-specific
 * optional-integer schemas stay local to each plugin; this helper is the
 * shared runtime coercion.
 */
export function coerceOptionalInt(
  v: unknown,
  paramName: string,
  min: number,
  max: number,
): number | undefined {
  if (v === undefined || v === null || v === "") return undefined;
  // 0 is an empty-param sentinel ONLY when 0 is out of bounds anyway. For
  // 0-indexed params (edit's `occurrence`, min=0) it is the most common legal
  // value — dropping it sent agents into an ambiguous_match loop that told
  // them to pass the param they had just passed.
  if (typeof v === "number" && (!Number.isFinite(v) || (v === 0 && min > 0))) return undefined;
  const n = typeof v === "string" ? Number(v) : v;
  if (typeof n !== "number" || !Number.isInteger(n)) {
    throw new Error(`${paramName} must be an integer between ${min} and ${max}`);
  }
  if (n < min || n > max) {
    throw new Error(`${paramName} must be between ${min} and ${max}`);
  }
  return n;
}

/**
 * True when a value represents "agent did not provide this param".
 *
 * GPT-family models send empty strings / empty arrays / null instead of
 * omitting optional params entirely. Use this BEFORE mutual-exclusion
 * checks so an empty `targets: []` or `url: ""` doesn't get counted as
 * present and trigger a misleading "X is mutually exclusive with Y" error.
 *
 * Treats undefined / null / "" / [] / {} as empty. Booleans and numbers
 * (including 0 and false) are NOT empty by themselves — only string and
 * collection sentinels qualify.
 */
export function isEmptyParam(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (typeof value === "string") return value.length === 0;
  if (Array.isArray(value)) return value.length === 0;
  if (typeof value === "object") return Object.keys(value as object).length === 0;
  return false;
}
