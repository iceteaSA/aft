/**
 * Unit tests for AST tool argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { astTools } from "../tools/ast.js";
import type { PluginContext } from "../types.js";

function createSdkContext(directory: string): ToolContext {
  return {
    sessionID: "test-session",
    messageID: "test-message",
    agent: "build",
    abort: new AbortController().signal,
    directory,
    worktree: directory,
    metadata: () => {},
  } as ToolContext;
}

describe("AST tool adapters", () => {
  test('ast_grep_replace treats string dryRun "true" as preview (dry_run true)', async () => {
    const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
    const bridge = {
      send: async (command: string, params: Record<string, unknown> = {}) => {
        calls.push({ command, params });
        return { success: true, dry_run: true, text: "preview" };
      },
    };
    const pool = { getBridge: () => bridge } as unknown as BridgePool;
    const ctx: PluginContext = {
      pool,
      client: {
        lsp: { status: async () => ({ data: [] }) },
        find: { symbols: async () => ({ data: [] }) },
      },
      config: {} as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    const tools = astTools(ctx);
    const dir = "/tmp/aft-ast-test";
    const sdkCtx = createSdkContext(dir);

    await tools.ast_grep_replace.execute(
      {
        pattern: "foo($A)",
        rewrite: "bar($A)",
        lang: "javascript",
        dryRun: "true" as unknown as boolean,
      },
      sdkCtx,
    );

    expect(calls).toHaveLength(1);
    expect(calls[0]?.command).toBe("ast_replace");
    expect(calls[0]?.params.dry_run).toBe(true);
  });
});
