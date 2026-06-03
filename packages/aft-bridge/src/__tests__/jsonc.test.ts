/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { stripJsoncSymbols } from "../jsonc.js";

// Regression coverage for issue #88: comment-json attaches
// Symbol(before:<key>) / Symbol(after:<key>) properties to track comments.
// Anything that stringifies own-property keys (Zod error-path construction)
// throws "Cannot convert a symbol to a string" on those symbols, which
// silently dropped the whole config to defaults. stripJsoncSymbols produces a
// symbol-free deep copy for validation while leaving values intact.
//
// comment-json is bundled in the host plugins, not in aft-bridge, so these
// tests construct the exact symbol-keyed shape comment-json produces directly
// (Symbol(before:<key>) attached as an own property) rather than importing it.
function withCommentSymbol<T extends object>(obj: T, beforeKey: string): T {
  (obj as Record<symbol, unknown>)[Symbol.for(`before:${beforeKey}`)] = [
    { type: "LineComment", value: " a custom server", inline: false },
  ];
  return obj;
}

describe("stripJsoncSymbols", () => {
  test("removes comment symbols attached on nested objects", () => {
    const servers = withCommentSymbol({ "my-server": { binary: "my-lsp" } }, "my-server");
    const raw: Record<string, unknown> = {
      search_index: true,
      lsp: { servers },
    };
    // Precondition: the symbol key is really present.
    expect(Object.getOwnPropertySymbols(servers).length).toBeGreaterThan(0);

    const clean = stripJsoncSymbols(raw) as Record<string, unknown>;
    const cleanServers = (clean.lsp as { servers: Record<string, unknown> }).servers;
    expect(Object.getOwnPropertySymbols(cleanServers)).toHaveLength(0);
    // Walk every nested object: no symbol keys survive anywhere.
    const allSymbolsGone = (v: unknown): boolean => {
      if (Array.isArray(v)) return v.every(allSymbolsGone);
      if (v !== null && typeof v === "object") {
        if (Object.getOwnPropertySymbols(v).length > 0) return false;
        return Object.values(v as Record<string, unknown>).every(allSymbolsGone);
      }
      return true;
    };
    expect(allSymbolsGone(clean)).toBe(true);
  });

  test("preserves values and structure", () => {
    const raw: Record<string, unknown> = withCommentSymbol(
      {
        search_index: true,
        semantic: { max_files: 20000, model: "all-MiniLM-L6-v2" },
        lsp: { servers: { a: { binary: "x" } } },
      },
      "search_index",
    );
    const clean = stripJsoncSymbols(raw);
    expect(clean).toEqual({
      search_index: true,
      semantic: { max_files: 20000, model: "all-MiniLM-L6-v2" },
      lsp: { servers: { a: { binary: "x" } } },
    });
  });

  test("the stripped copy cannot throw on key stringification (the #88 crash path)", () => {
    const servers = withCommentSymbol({ bad: { binary: 123 } }, "bad");
    const raw: Record<string, unknown> = { lsp: { servers } };
    const clean = stripJsoncSymbols(raw) as Record<string, unknown>;
    // Simulate Zod's error-path stringification over every key in the tree.
    const stringifyAllKeys = (v: unknown): void => {
      if (Array.isArray(v)) {
        v.forEach(stringifyAllKeys);
        return;
      }
      if (v !== null && typeof v === "object") {
        for (const k of Reflect.ownKeys(v)) {
          // String(symbol) is fine; `${symbol}` is the throw. Use template form.
          `${String(k)}`;
          stringifyAllKeys((v as Record<string | symbol, unknown>)[k as never]);
        }
      }
    };
    // Reflect.ownKeys includes symbols; on the clean copy there are none, so the
    // template-literal interpolation that throws in #88 can never be reached.
    expect(() => stringifyAllKeys(clean)).not.toThrow();
    // Confirm the clean tree genuinely has zero symbol keys at the crash site.
    const cleanServers = (clean.lsp as { servers: Record<string, unknown> }).servers;
    expect(Reflect.ownKeys(cleanServers).every((k) => typeof k === "string")).toBe(true);
  });

  test("passes through primitives and arrays untouched", () => {
    expect(stripJsoncSymbols(42)).toBe(42);
    expect(stripJsoncSymbols("x")).toBe("x");
    expect(stripJsoncSymbols(null)).toBe(null);
    expect(stripJsoncSymbols([1, { a: 2 }])).toEqual([1, { a: 2 }]);
  });
});
