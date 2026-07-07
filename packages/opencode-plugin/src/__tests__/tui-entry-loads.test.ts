/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";

// The `./tui` export is raw TSX with `/** @jsxImportSource @opentui/solid */`,
// so importing it forces resolution of `@opentui/solid/jsx-dev-runtime` and
// `solid-js`. Those MUST be runtime `dependencies`, pinned to the OpenTUI
// line the host embeds: OpenTUI's Solid transform excludes any path under
// node_modules, and a PUBLISHED plugin's TUI entry lives inside OpenCode's
// plugin cache under node_modules — so the host transform is skipped there
// and bare imports resolve through the package's own node_modules. Removing
// the deps passes every dev-checkout test (file paths outside node_modules
// DO get the host transform) and then kills the sidebar silently for every
// npm-install user (magic-context v0.31.1 shipped that shape and broke;
// OpenCode Alfonso traced the exclusion to createSolidTransformPlugin()).
// Dev-path validation cannot catch this class: verify published shape via
// npm pack + prod-only install + import of the TUI entry. This test imports
// the entry exactly the way OpenCode loads `./tui` and asserts it resolves +
// exposes the plugin shape.
describe("tui entry module resolution", () => {
  test("the ./tui entry imports without a missing-module error", async () => {
    const mod = (await import("../tui/index.tsx")) as {
      default?: { id?: string; tui?: unknown };
    };
    expect(mod.default).toBeDefined();
    expect(mod.default?.id).toBe("aft-opencode");
    expect(typeof mod.default?.tui).toBe("function");
  });

  test("the @opentui/solid jsx runtime resolves from this package", () => {
    // The pragma compiles to `@opentui/solid/jsx-dev-runtime`; if it is not a
    // declared dep, this throws MODULE_NOT_FOUND.
    const resolved = require.resolve("@opentui/solid/jsx-dev-runtime");
    expect(resolved).toContain("@opentui");
    // solid-js must resolve to a single physical copy (dual-instance would break
    // Solid's reactive context across the OpenTUI tree even though it resolves).
    const solid = require.resolve("solid-js");
    expect(solid).toContain("solid-js");
  });

  test("opentui and solid are exact-pinned runtime dependencies", async () => {
    // Published TUI entries import these from INSIDE OpenCode's plugin cache
    // (under node_modules), where the host's Solid transform does not apply —
    // they must ship as runtime deps or published installs die at import.
    // Exact pins (no ^/~ range) keep the local copy tracking the host's
    // embedded OpenTUI line instead of drifting ahead on a range.
    const pkg = (await import("../../package.json")) as {
      default: { dependencies?: Record<string, string> };
    };
    const deps = pkg.default.dependencies ?? {};
    for (const required of ["@opentui/core", "@opentui/solid", "solid-js"]) {
      const pin = deps[required];
      expect(pin).toBeDefined();
      expect(/^\d/.test(pin ?? "")).toBe(true);
    }
  });
});
