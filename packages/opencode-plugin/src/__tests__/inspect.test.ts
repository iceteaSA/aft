/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import {
  createInspectTier2IdleScheduler,
  inspectTools,
  shouldRegisterInspectTool,
} from "../tools/inspect.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

let projectRoot: string;

beforeAll(() => {
  projectRoot = realpathSync(mkdtempSync(join(tmpdir(), "aft-test-repo-")));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

type BridgeResponse = Record<string, unknown>;
type SendCall = {
  command: string;
  params: Record<string, unknown>;
  options?: Record<string, unknown>;
};
type ToolCallCall = {
  sessionId: string | undefined;
  name: string;
  rawArgs: Record<string, unknown>;
  options?: Record<string, unknown>;
};

type CapturedTimer = {
  callback: () => void;
  delay: number;
  cleared: boolean;
};

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: BridgePool, config: Record<string, unknown>): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: config as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
}

function createMockSdkContext(directory = projectRoot): ToolContext {
  return {
    sessionID: "inspect-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

function schemaDescription(schema: unknown): string {
  const record = schema as { description?: string; _def?: { description?: string } };
  return record.description ?? record._def?.description ?? "";
}

function createInspectHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const toolCallCalls: ToolCallCall[] = [];
  const localBridge = {
    send: async (
      command: string,
      params: Record<string, unknown> = {},
      options?: Record<string, unknown>,
    ) => {
      sendCalls.push({ command, params, options });
      return await sendImpl(command, params);
    },
    toolCall: async (
      sessionId: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
      options?: Record<string, unknown>,
    ) => {
      toolCallCalls.push({ sessionId, name, rawArgs, options });
      return await sendImpl(name, rawArgs);
    },
  };
  const pool = {
    getBridge: () => localBridge,
  } as unknown as BridgePool;
  return {
    sendCalls,
    toolCallCalls,
    tools: inspectTools(createPluginContext(pool, {})),
  };
}

describe("aft_inspect tool", () => {
  test("description documents diagnostics and scope behavior", () => {
    const { tools } = createInspectHarness(() => ({ success: true, summary: {} }));
    const inspect = tools.aft_inspect;

    expect(inspect.description).toContain("diagnostics");
    expect(inspect.description).toContain("Tier 1 (todos, metrics)");
    expect(inspect.description).toContain("waits for a fresh reuse scan");
    expect(inspect.description).toContain("complete: false");
    expect(schemaDescription(inspect.args.scope)).toContain("Tier 1 scopes the scan");
    expect(schemaDescription(inspect.args.scope)).toContain(
      "Tier 2 scans project-wide and applies scope as a result filter",
    );
  });

  test("sends corrected inspect field names to the bridge", async () => {
    const { sendCalls, toolCallCalls, tools } = createInspectHarness(() => ({
      success: true,
      text: "ok",
    }));

    await tools.aft_inspect.execute(
      { sections: ["todos", "dead_code"], scope: "src", topK: 7 },
      createMockSdkContext(projectRoot),
    );

    expect(sendCalls).toEqual([]);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "inspect-session",
        name: "inspect",
        rawArgs: {
          sections: ["todos", "dead_code"],
          scope: join(projectRoot, "src"),
          topK: 7,
        },
        options: expect.objectContaining({
          keepBridgeOnTimeout: true,
          timeoutMs: 60_000,
        }),
      },
    ]);
  });

  test("normalizes empty sections and scope sentinels", async () => {
    const { toolCallCalls, tools } = createInspectHarness(() => ({ success: true, text: "ok" }));

    await tools.aft_inspect.execute(
      { sections: [], scope: "", topK: undefined },
      createMockSdkContext(projectRoot),
    );

    expect(toolCallCalls[0]?.rawArgs.sections).toBeUndefined();
    expect(toolCallCalls[0]?.rawArgs.scope).toBeUndefined();
    expect(toolCallCalls[0]?.rawArgs.topK).toBeUndefined();
  });

  test("returns the server-rendered Rust text body without JSON fallback", async () => {
    const rendered =
      "Duplicates: 2 (top by cost):\n  1083  a.ts == b.ts\nDead code: 1 (rust 1):\n  x.rs::foo\n\ndiagnostics: 1 errors, 0 warnings, 0 info, 2 hints";
    const { tools } = createInspectHarness(() => ({
      success: true,
      text: rendered,
      summary: { diagnostics: { errors: 1, warnings: 0, info: 0, hints: 2 } },
    }));

    const result = await tools.aft_inspect.execute({}, createMockSdkContext(projectRoot));
    const text = typeof result === "string" ? result : (result.output as string);

    expect(text).toBe(rendered);
    expect(text).toContain("Duplicates: 2 (top by cost):");
    expect(text).toContain("  x.rs::foo");
    expect(text).toContain("diagnostics: 1 errors, 0 warnings, 0 info, 2 hints");
    expect(text).not.toContain('"success"');
    expect(text).not.toContain("scanner_state");
  });

  test("registration gate follows surface, disabled_tools, and inspect.enabled", () => {
    expect(shouldRegisterInspectTool({ tool_surface: "recommended" })).toBe(true);
    expect(shouldRegisterInspectTool({ tool_surface: "all" })).toBe(true);
    expect(shouldRegisterInspectTool({ tool_surface: "minimal" })).toBe(false);
    expect(
      shouldRegisterInspectTool({
        tool_surface: "recommended",
        disabled_tools: ["aft_inspect"],
      }),
    ).toBe(false);
    expect(
      shouldRegisterInspectTool({
        tool_surface: "recommended",
        inspect: { enabled: false },
      }),
    ).toBe(false);
  });

  test("session.idle schedules Tier 2 inspect after the configured debounce", async () => {
    const timers: CapturedTimer[] = [];
    const runs: string[] = [];
    const scheduler = createInspectTier2IdleScheduler({
      isEnabled: () => true,
      idleMinutes: () => 4,
      run: async (sessionID) => {
        runs.push(sessionID);
      },
      setTimer: (callback, delay) => {
        const timer = { callback, delay, cleared: false };
        timers.push(timer);
        return timer as unknown as ReturnType<typeof setTimeout>;
      },
      clearTimer: (timer) => {
        (timer as unknown as CapturedTimer).cleared = true;
      },
    });

    scheduler.schedule("sid-1");
    expect(timers[0]?.delay).toBe(4 * 60 * 1000);

    timers[0]?.callback();
    await Promise.resolve();

    expect(runs).toEqual(["sid-1"]);
  });

  test("tool call during an idle window cancels the pending Tier 2 timer", () => {
    const timers: CapturedTimer[] = [];
    const scheduler = createInspectTier2IdleScheduler({
      isEnabled: () => true,
      idleMinutes: () => 4,
      run: async () => {},
      setTimer: (callback, delay) => {
        const timer = { callback, delay, cleared: false };
        timers.push(timer);
        return timer as unknown as ReturnType<typeof setTimeout>;
      },
      clearTimer: (timer) => {
        (timer as unknown as CapturedTimer).cleared = true;
      },
    });

    scheduler.schedule("sid-2");
    expect(timers[0]?.cleared).toBe(false);

    scheduler.clear("sid-2");

    expect(timers[0]?.cleared).toBe(true);
  });
});
