import { afterEach, describe, expect, test } from "bun:test";
import { appendFile, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { __ptyCacheSizeForTests, __resetPtyCacheForTests } from "../shared/pty-cache.js";
import { registerBashTool } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

interface MockToolDef {
  name: string;
  label: string;
  description: string;
  parameters: Record<string, unknown>;
  execute: (
    toolCallId: string,
    params: Record<string, unknown>,
    signal: AbortSignal | undefined,
    onUpdate: ((update: unknown) => void) | undefined,
    ctx: { cwd: string },
  ) => Promise<unknown>;
}

const tempDirs: string[] = [];

afterEach(async () => {
  __resetPtyCacheForTests();
  await Promise.all(tempDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

function api(tools: Map<string, MockToolDef>): ExtensionAPI {
  return {
    registerTool: (tool: MockToolDef) => tools.set(tool.name, tool),
  } as unknown as ExtensionAPI;
}

function ctx(
  send: (
    command: string,
    params: Record<string, unknown>,
  ) => Record<string, unknown> | Promise<Record<string, unknown>>,
) {
  const calls: Array<[string, Record<string, unknown>]> = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push([command, params]);
      return await send(command, params);
    },
  } as unknown as BinaryBridge;
  return {
    calls,
    ctx: {
      pool: { getBridge: () => bridge } as PluginContext["pool"],
      config: {} as PluginContext["config"],
      storageDir: "/tmp/test",
    } satisfies PluginContext,
  };
}

async function spill(contents: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "aft-pi-pty-test-"));
  tempDirs.push(dir);
  const file = join(dir, "task.pty");
  await writeFile(file, contents);
  return file;
}

function text(result: unknown): string {
  return (result as { content: Array<{ type: string; text: string }> }).content[0].text;
}

describe("Pi bash PTY layer", () => {
  test("pty true requires background true", async () => {
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({ success: true }));
    registerBashTool(api(tools), pluginCtx);
    await expect(
      tools.get("bash")!.execute("call", { command: "python", pty: true }, undefined, undefined, {
        cwd: process.cwd(),
      }),
    ).rejects.toThrow("PTY mode requires background: true");
  });

  test("bash pty true forwards pty to bridge", async () => {
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      task_id: "bash-pty",
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash")!
      .execute("call", { command: "python", pty: true, background: true }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("bash-pty");
    expect(calls[0][0]).toBe("bash");
    expect(calls[0][1]).toMatchObject({ pty: true, background: true });
  });

  test("bash_write schema accepts task_id/input and returns bridge response", async () => {
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({ success: true, bytes_written: 3 }));
    registerBashTool(api(tools), pluginCtx);
    const bashWrite = tools.get("bash_write")!;
    expect(JSON.stringify(bashWrite.parameters)).toContain("task_id");
    expect(JSON.stringify(bashWrite.parameters)).toContain("input");
    const result = await bashWrite.execute(
      "call",
      { task_id: "bash-abc", input: "x\n" },
      undefined,
      undefined,
      { cwd: process.cwd() },
    );
    expect(text(result)).toContain('"bytes_written": 3');
    expect(calls[0][0]).toBe("bash_write");
    expect(calls[0][1]).toMatchObject({ task_id: "bash-abc", input: "x\n" });
  });

  test("bash_status output_mode raw returns raw bytes", async () => {
    const outputPath = await spill("raw\u001b[31m-bytes");
    const tools = new Map<string, MockToolDef>();
    const { calls, ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-raw", output_mode: "raw" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("raw\u001b[31m-bytes");
    expect(calls[0][1].output_mode).toBe("raw");
  });

  test("bash_status output_mode screen returns rendered screen", async () => {
    const outputPath = await spill("\u001b[2J\u001b[Hhello\u001b[10;5Hthere");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-screen", output_mode: "screen" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("hello");
    expect(text(result)).toContain("there");
  });

  test("bash_status cache reuses terminal across calls", async () => {
    const outputPath = await spill("first");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "running",
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    const status = tools.get("bash_status")!;
    await status.execute(
      "call",
      { task_id: "bash-cache", output_mode: "raw" },
      undefined,
      undefined,
      {
        cwd: process.cwd(),
      },
    );
    await appendFile(outputPath, "second");
    const second = await status.execute(
      "call",
      { task_id: "bash-cache", output_mode: "raw" },
      undefined,
      undefined,
      { cwd: process.cwd() },
    );
    expect(text(second)).toContain("second");
    expect(text(second)).not.toContain("firstsecond");
    expect(__ptyCacheSizeForTests()).toBe(1);
  });

  test("bash_status cache disposes on terminal status", async () => {
    const outputPath = await spill("done");
    const tools = new Map<string, MockToolDef>();
    const { ctx: pluginCtx } = ctx(() => ({
      success: true,
      status: "completed",
      exit_code: 0,
      mode: "pty",
      output_path: outputPath,
    }));
    registerBashTool(api(tools), pluginCtx);
    const result = await tools
      .get("bash_status")!
      .execute("call", { task_id: "bash-done", output_mode: "screen" }, undefined, undefined, {
        cwd: process.cwd(),
      });
    expect(text(result)).toContain("done");
    expect(__ptyCacheSizeForTests()).toBe(0);
  });
});
