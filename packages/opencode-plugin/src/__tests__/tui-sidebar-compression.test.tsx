/// <reference path="../bun-test.d.ts" />

import { afterAll, describe, expect, mock, test } from "bun:test";
import { join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import type { StatusCompression } from "../shared/status";

// These module mocks are applied process-globally by Bun. Restore them after
// this file so the @opentui/solid and solid-js stubs do not leak into other
// test files in the same `bun test` run.
afterAll(() => {
  mock.restore();
});

mock.module("@opentui/solid/jsx-dev-runtime", () => ({
  Fragment: (props: { children?: unknown }) => props.children,
  jsxDEV: () => null,
}));
mock.module("@opentui/solid/jsx-runtime", () => ({
  Fragment: (props: { children?: unknown }) => props.children,
  jsx: () => null,
  jsxs: () => null,
}));
mock.module("solid-js", () => ({
  createEffect: () => undefined,
  createMemo: (fn: () => unknown) => fn,
  createSignal: (initial: unknown) => [() => initial, () => undefined],
  on: (_source: unknown, fn: unknown) => fn,
  onCleanup: () => undefined,
}));

const {
  collapsedCompressionValue,
  collapsedHealthLights,
  formatCompressionSidebarRows,
  resolveTuiStorageDir,
  scopedSidebarSnapshot,
  shouldSuppressUninitializedDowngrade,
} = await import("../tui/sidebar.tsx");

const compression = (overrides: Partial<StatusCompression> = {}): StatusCompression => ({
  project: {
    events: 1234,
    original_tokens: 567000,
    compressed_tokens: 234000,
    savings_tokens: 333000,
  },
  session: { events: 12, original_tokens: 5600, compressed_tokens: 2300, savings_tokens: 3300 },
  ...overrides,
});

describe("sidebar compression rows", () => {
  test("TUI storage resolution matches CortexKit storage without importing the bridge barrel", async () => {
    await withEnv({ XDG_DATA_HOME: "/tmp/aft-tui-storage-test" }, () => {
      expect(resolveTuiStorageDir()).toBe(join("/tmp/aft-tui-storage-test", "cortexkit", "aft"));
    });
  });

  test("sidebar snapshot is scoped to the current directory and session", () => {
    const snapshot = { version: "test" } as any;
    const scoped = { directory: "/project/a", sessionID: "session-a", snapshot };

    expect(scopedSidebarSnapshot(scoped, "/project/a", "session-a")).toBe(snapshot);
    expect(scopedSidebarSnapshot(scoped, "/project/b", "session-a")).toBeNull();
    expect(scopedSidebarSnapshot(scoped, "/project/a", "session-b")).toBeNull();
    expect(scopedSidebarSnapshot(null, "/project/a", "session-a")).toBeNull();
  });

  test("sidebar_renders_compression_when_project_events_present", () => {
    const rows = formatCompressionSidebarRows(compression());

    // Each scope expands to: scope-header + Tokens Saved + Compression
    // Ratio. With both Session and Project, six rows total. 333,000
    // savings / 567,000 original ≈ 59% reduction.
    expect(rows).toHaveLength(6);
    expect(rows[0]).toEqual({ kind: "scope", label: "Session" });
    expect(rows[1]).toEqual({ kind: "stat", label: "Tokens Saved", value: "3,300" });
    expect(rows[2]).toEqual({ kind: "stat", label: "Compression Ratio", value: "59%" });
    expect(rows[3]).toEqual({ kind: "scope", label: "Project" });
    expect(rows[4]).toEqual({ kind: "stat", label: "Tokens Saved", value: "333,000" });
    expect(rows[5]).toEqual({ kind: "stat", label: "Compression Ratio", value: "59%" });
  });

  test("sidebar_hides_compression_when_undefined", () => {
    expect(formatCompressionSidebarRows(undefined)).toEqual([]);
  });

  test("sidebar_hides_compression_when_zero_events", () => {
    expect(
      formatCompressionSidebarRows(
        compression({
          project: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
        }),
      ),
    ).toEqual([]);
  });

  test("sidebar_hides_session_scope_when_session_events_zero", () => {
    const rows = formatCompressionSidebarRows(
      compression({
        session: { events: 0, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
      }),
    );

    // Only the Project scope (header + 2 stats) when session.events === 0.
    expect(rows).toHaveLength(3);
    expect(rows[0]).toEqual({ kind: "scope", label: "Project" });
    expect(rows.some((row) => row.kind === "scope" && row.label === "Session")).toBe(false);
  });
});

describe("shouldSuppressUninitializedDowngrade (sidebar flicker fix)", () => {
  test("suppresses a not_initialized downgrade when good data is already shown", () => {
    expect(shouldSuppressUninitializedDowngrade("not_initialized", true)).toBe(true);
  });

  test("allows the first not_initialized snapshot (no good data yet)", () => {
    expect(shouldSuppressUninitializedDowngrade("not_initialized", false)).toBe(false);
  });

  test("never suppresses a real (initialized) snapshot, even over good data", () => {
    expect(shouldSuppressUninitializedDowngrade("main", true)).toBe(false);
    expect(shouldSuppressUninitializedDowngrade("worktree", true)).toBe(false);
  });

  test("treats undefined cache_role as a real snapshot (does not suppress)", () => {
    expect(shouldSuppressUninitializedDowngrade(undefined, true)).toBe(false);
  });
});

describe("collapsedCompressionValue (collapsed sidebar row)", () => {
  test("returns null when no compression recorded (0 project events)", () => {
    expect(collapsedCompressionValue(undefined)).toBeNull();
    expect(
      collapsedCompressionValue(compression({ project: { ...compression().project, events: 0 } })),
    ).toBeNull();
  });

  test("shortens tokens and shows ratio — e.g. 7.6M / 64%", () => {
    const value = collapsedCompressionValue(
      compression({
        project: {
          events: 100,
          original_tokens: 11_900_000,
          compressed_tokens: 4_235_000,
          savings_tokens: 7_665_687,
        },
      }),
    );
    // 7,665,687 → 7.7M (formatCount rounds to one decimal); ratio 7.66M/11.9M ≈ 64%.
    expect(value).toBe("7.7M / 64%");
  });

  test("uses K scale for thousands", () => {
    const value = collapsedCompressionValue(
      compression({
        project: {
          events: 5,
          original_tokens: 10_000,
          compressed_tokens: 6_500,
          savings_tokens: 3_500,
        },
      }),
    );
    expect(value).toBe("4K / 35%");
  });

  test("0% ratio when original tokens is 0 (no divide-by-zero)", () => {
    const value = collapsedCompressionValue(
      compression({
        project: { events: 1, original_tokens: 0, compressed_tokens: 0, savings_tokens: 0 },
      }),
    );
    expect(value).toBe("0 / 0%");
  });
});

describe("collapsedHealthLights (collapsed Code Health traffic lights)", () => {
  const bar = (overrides = {}) => ({
    errors: 0,
    warnings: 0,
    dead_code: 0,
    unused_exports: 0,
    duplicates: 0,
    todos: 0,
    tier2_stale: false,
    ...overrides,
  });

  test("returns null when status bar is undefined (Tier-2 not populated)", () => {
    expect(collapsedHealthLights(undefined)).toBeNull();
  });

  test("all green on a clean bar", () => {
    expect(collapsedHealthLights(bar())).toEqual({
      diagnostics: "ok",
      code: "ok",
      todos: "ok",
    });
  });

  test("diagnostics light: red on errors (wins over warnings)", () => {
    expect(collapsedHealthLights(bar({ errors: 2, warnings: 5 }))?.diagnostics).toBe("err");
  });

  test("diagnostics light: yellow on warnings only", () => {
    expect(collapsedHealthLights(bar({ warnings: 3 }))?.diagnostics).toBe("warn");
  });

  test("code light: yellow when duplicates are non-zero", () => {
    expect(collapsedHealthLights(bar({ duplicates: 1167 }))?.code).toBe("warn");
  });

  test("code light: dead_code / unused_exports do NOT drive the light (hidden until oxc engine)", () => {
    // Until the oxc resolver lands, dead_code/unused_exports are excluded from
    // the user-facing health signal because the tree-sitter scanner over-reports.
    expect(collapsedHealthLights(bar({ dead_code: 999, unused_exports: 999 }))?.code).toBe("ok");
  });

  test("code light: green when duplicates are zero", () => {
    expect(
      collapsedHealthLights(bar({ dead_code: 0, unused_exports: 0, duplicates: 0 }))?.code,
    ).toBe("ok");
  });

  test("todos light: yellow when any todos, green otherwise", () => {
    expect(collapsedHealthLights(bar({ todos: 4 }))?.todos).toBe("warn");
    expect(collapsedHealthLights(bar({ todos: 0 }))?.todos).toBe("ok");
  });
});
