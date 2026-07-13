/// <reference path="../bun-test.d.ts" />

import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ExtensionAPI, ExtensionContext, Theme } from "@earendil-works/pi-coding-agent";
import type { PluginContext } from "../types.js";

export interface MockToolDef {
  name: string;
  label?: string;
  description?: string;
  parameters?: unknown;
  execute: (
    toolCallId: string,
    params: any,
    signal: AbortSignal | undefined,
    onUpdate: ((update: unknown) => void) | undefined,
    ctx: ExtensionContext,
  ) => Promise<unknown>;
  renderCall?: (args: unknown, theme: Theme, context: unknown) => unknown;
  renderResult?: (result: unknown, options: unknown, theme: Theme, context: unknown) => unknown;
}

export interface MockCommandDef {
  description?: string;
  handler: (args: string, ctx: any) => Promise<void> | void;
}

export function makeMockApi(): {
  api: ExtensionAPI;
  tools: Map<string, MockToolDef>;
  commands: Map<string, MockCommandDef>;
} {
  const tools = new Map<string, MockToolDef>();
  const commands = new Map<string, MockCommandDef>();
  const api = {
    registerTool(tool: MockToolDef) {
      tools.set(tool.name, tool);
    },
    registerCommand(name: string, command: MockCommandDef) {
      commands.set(name, command);
    },
  } as unknown as ExtensionAPI;
  return { api, tools, commands };
}

export function makeMockBridge(
  handler: (
    command: string,
    params: Record<string, unknown>,
    options?: Record<string, unknown>,
  ) => Promise<Record<string, unknown>> | Record<string, unknown> = () => ({ success: true }),
): {
  bridge: BinaryBridge;
  calls: Array<{
    command: string;
    params: Record<string, unknown>;
    options?: Record<string, unknown>;
  }>;
} {
  const calls: Array<{
    command: string;
    params: Record<string, unknown>;
    options?: Record<string, unknown>;
  }> = [];
  const bridge = {
    cachedStatus: null as Record<string, unknown> | null,
    getCachedStatus() {
      return this.cachedStatus;
    },
    cacheStatusSnapshot(snapshot: Record<string, unknown>) {
      this.cachedStatus = snapshot;
    },
    async send(
      command: string,
      params: Record<string, unknown>,
      options?: Record<string, unknown>,
    ) {
      calls.push({ command, params, options });
      return handler(command, params, options);
    },
    async toolCall(
      sessionId: string | undefined,
      name: string,
      rawArgs: Record<string, unknown>,
      options?: Record<string, unknown>,
    ) {
      const { preview, ...sendOptions } = options ?? {};
      const params: Record<string, unknown> = { name, arguments: rawArgs };
      if (sessionId) params.session_id = sessionId;
      if (preview === true) params.preview = true;
      const normalizedOptions = Object.keys(sendOptions).length > 0 ? sendOptions : undefined;
      calls.push({ command: "tool_call", params, options: normalizedOptions });
      return handler(name, rawArgs, normalizedOptions);
    },
  } as unknown as BinaryBridge;
  return { bridge, calls };
}

export function makePluginContext(
  bridge: BinaryBridge,
  overrides: Partial<PluginContext> = {},
): PluginContext {
  return {
    pool: {
      getBridge: () => bridge,
    } as unknown as PluginContext["pool"],
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-pi-tests",
    ...overrides,
  };
}

export function makeExtContext(cwd = process.cwd(), sessionId?: string): ExtensionContext {
  return {
    cwd,
    hasUI: false,
    sessionManager: sessionId ? { getSessionId: () => sessionId } : undefined,
  } as unknown as ExtensionContext;
}

export async function executeTool(
  tool: MockToolDef,
  params: Record<string, unknown>,
  extCtx: ExtensionContext = makeExtContext(),
): Promise<unknown> {
  return tool.execute("call-id", params, undefined, undefined, extCtx);
}
