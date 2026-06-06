/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { buildHintsFromConfig, buildWorkflowHints } from "../workflow-hints.js";

describe("Pi buildWorkflowHints", () => {
  test("renders all four sections at tool_surface=all with bg + semantic", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      absentTools: new Set(),
    });
    expect(out).not.toBeNull();
    expect(out).toContain("## Prefer AFT tools for token efficiency");
    expect(out).toContain("**Parallel tool calls**");
    expect(out).toContain("emit them in ONE response instead of serializing");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain('`aft_outline({ target: "<url>" })`');
    expect(out).not.toContain("aft_outline({ url })");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("`aft_search` is the primary code-search tool");
    expect(out).toContain('`hint: "regex"`');
    expect(out).toContain("auto-routes concepts, identifiers, regex");
    // Imperative anti-bash-grep + parallel-wave steer must be present (parity).
    expect(out).toContain("fire independent lookups in ONE parallel tool-call wave");
    expect(out).toContain("DO NOT run `grep`/`rg`/`find` through `bash` to locate code");
    expect(out).toContain("the bash path is unindexed, unranked, serial");
    expect(out).toContain("Use `aft_callgraph`");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("`aft_inspect`");
    expect(out).toContain("diagnostics");
    expect(out).toContain("before you run tests or commit");
    expect(out).toContain("does not surface compile/type errors automatically");
    expect(out).toContain("**Long-running commands**");
    // Anti-polling guidance must be present so agents stop calling
    // bash_status back-to-back. Mirrors OpenCode plugin parity.
    expect(out).toContain("a completion reminder arrives automatically");
    expect(out).toContain("Do not poll");
    // Anti-sync-block steer (parity with OpenCode): end the turn or use async,
    // never sync-wait bash_watch on a long task.
    expect(out).toContain("end your turn");
    expect(out).toContain("do not sync-wait with `bash_watch` for a long task");
    expect(out).toContain("`task_id`");
    expect(out).toContain("`bash_status({ task_id })`");
    expect(out).not.toContain("taskId");
  });

  test("omits bg-bash section when background is disabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(out).not.toContain("**Long-running commands**");
  });

  test("shows pipe guidance only when compression is enabled", () => {
    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: true,
      absentTools: new Set(),
    });
    expect(on).toContain(
      "When AFT bash output compression is on, do NOT pipe test/build commands through grep/head/tail",
    );

    const off = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(off).not.toContain(
      "When AFT bash output compression is on, do NOT pipe test/build commands through grep/head/tail",
    );
  });

  test("omits navigate at recommended surface", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(out).not.toContain("Use `aft_callgraph`");
  });

  test("inspect hint is gated by registered tool availability", () => {
    const registered = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(registered).toContain("**Codebase health & diagnostics**");
    expect(registered).toContain("aft_inspect");

    const minimal = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(minimal).not.toContain("**Codebase health & diagnostics**");
    expect(minimal).not.toContain("aft_inspect");
  });

  test("returns null when all sections gated off by absentTools", () => {
    const out = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(["aft_outline", "aft_zoom"]),
    });
    // null proves the parallel-tool-call frame is never emitted on its own
    // (unshift runs only when sections already have content).
    expect(out).toBeNull();
  });
});

describe("Pi buildHintsFromConfig", () => {
  test("emits hints by default and includes hoisted bash name", () => {
    const config: AftConfig = {
      tool_surface: "recommended",
      experimental: { bash: { background: true } },
    };
    const out = buildHintsFromConfig(config, new Set(), true);
    expect(out).not.toBeNull();
    expect(out).toContain("`bash({ background: true })`");
    expect(out).toContain("**Codebase health & diagnostics**");
  });
});
