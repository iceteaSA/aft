/// <reference path="../../bun-test.d.ts" />
import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { join } from "node:path";
import { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { $ } from "bun";
import { createBashStatusTool, createBashTool } from "../../tools/bash.js";
import { createBashWriteTool } from "../../tools/bash_write.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import { cleanupHarnesses, createHarness, type E2EHarness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);
const python = (
  await $`command -v python3 || command -v python`
    .quiet()
    .text()
    .catch(() => "")
).trim();

maybeDescribe("e2e bash PTY (OpenCode adapter + bridge + Rust)", () => {
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function pluginHarness() {
    const h = await createHarness(initialBinary, {
      fixtureNames: [],
      bridgeOptions: { timeoutMs: 20_000 },
    });
    harnesses.push(h);
    const pool = new BridgePool(
      h.binaryPath,
      { timeoutMs: 20_000 },
      {
        restrict_to_project_root: false,
        storage_dir: join(h.tempDir, ".aft-storage"),
        harness: "opencode",
        experimental_bash_background: true,
      },
    );
    const ctx: PluginContext = {
      pool,
      client: {} as PluginContext["client"],
      config: {} as PluginContext["config"],
      storageDir: join(h.tempDir, ".aft-storage"),
    };
    const cleanup = h.cleanup;
    Object.defineProperty(h, "cleanup", {
      value: async () => {
        await pool.shutdown();
        await cleanup.call(h);
      },
    });
    return {
      h,
      bash: createBashTool(ctx),
      status: createBashStatusTool(ctx),
      write: createBashWriteTool(ctx),
    };
  }

  function runtime(h: E2EHarness): ToolContext {
    return {
      sessionID: "pty-e2e-session",
      messageID: "message",
      agent: "agent",
      directory: h.tempDir,
      worktree: h.tempDir,
      abort: new AbortController().signal,
      metadata: () => {},
      ask: noopAsk,
    } as ToolContext;
  }

  async function screen(
    status: ReturnType<typeof createBashStatusTool>,
    h: E2EHarness,
    taskId: string,
  ): Promise<string> {
    return String(await status.execute({ taskId, outputMode: "screen" }, runtime(h)));
  }

  test.skipIf(!python)("Test 28: pty_e2e_python_repl", async () => {
    const { h, bash, status, write } = await pluginHarness();
    const launched = String(
      await bash.execute({ command: `${python} -q`, pty: true, background: true }, runtime(h)),
    );
    const taskId = launched.match(/bash-[a-zA-Z0-9_-]+/)?.[0];
    expect(taskId).toBeDefined();
    await write.execute({ taskId, input: "print('hello')\n" }, runtime(h));
    const started = Date.now();
    let out = "";
    while (Date.now() - started < 5_000) {
      out = await screen(status, h, taskId!);
      if (out.includes("hello")) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    expect(out).toContain("hello");
    await write.execute({ taskId, input: "exit()\n" }, runtime(h));
  });

  test("Test 29: pty_e2e_ansi_screen_rendering", async () => {
    const { h, bash, status } = await pluginHarness();
    const command = `printf '\\033[2J\\033[Halpha\\033[10;5Hbeta'`;
    const launched = String(
      await bash.execute({ command, pty: true, background: true }, runtime(h)),
    );
    const taskId = launched.match(/bash-[a-zA-Z0-9_-]+/)?.[0];
    expect(taskId).toBeDefined();
    const started = Date.now();
    let out = "";
    while (Date.now() - started < 5_000) {
      out = await screen(status, h, taskId!);
      if (out.includes("alpha") && out.includes("beta")) break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    expect(out).toContain("alpha");
    expect(out).toContain("beta");
  });
});
