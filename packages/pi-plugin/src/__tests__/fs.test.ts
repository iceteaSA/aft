/**
 * Unit tests for aft_delete/aft_move tool_call argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { registerFsTools } from "../tools/fs.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("fs tool adapters", () => {
  test("aft_delete batches all files into one bridge call and reports partial success", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "Deleted 1/2 file(s)",
      complete: false,
      deleted: [{ file: "ok.ts", backup_id: "b-1" }],
      skipped_files: [{ file: "locked.ts", reason: "permission denied" }],
    }));
    registerFsTools(api, makePluginContext(bridge), { delete: true, move: true });

    const result = (await executeTool(
      tools.get("aft_delete")!,
      { files: ["ok.ts", "locked.ts"] },
      makeExtContext(projectRoot, "delete-session"),
    )) as { content: Array<{ text: string }>; details: Record<string, unknown> };

    expect(calls).toHaveLength(1);
    expect(calls[0].params).toEqual({
      name: "delete",
      arguments: {
        files: ["ok.ts", "locked.ts"],
        recursive: false,
      },
      session_id: "delete-session",
    });
    expect(result.content[0].text).toBe("Deleted 1/2 file(s)");
    expect(result.details).toMatchObject({
      complete: false,
      deleted: [{ file: "ok.ts", backup_id: "b-1" }],
      skipped_files: [{ file: "locked.ts", reason: "permission denied" }],
    });
  });

  test("aft_delete throws when every file fails instead of claiming success", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      text: "Deleted 1/2 file(s)",
      complete: false,
      deleted: [],
      skipped_files: [{ file: "missing.ts", reason: "file_not_found" }],
    }));
    registerFsTools(api, makePluginContext(bridge), { delete: true, move: false });

    await expect(executeTool(tools.get("aft_delete")!, { files: ["missing.ts"] })).rejects.toThrow(
      "delete failed for all 1 file(s)",
    );
  });

  test("aft_delete throws when bridge reports success=false", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      message: "Delete failed in bridge",
    }));
    registerFsTools(api, makePluginContext(bridge), { delete: true, move: false });

    await expect(executeTool(tools.get("aft_delete")!, { files: ["locked.ts"] })).rejects.toThrow(
      "Delete failed in bridge",
    );
  });

  test("aft_move forwards agent-typed filePath and destination", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "Moved src/old.ts → src/new.ts",
      moved: true,
    }));
    registerFsTools(api, makePluginContext(bridge), { delete: false, move: true });

    const result = (await executeTool(tools.get("aft_move")!, {
      filePath: "src/old.ts",
      destination: "src/new.ts",
    })) as { content: Array<{ text: string }> };

    expect(calls[0].command).toBe("tool_call");
    expect(calls[0].params).toMatchObject({
      name: "move",
      arguments: { filePath: "src/old.ts", destination: "src/new.ts" },
    });
    expect(result.content[0].text).toBe("Moved src/old.ts → src/new.ts");
  });
});
