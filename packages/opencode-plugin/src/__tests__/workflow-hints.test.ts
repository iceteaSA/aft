/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import type { AftConfig } from "../config.js";
import { buildHintsFromConfig, buildWorkflowHints } from "../workflow-hints.js";

describe("buildWorkflowHints", () => {
  test("renders all four sections at tool_surface=all with bg + semantic enabled", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set(),
    });
    expect(out).not.toBeNull();
    expect(out).toContain("## Prefer AFT tools for token efficiency");
    expect(out).toContain("**Parallel tool calls**");
    expect(out).toContain("emit them in ONE response instead of serializing");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("`aft_search` is the primary code-search tool");
    expect(out).toContain('`hint: "regex"`');
    expect(out).toContain("auto-routes concepts, identifiers, regex");
    // Imperative anti-bash-grep + parallel-wave steer must be present.
    expect(out).toContain("fire independent lookups in ONE parallel tool-call wave");
    expect(out).toContain("DO NOT run `grep`/`rg`/`find` through `bash` to locate code");
    expect(out).toContain("the bash path is unindexed, unranked, serial");
    expect(out).toContain("Use `aft_callgraph`");
    expect(out).toContain("- `callers`");
    expect(out).toContain("- `impact`");
    expect(out).toContain("- `trace_to`");
    expect(out).toContain("- `trace_data`");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("`aft_inspect`");
    expect(out).toContain("diagnostics");
    expect(out).toContain("before you run tests or commit");
    expect(out).toContain("does not surface compile/type errors automatically");
    expect(out).toContain("**Long-running commands**");
    expect(out).toContain("`bash({ background: true })`");
    // Anti-polling guidance must be present so agents stop calling
    // bash_status back-to-back when waiting for a background task.
    expect(out).toContain("a completion reminder arrives automatically");
    expect(out).toContain("Do not poll");
    // Anti-sync-block steer: agents must not sync-wait bash_watch on a long task
    // (it freezes the turn and locks the user out); end the turn or use async.
    expect(out).toContain("end your turn");
    expect(out).toContain("do not sync-wait with `bash_watch` for a long task");
    expect(out).toContain("background: true");
  });

  test("omits long-running bash hint when background bash is off (foreground auto-promotes)", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).not.toBeNull();
    // Foreground bash now auto-promotes after a short wait-window, so we
    // don't need to teach the agent about timeouts up front. The bash hint
    // section is gone entirely when bg-bash is disabled.
    expect(out).not.toContain("background: true");
    expect(out).not.toContain("**Long-running commands**");
    expect(out).not.toContain("**Long-running bash commands**");
    expect(out).not.toContain("30 seconds");
  });

  test("shows pipe guidance only when compression is enabled", () => {
    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: true,
      disabledTools: new Set(),
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
      disabledTools: new Set(),
    });
    expect(off).not.toContain(
      "When AFT bash output compression is on, do NOT pipe test/build commands through grep/head/tail",
    );
  });

  test("omits the navigate section at tool_surface=recommended", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).not.toContain("Use `aft_callgraph`");
    expect(out).not.toContain("- `callers`");
  });

  test("uses aft_grep when hoist_builtin_tools is false", () => {
    const out = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: false,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(out).toContain("`aft_grep`");
    expect(out).not.toContain("`grep` to locate");
  });

  test("references aft_search only when semantic is enabled", () => {
    const off = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(off).not.toContain("aft_search");

    const on = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(on).toContain("aft_search");
  });

  test("inspect hint is gated by registered tool availability", () => {
    const registered = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(registered).toContain("**Codebase health & diagnostics**");
    expect(registered).toContain("aft_inspect");

    const minimal = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      disabledTools: new Set(),
    });
    expect(minimal).not.toContain("**Codebase health & diagnostics**");
    expect(minimal).not.toContain("aft_inspect");
  });

  test("returns null at minimal surface — only safety tool present", () => {
    // At minimal surface, aft_outline + aft_zoom may still be present, but
    // grep is not. Code-exploration section needs both. URL section still
    // works on outline+zoom alone, so we get a non-null block. Test the
    // truly empty case:
    const empty = buildWorkflowHints({
      toolSurface: "minimal",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      // Disable all tools that could produce a hint section. At minimal
      // surface, aft_callgraph/grep/aft_search are already absent; disabling
      // outline+zoom kills URL+exploration sections, bash kills the timeout
      // hint, leaving nothing to render.
      disabledTools: new Set(["aft_outline", "aft_zoom", "bash"]),
    });
    // null proves the parallel-tool-call frame is never emitted on its own
    // (unshift runs only when sections already have content).
    expect(empty).toBeNull();
  });

  test("section guarded by disabledTools", () => {
    const out = buildWorkflowHints({
      toolSurface: "all",
      hoistBuiltins: true,
      semanticEnabled: true,
      bashBackgroundEnabled: true,
      bashCompressionEnabled: true,
      disabledTools: new Set(["aft_callgraph", "bash_status"]),
    });
    // navigate section gated off (aft_callgraph disabled).
    expect(out).not.toContain("Use `aft_callgraph`");
    // bg-bash section gated off (bash_status disabled) — and there's no
    // 30s fallback anymore, foreground bash auto-promotes silently.
    expect(out).not.toContain("**Long-running commands**");
    expect(out).not.toContain("**Long-running bash commands**");
    expect(out).not.toContain("30 seconds");
    // Other sections survive.
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain("**Code exploration**");
  });
});

describe("buildHintsFromConfig", () => {
  test("emits hints by default", () => {
    const config: AftConfig = { tool_surface: "recommended" };
    const out = buildHintsFromConfig(config, new Set());
    expect(out).not.toBeNull();
    expect(out).toContain("## Prefer AFT tools for token efficiency");
  });

  test("honors hoist_builtin_tools=false (uses aft_grep)", () => {
    const config: AftConfig = { tool_surface: "recommended", hoist_builtin_tools: false };
    const out = buildHintsFromConfig(config, new Set());
    expect(out).toContain("`aft_grep`");
  });

  test("appends bg-bash hint by default on recommended (post-v0.27.2 graduation)", () => {
    // Bash + background are on by default for `recommended` after the bash
    // graduation, so the long-running hint surfaces without explicit opt-in.
    const defaults: AftConfig = { tool_surface: "recommended" };
    expect(buildHintsFromConfig(defaults, new Set())).toContain("**Long-running commands**");
  });

  test("omits bg-bash hint when bash: false (hard opt-out)", () => {
    const off: AftConfig = { tool_surface: "recommended", bash: false };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("**Long-running commands**");
  });

  test("omits bg-bash hint when bash: { background: false }", () => {
    const off: AftConfig = { tool_surface: "recommended", bash: { background: false } };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("**Long-running commands**");
  });

  test("omits bg-bash hint on tool_surface=minimal (bash off by default)", () => {
    const off: AftConfig = { tool_surface: "minimal" };
    expect(buildHintsFromConfig(off, new Set())).not.toContain("**Long-running commands**");
  });

  test("legacy experimental.bash.background=true still enables bg-bash hint", () => {
    const on: AftConfig = {
      tool_surface: "recommended",
      experimental: { bash: { background: true } },
    };
    expect(buildHintsFromConfig(on, new Set())).toContain("**Long-running commands**");
  });
});
