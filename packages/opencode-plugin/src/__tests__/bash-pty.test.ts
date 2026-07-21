/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { appendFile, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { createBashStatusTool, createBashTool } from "../tools/bash.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import { createBashWriteTool } from "../tools/bash_write.js";
import type { PluginContext } from "../types.js";
import { noopAsk, toolResultText } from "./test-helpers";

const tempDirs: string[] = [];

afterEach(async () => {
  _resetSubagentCacheForTest();
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

type BridgeResponse = Record<string, unknown>;

async function addArtifactBytes(
  command: string,
  params: Record<string, unknown>,
  response: BridgeResponse,
): Promise<BridgeResponse> {
  if (command !== "bash_status" || response.success === false) return response;
  const data = { ...response };
  const path = data.output_path;
  if (typeof path !== "string") return data;
  const bytes = await readFile(path);
  if (data.mode === "pty") {
    const offset = Math.max(0, Number(params.output_offset ?? 0));
    data.output_chunk_base64 = bytes.subarray(offset).toString("base64");
    data.output_next_offset = bytes.length;
    if (params.output_mode === "raw" || params.output_mode === "both") {
      data.pty_raw = bytes.toString("utf8");
    }
  }
  return data;
}

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
      return addArtifactBytes(command, params, await send(command, params));
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
  test("Test 20: pty true implies background true (no explicit flag needed)", async () => {
    const { ctx: pluginCtx, calls } = ctx(() => ({
      success: true,
      status: "running",
      task_id: "bash-pty-implied-bg",
      output:
        'PTY task started: bash-pty-implied-bg. Use bash_status({ taskId: "bash-pty-implied-bg", outputMode: "screen" }) to see the visible terminal, bash_write({ taskId: "bash-pty-implied-bg", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.',
    }));
    const bash = createBashTool(pluginCtx);
    // Caller omits background: true — plugin must auto-promote because pty:true
    // requires the polling background lifecycle.
    const output = toolResultText(await bash.execute({ command: "python", pty: true }, runtime()));
    expect(output).toContain("bash-pty-implied-bg");
    // Rust spawn payload sees background:true and pty:true.
    const lastCall = calls.at(-1);
    expect(lastCall?.params).toMatchObject({ pty: true, background: true });
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

  test("Test 25: bash_status outputMode screen returns server-rendered screen", async () => {
    const outputPath = await spill("raw bytes are not rendered in screen mode");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
      pty_screen: "hello\n    there",
    }));
    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-screen", outputMode: "screen" },
      runtime(),
    );
    expect(result).toContain("hello");
    expect(result).toContain("there");
    expect(result).not.toContain("raw bytes are not rendered");
  });

  test("Test 25b: bash_status outputMode both combines server screen with raw bytes", async () => {
    const outputPath = await spill("raw\u001b[31m-bytes");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
      pty_screen: "left\nwide",
    }));
    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-both", outputMode: "both" },
      runtime(),
    );
    expect(result).toContain(String.raw`"screen": "left\nwide"`);
    expect(result).toContain(String.raw`"raw": "raw\u001b[31m-bytes"`);
  });

  test("bash_status preserves full coordinated non-PTY preview", async () => {
    const preview = `BEGIN-${"x".repeat(2500)}-END`;
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pipes",
      output_preview: preview,
    }));

    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-long-preview" },
      runtime(),
    );

    expect(result).toContain(preview);
    expect(result).toContain("-END");
  });

  test("bash_status PTY path ignores compressed output_preview", async () => {
    const outputPath = await spill("raw pty bytes");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      mode: "pty",
      output_path: outputPath,
      output_preview: "COMPRESSED PIPE PREVIEW SHOULD NOT RENDER",
    }));

    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-pty-raw-guard", outputMode: "raw" },
      runtime(),
    );

    expect(result).toContain("raw pty bytes");
    expect(result).not.toContain("COMPRESSED PIPE PREVIEW");
  });

  test("Test 26: bash_status raw rereads the full PTY output file", async () => {
    const outputPath = await spill("first");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    const status = createBashStatusTool(pluginCtx);
    await status.execute({ taskId: "bash-raw-reread", outputMode: "raw" }, runtime());
    await appendFile(outputPath, "second");
    const second = await status.execute(
      { taskId: "bash-raw-reread", outputMode: "raw" },
      runtime(),
    );
    expect(second).toContain("firstsecond");
  });

  test("Test 26b: bash_watch pattern matches PTY bytes", async () => {
    const outputPath = await spill("booting\nready on pty\n");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    const result = await createBashWatchTool(pluginCtx).execute(
      { taskId: "bash-pty-wait", pattern: "ready on pty" },
      runtime(),
    );
    expect(result).toContain('matched "ready on pty" at offset 8');
    expect(result).toContain("ready on pty");
  });

  test("Test 26c: bash_watch PTY scan is independent from bash_status cursor", async () => {
    const outputPath = await spill("rea");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-pty-shared", outputMode: "raw" },
      runtime(),
    );
    await appendFile(outputPath, "dy\n");

    const result = await createBashWatchTool(pluginCtx).execute(
      { taskId: "bash-pty-shared", pattern: "ready", timeoutMs: 1 },
      runtime(),
    );

    expect(result).toContain('matched "ready" at offset 0');
  });

  test("Test 27: bash_status terminal PTY screen uses server-rendered text", async () => {
    const outputPath = await spill("raw terminal bytes");
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pty",
      output_path: outputPath,
      pty_screen: "done",
    }));
    const result = await createBashStatusTool(pluginCtx).execute(
      { taskId: "bash-done", outputMode: "screen" },
      runtime(),
    );
    expect(result).toContain("done");
    expect(result).not.toContain("raw terminal bytes");
  });
});
