/**
 * Unit tests for aft_safety argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerSafetyTool } from "../tools/safety.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

describe("aft_safety adapter", () => {
  test("checkpoint forwards agent-facing filePath so server translation promotes it", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, {
      op: "checkpoint",
      name: "before-edit",
      filePath: "src/app.ts",
    });

    expect(calls[0]).toMatchObject({
      command: "tool_call",
      params: {
        name: "aft_safety",
        arguments: { op: "checkpoint", name: "before-edit", filePath: "src/app.ts" },
      },
    });
    expect(calls[0].params.arguments).not.toHaveProperty("file");
  });

  test("history requires filePath before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerSafetyTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_safety")!, { op: "history" })).rejects.toThrow(
      "requires 'filePath'",
    );
    expect(calls).toHaveLength(0);
  });

  test("undo without filePath previews then calls tool_call with operation args", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "undo_preview"
        ? { success: true, paths: [] }
        : { success: true, operation: true, text: "restored operation" },
    );
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "undo" });

    expect(calls.map((call) => call.command)).toEqual(["undo_preview", "tool_call"]);
    expect(calls[0].params).toEqual({});
    expect(calls[1].params).toMatchObject({
      name: "aft_safety",
      arguments: { op: "undo" },
    });
  });

  test("undo with filePath previews with file and forwards agent-facing filePath", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "undo_preview"
        ? { success: true, paths: ["src/app.ts"] }
        : { success: true, backup_id: "b1", text: "restored src/app.ts" },
    );
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "undo", filePath: "src/app.ts" });

    expect(calls[0].command).toBe("undo_preview");
    expect(calls[0].params).toMatchObject({ file: "src/app.ts" });
    expect(calls[1]).toMatchObject({
      command: "tool_call",
      params: {
        name: "aft_safety",
        arguments: { op: "undo", filePath: "src/app.ts" },
      },
    });
  });

  test("restore previews checkpoint paths then forwards restore args through tool_call", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "checkpoint_paths"
        ? { success: true, paths: ["src/a.ts"] }
        : { success: true, text: "checkpoint restored" },
    );
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, {
      op: "restore",
      name: "before-edit",
      files: ["src/a.ts", "src/b.ts"],
    });

    expect(calls[0].command).toBe("checkpoint_paths");
    expect(calls[0].params).toMatchObject({ name: "before-edit" });
    expect(calls[1]).toMatchObject({
      command: "tool_call",
      params: {
        name: "aft_safety",
        arguments: {
          op: "restore",
          name: "before-edit",
          files: ["src/a.ts", "src/b.ts"],
        },
      },
    });
  });
});
