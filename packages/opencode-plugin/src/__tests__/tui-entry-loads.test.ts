/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";

// The `./tui` export is raw TSX with `/** @jsxImportSource @opentui/solid */`,
// so importing it forces resolution of `@opentui/solid/jsx-dev-runtime` and
// `solid-js`. Ownership of those packages is the host's: OpenCode registers
// its own OpenTUI JSX transform before importing TUI plugins, and a
// plugin-local runtime copy creates the dual-instance/version-skew break
// (host 0.3.4 vs local 0.4.2 idle-CPU incident, and again on the next host
// bump). They are therefore declared as optional PEERS plus exact DEV deps —
// never runtime `dependencies`. In this repo the dev deps satisfy the
// resolution below; under a real OpenCode install the host copies do. The
// rest of the bun suite never imports the TSX entry, so nothing else catches
// resolution breaks in this class. This test imports the entry exactly the
// way OpenCode loads `./tui` and asserts it resolves + exposes the plugin
// shape.
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

  test("opentui and solid are never runtime dependencies", async () => {
    // The host owns the OpenTUI/solid runtime. Shipping them in
    // `dependencies` makes npm install a plugin-local copy that shadows the
    // host's, recreating the dual-instance/version-skew break on every host
    // bump. Optional peers + dev deps only.
    const pkg = (await import("../../package.json")) as {
      default: {
        dependencies?: Record<string, string>;
        peerDependencies?: Record<string, string>;
        peerDependenciesMeta?: Record<string, { optional?: boolean }>;
      };
    };
    const deps = pkg.default.dependencies ?? {};
    for (const banned of ["@opentui/core", "@opentui/solid", "solid-js"]) {
      expect(deps[banned]).toBeUndefined();
      expect(pkg.default.peerDependencies?.[banned]).toBeDefined();
      expect(pkg.default.peerDependenciesMeta?.[banned]?.optional).toBe(true);
    }
  });
});
