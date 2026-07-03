/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

// The CLI runs under plain Node via npx. Any @cortexkit import that survives
// bundling resolves the INSTALLED package at runtime; aft-bridge's dist chain
// loads @cortexkit/subc-client, which is published as TypeScript source that
// Node refuses to load from node_modules ("Stripping types is currently
// unsupported"). That broke `doctor --fix` binary downloads on npx installs.
// Every workspace import must therefore be inlined: literal specifiers only.
describe("CLI dist bundle is self-contained", () => {
  const distPath = join(import.meta.dir, "..", "..", "dist", "index.js");

  test("no runtime @cortexkit imports survive bundling", () => {
    if (!existsSync(distPath)) {
      // Source-only checkouts may not have built dist yet; the build step
      // always runs before publish, where this guard matters.
      return;
    }
    const bundle = readFileSync(distPath, "utf-8");

    const staticImports = bundle.match(/from\s*["']@cortexkit\/[^"']+["']/g) ?? [];
    expect(staticImports).toEqual([]);

    const dynamicImports = bundle.match(/import\(\s*["']@cortexkit\/[^"']+["']\s*\)/g) ?? [];
    expect(dynamicImports).toEqual([]);

    // A dynamic import with a non-literal specifier is invisible to the
    // bundler and becomes a runtime resolution — the exact bug class.
    const variableSpecifiers = (bundle.match(/import\(\s*[^"')\s][^)]*\)/g) ?? []).filter(
      (m) => !m.includes("import.meta"),
    );
    expect(variableSpecifiers).toEqual([]);
  });
});
