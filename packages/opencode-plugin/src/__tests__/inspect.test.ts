/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import {
  createInspectTier2IdleScheduler,
  inspectTools,
  shouldRegisterInspectTool,
} from "../tools/inspect.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };

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

function createMockSdkContext(directory = "/tmp/inspect-tests"): ToolContext {
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

function createInspectHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const localBridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
    },
  };
  const pool = {
    getBridge: () => localBridge,
  } as unknown as BridgePool;
  return {
    sendCalls,
    tools: inspectTools(createPluginContext(pool, {})),
  };
}

describe("aft_inspect tool", () => {
  test("sends corrected inspect field names to the bridge", async () => {
    const { sendCalls, tools } = createInspectHarness(() => ({ success: true, summary: {} }));

    await tools.aft_inspect.execute(
      { sections: ["todos", "dead_code"], scope: "src", topK: 7 },
      createMockSdkContext("/repo"),
    );

    expect(sendCalls).toEqual([
      {
        command: "inspect",
        params: {
          sections: ["todos", "dead_code"],
          scope: "src",
          topK: 7,
          session_id: "inspect-session",
        },
      },
    ]);
  });

  test("normalizes empty sections and scope sentinels", async () => {
    const { sendCalls, tools } = createInspectHarness(() => ({ success: true, summary: {} }));

    await tools.aft_inspect.execute(
      { sections: [], scope: "", topK: undefined },
      createMockSdkContext("/repo"),
    );

    expect(sendCalls[0]?.params.sections).toBeUndefined();
    expect(sendCalls[0]?.params.scope).toBeUndefined();
    expect(sendCalls[0]?.params.topK).toBeUndefined();
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
