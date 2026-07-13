/**
 * Unit tests for AST tool argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { astTools } from "../tools/ast.js";
import type { PluginContext } from "../types.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

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
  test('ast_grep_replace treats string dryRun "true" as preview and forwards raw dryRun', async () => {
    const calls: Array<{ name: string; rawArgs: Record<string, unknown> }> = [];
    const bridge = {
      toolCall: async (
        _sessionId: string | undefined,
        name: string,
        rawArgs: Record<string, unknown> = {},
      ) => {
        calls.push({ name, rawArgs });
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
    const sdkCtx = createSdkContext(projectRoot);

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
    expect(calls[0]?.name).toBe("ast_replace");
    expect(calls[0]?.rawArgs.dryRun).toBe("true");
    expect(calls[0]?.rawArgs).not.toHaveProperty("dry_run");
  });
});
