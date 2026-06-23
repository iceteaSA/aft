import { describe, expect, test } from "bun:test";
import { coerceBoolean, coerceStringArray } from "../coerce.js";

describe("coerceBoolean", () => {
  test("passes real booleans through", () => {
    expect(coerceBoolean(true)).toBe(true);
    expect(coerceBoolean(false)).toBe(false);
  });

  test("coerces the stringified booleans models emit (the recursive bug)", () => {
    expect(coerceBoolean("true")).toBe(true);
    expect(coerceBoolean("TRUE")).toBe(true);
    expect(coerceBoolean("  true  ")).toBe(true);
    expect(coerceBoolean("1")).toBe(true);
    expect(coerceBoolean(1)).toBe(true);
  });

  test("treats everything else as false (tight truthy set for safety gates)", () => {
    expect(coerceBoolean("false")).toBe(false);
    expect(coerceBoolean("0")).toBe(false);
    expect(coerceBoolean(0)).toBe(false);
    expect(coerceBoolean(2)).toBe(false);
    expect(coerceBoolean("yes")).toBe(false);
    expect(coerceBoolean("")).toBe(false);
    expect(coerceBoolean(undefined)).toBe(false);
    expect(coerceBoolean(null)).toBe(false);
    expect(coerceBoolean({})).toBe(false);
    expect(coerceBoolean([])).toBe(false);
  });
});

describe("coerceStringArray", () => {
  test("passes a real string array through, dropping empties + non-strings", () => {
    expect(coerceStringArray(["a.ts", "b.ts"])).toEqual(["a.ts", "b.ts"]);
    expect(coerceStringArray(["a.ts", "", "b.ts"])).toEqual(["a.ts", "b.ts"]);
    expect(coerceStringArray(["a.ts", 3, null, "b.ts"] as unknown)).toEqual(["a.ts", "b.ts"]);
  });

  test("parses a JSON-stringified array (the crash trigger)", () => {
    expect(coerceStringArray('["a.ts","b.ts"]')).toEqual(["a.ts", "b.ts"]);
    expect(coerceStringArray('  ["only.ts"]  ')).toEqual(["only.ts"]);
  });

  test("wraps a single bare string as a one-element array", () => {
    expect(coerceStringArray("a.ts")).toEqual(["a.ts"]);
  });

  test("preserves spaces in a single path (no splitting)", () => {
    expect(coerceStringArray("my file.ts")).toEqual(["my file.ts"]);
    expect(coerceStringArray("a/b c/d.ts")).toEqual(["a/b c/d.ts"]);
  });

  test("returns empty for null/undefined/empty/other shapes", () => {
    expect(coerceStringArray(undefined)).toEqual([]);
    expect(coerceStringArray(null)).toEqual([]);
    expect(coerceStringArray("")).toEqual([]);
    expect(coerceStringArray("   ")).toEqual([]);
    expect(coerceStringArray([])).toEqual([]);
    expect(coerceStringArray(42)).toEqual([]);
    expect(coerceStringArray({ files: "a.ts" })).toEqual([]);
  });

  test("falls back to single-string when JSON is malformed", () => {
    // Looks array-ish but isn't valid JSON -> treat as a single path.
    expect(coerceStringArray("[not json")).toEqual(["[not json"]);
  });
});
