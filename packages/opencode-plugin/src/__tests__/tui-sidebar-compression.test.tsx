/// <reference path="../bun-test.d.ts" />

import { afterAll, describe, expect, mock, test } from "bun:test";
import { join } from "node:path";
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

const { formatCompressionSidebarRows, resolveTuiStorageDir, scopedSidebarSnapshot } = await import(
  "../tui/sidebar.tsx"
);

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
  test("TUI storage resolution matches CortexKit storage without importing the bridge barrel", () => {
    const original = process.env.XDG_DATA_HOME;
    process.env.XDG_DATA_HOME = "/tmp/aft-tui-storage-test";
    try {
      expect(resolveTuiStorageDir()).toBe(join("/tmp/aft-tui-storage-test", "cortexkit", "aft"));
    } finally {
      if (original === undefined) delete process.env.XDG_DATA_HOME;
      else process.env.XDG_DATA_HOME = original;
    }
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
