/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { __test__ } from "../index.js";
import { registerInspectTool } from "../tools/inspect.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

describe("Pi aft_inspect surface", () => {
  test("registers at recommended surface unless explicitly disabled", () => {
    expect(__test__.resolveToolSurface({ tool_surface: "recommended" }).inspect).toBe(true);
    expect(__test__.resolveToolSurface({ tool_surface: "minimal" }).inspect).toBe(false);
    expect(
      __test__.resolveToolSurface({
        tool_surface: "recommended",
        disabled_tools: ["aft_inspect"],
      }).inspect,
    ).toBe(false);
    expect(
      __test__.resolveToolSurface({
        tool_surface: "recommended",
        inspect: { enabled: false },
      }).inspect,
    ).toBe(false);
  });
});

describe("Pi aft_inspect adapter", () => {
  test("sends corrected inspect field names to the bridge", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, summary: {} }));
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(
      tools.get("aft_inspect")!,
      { sections: "todos", scope: ["src", "tests"], topK: 9 },
      makeExtContext("/repo", "pi-session"),
    );

    expect(calls[0].command).toBe("inspect");
    expect(calls[0].params).toEqual({
      sections: "todos",
      scope: ["src", "tests"],
      topK: 9,
      session_id: "pi-session",
    });
  });

  test("normalizes empty sections and scope sentinels", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, summary: {} }));
    registerInspectTool(api, makePluginContext(bridge));

    await executeTool(
      tools.get("aft_inspect")!,
      { sections: [], scope: "" },
      makeExtContext("/repo", "pi-session"),
    );

    expect(calls[0].params.sections).toBeUndefined();
    expect(calls[0].params.scope).toBeUndefined();
    expect(calls[0].params.topK).toBeUndefined();
  });
});
