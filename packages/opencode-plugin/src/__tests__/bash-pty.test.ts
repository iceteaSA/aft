/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { appendFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { __ptyCacheSizeForTests, __resetPtyCacheForTests } from "../shared/pty-cache.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { createBashStatusTool, createBashTool } from "../tools/bash.js";
import { createBashWriteTool } from "../tools/bash_write.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

const tempDirs: string[] = [];

afterEach(async () => {
  __resetPtyCacheForTests();
  _resetSubagentCacheForTest();
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

type BridgeResponse = Record<string, unknown>;

function runtime(overrides: Partial<ToolContext> = {}): ToolContext {
  return {
    sessionID: "pty-session",
    messageID: "message",
    agent: "agent",
    directory: process.cwd(),
    worktree: process.cwd(),
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
    ...overrides,
  } as ToolContext;
}

function ctx(
  send: (
    command: string,
    params: Record<string, unknown>,
  ) => BridgeResponse | Promise<BridgeResponse>,
) {
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push({ command, params });
      return await send(command, params);
    },
  };
  const pluginCtx: PluginContext = {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
  return { calls, ctx: pluginCtx };
}

async function spill(contents: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "aft-pty-test-"));
  tempDirs.push(dir);
  const file = join(dir, "task.pty");
  await writeFile(file, contents);
  return file;
}

describe("OpenCode bash PTY layer", () => {
  test("Test 20: pty true requires background true", async () => {
    const { ctx: pluginCtx } = ctx(() => ({ success: true }));
    const bash = createBashTool(pluginCtx);
    await expect(bash.execute({ command: "python", pty: true }, runtime())).rejects.toThrow(
      "PTY mode requires background: true",
    );
  });

  test("Test 21: subagent pty true is rejected", async () => {
    const { ctx: pluginCtx } = ctx(() => ({ success: true }));
    pluginCtx.client = {
      session: { get: async () => ({ data: { parentID: "parent" } }) },
      lsp: {},
      find: {},
    } as PluginContext["client"];
    const bash = createBashTool(pluginCtx);
    await expect(
      bash.execute({ command: "python", pty: true, background: true }, runtime()),
    ).rejects.toThrow(
      "PTY mode is not available in subagent sessions; subagents cannot drive interactive terminals.",
    );
  });

  test("Test 22: bash_write schema accepts taskId/input", () => {
    const { ctx: pluginCtx } = ctx(() => ({ success: true }));
    const bashWrite = createBashWriteTool(pluginCtx);
    expect(bashWrite.args.taskId.safeParse("bash-abc").success).toBe(true);
    expect(bashWrite.args.input.safeParse("print('hello')\n").success).toBe(true);
  });

  test("Test 23: bash_write returns bridge response", async () => {
    const { calls, ctx: pluginCtx } = ctx(() => ({ success: true, bytes_written: 4 }));
    const bashWrite = createBashWriteTool(pluginCtx);
    const result = await bashWrite.execute({ taskId: "bash-abc", input: "hi\n" }, runtime());
    expect(result).toContain('"bytes_written": 4');
    expect(calls[0]).toMatchObject({ command: "bash_write" });
    expect(calls[0].params).toMatchObject({ task_id: "bash-abc", input: "hi\n" });
  });

  test("Test 24: bash_status outputMode raw returns raw bytes", async () => {
    const outputPath = await spill("raw\u001b[31m-bytes");
    const { calls, ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    const status = createBashStatusTool(pluginCtx);
    const result = await status.execute({ taskId: "bash-raw", outputMode: "raw" }, runtime());
    expect(result).toContain("raw\u001b[31m-bytes");
    expect(calls[0].params.output_mode).toBe("raw");
  });

  test("Test 25: bash_status outputMode screen returns rendered screen", async () => {
    const outputPath = await spill("\u001b[2J\u001b[Hhello\u001b[10;5Hthere");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-screen", outputMode: "screen" },
      runtime(),
    );
    expect(result).toContain("hello");
    expect(result).toContain("there");
  });

  test("Test 26: bash_status cache reuses terminal across calls", async () => {
    const outputPath = await spill("first");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    const status = createBashStatusTool(pluginCtx);
    await status.execute({ taskId: "bash-cache", outputMode: "raw" }, runtime());
    await appendFile(outputPath, "second");
    const second = await status.execute({ taskId: "bash-cache", outputMode: "raw" }, runtime());
    expect(second).toContain("second");
    expect(second).not.toContain("firstsecond");
    expect(__ptyCacheSizeForTests()).toBe(1);
  });

  test("Test 27: bash_status cache disposes on terminal status", async () => {
    const outputPath = await spill("done");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pty",
      output_path: outputPath,
    }));
    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-done", outputMode: "screen" },
      runtime(),
    );
    expect(result).toContain("done");
    expect(__ptyCacheSizeForTests()).toBe(0);
  });
});
