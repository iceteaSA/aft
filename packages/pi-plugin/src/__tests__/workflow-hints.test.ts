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
    expect(out).toContain("## IMPORTANT NOTICE about your tools");
    // Opening notice: non-standard tool set, reach for it first (parity).
    expect(out).toContain("You are equipped with a non-standard tool set");
    expect(out).toContain("Always reach for these tools first");
    expect(out).toContain("**Parallel tool calls**");
    expect(out).toContain("emit them in ONE response instead of serializing");
    expect(out).toContain("**Web/URL access**");
    expect(out).toContain('`aft_outline({ target: "<url>" })`');
    expect(out).not.toContain("aft_outline({ url })");
    expect(out).toContain("**Code exploration**");
    expect(out).toContain("`aft_search` is the primary code-search tool");
    expect(out).toContain('`hint: "regex"`');
    expect(out).toContain("auto-routes concepts, identifiers, regex");
    // Imperative anti-bash-grep steer with concrete reflex translations (parity).
    expect(out).toContain("DO NOT run `grep`/`rg`/`find`/`sed`/`cat` through `bash`");
    expect(out).toContain("the bash path is unindexed, unranked, serial");
    expect(out).toContain("Reflex translations:");
    expect(out).toContain('aft_search({ query: "handleAuth" })');
    expect(out).toContain("Use `aft_callgraph`");
    expect(out).toContain("**Codebase health & diagnostics**");
    expect(out).toContain("`aft_inspect`");
    expect(out).toContain("diagnostics");
    expect(out).toContain("before you run tests or commit");
    expect(out).toContain("does not surface compile/type errors automatically");
    expect(out).toContain("**Long-running commands**");
    expect(out).toContain("Nothing else useful to do");
    expect(out).toContain("sync `bash_watch`");
    expect(out).toContain("timeout_ms");
    expect(out).toContain("the user can interrupt");
    expect(out).toContain("completion reminder delivers the result");
    expect(out).toContain("async `bash_watch`");
    expect(out).toContain("Never loop `bash_status`");
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
      "bash output is auto-compressed — failures and the summary are always kept",
    );
    expect(on).toContain("`bun test | grep fail` → run `bun test`");
    // The agent can't check the config — the section is gated instead of hedged.
    expect(on).not.toContain("compression is on,");

    const off = buildWorkflowHints({
      toolSurface: "recommended",
      hoistBuiltins: true,
      semanticEnabled: false,
      bashBackgroundEnabled: false,
      bashCompressionEnabled: false,
      absentTools: new Set(),
    });
    expect(off).not.toContain("bash output is auto-compressed");
    expect(off).not.toContain("`bun test | grep fail`");
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
