/**
 * JSONC parse helpers shared by host plugins.
 *
 * `comment-json` preserves comments by attaching `Symbol(before:<key>)` /
 * `Symbol(after:<key>)` properties to the parsed objects. Those symbol keys are
 * exactly what lets the migration round-trip rewrite a config without losing
 * user comments, but they are poison for anything that walks own-property keys
 * and stringifies them. Zod's validation builds error paths through the object
 * and interpolates each key into a string; the instant a comment symbol lands
 * in an error path (`path.join(".")` or a template literal), Node throws
 * `TypeError: Cannot convert a symbol to a string`, the whole config load is
 * caught by the outer try/catch, and every setting silently falls back to its
 * default. See issue #88.
 *
 * For the in-memory load path we don't need the comments, so we deep-copy the
 * parsed result into plain objects that carry only string-keyed enumerable
 * properties. The disk-rewrite/migration path keeps the original symbol-bearing
 * object so comments survive the round-trip.
 */
export function stripJsoncSymbols<T>(value: T): T {
  if (Array.isArray(value)) {
    return value.map((item) => stripJsoncSymbols(item)) as unknown as T;
  }
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    // Object.entries only yields string-keyed enumerable own properties, so the
    // comment symbols are dropped here by construction.
    for (const [key, val] of Object.entries(value as Record<string, unknown>)) {
      out[key] = stripJsoncSymbols(val);
    }
    return out as T;
  }
  return value;
}
