/// <reference path="../bun-test.d.ts" />
import { describe, expect, mock, test } from "bun:test";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { semanticTools } from "../tools/semantic.js";
import type { PluginContext } from "../types.js";
import { mockAsk, mockAskDeny, noopAsk } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type BridgeCall = { projectRoot: string };

function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
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

function createMockSdkContext(directory = "/tmp/semantic-tests", ask = noopAsk): ToolContext {
  return {
    sessionID: "semantic-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function createMockSemanticHarness(
  config: Record<string, unknown>,
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const bridgeCalls: BridgeCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
    },
  };

  const pool = {
    getBridge: (projectRoot: string) => {
      bridgeCalls.push({ projectRoot });
      return bridge;
    },
  } as unknown as BridgePool;

  return {
    bridgeCalls,
    sendCalls,
    tools: semanticTools(createPluginContext(pool, config)),
  };
}

function parseStructuredOutput(output: string): Record<string, unknown> {
  const marker = "\n\nStructured response:\n";
  const json = output.includes(marker)
    ? output.slice(output.indexOf(marker) + marker.length)
    : output;
  return JSON.parse(json) as Record<string, unknown>;
}

describe("semanticTools", () => {
  test("registers aft_search", () => {
    const { tools } = createMockSemanticHarness({}, () => ({ success: true }));

    expect(Object.keys(tools)).toEqual(["aft_search"]);
  });

  test("returns text plus structured response and sends semantic_search params", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const bridgeResponse = {
      success: true,
      text: "src/auth.ts\nvalidateToken [function] lines 10-32 score 0.913",
      interpreted_as: "hybrid",
      semantic_status: "ready",
      more_available: true,
      engine_capped: false,
      fully_degraded: false,
      warnings: ["short_query_rerouted"],
      results: [
        {
          file: "src/auth.ts",
          name: "validateToken",
          kind: "function",
          source: "hybrid",
          score: 0.913,
        },
      ],
    };
    const { bridgeCalls, sendCalls, tools } = createMockSemanticHarness({}, () => bridgeResponse);

    const output = await tools.aft_search.execute(
      { query: "authentication logic", topK: 5 },
      sdkCtx,
    );

    // Bridge now keyed by project root only; the session lives in params via callBridge helper.
    expect(bridgeCalls.length).toBe(1);
    expect(sendCalls).toEqual([
      {
        command: "semantic_search",
        params: {
          query: "authentication logic",
          top_k: 5,
          session_id: "semantic-session",
        },
      },
    ]);
    expect(output).toContain(bridgeResponse.text);
    expect(output).toContain("Search status: more results available.");
    const structured = parseStructuredOutput(output);
    expect(structured.interpreted_as).toBe("hybrid");
    expect(structured.semantic_status).toBe("ready");
    expect(structured.more_available).toBe(true);
    expect(structured.engine_capped).toBe(false);
    expect(structured.fully_degraded).toBe(false);
    expect(structured.warnings).toEqual(["short_query_rerouted"]);
    const results = structured.results as Array<Record<string, unknown>>;
    expect(results[0].source).toBe("hybrid");
  });

  test("rejects blank queries before permission or bridge calls", async () => {
    const ask = mockAsk();
    const sdkCtx = createMockSdkContext("/tmp/project", ask);
    const sendImpl = mock(() => ({ success: true, text: "should not call" }));
    const { sendCalls, tools } = createMockSemanticHarness({}, sendImpl);

    await expect(tools.aft_search.execute({ query: "   " }, sdkCtx)).rejects.toThrow(
      "invalid params",
    );

    expect(ask).not.toHaveBeenCalled();
    expect(sendCalls).toEqual([]);
    expect(sendImpl).not.toHaveBeenCalled();
  });

  test("renders all semantic honesty flags when text is present", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const { tools } = createMockSemanticHarness({}, () => ({
      success: true,
      text: "partial results",
      more_available: true,
      engine_capped: true,
      fully_degraded: true,
      complete: false,
      results: [],
    }));

    const output = await tools.aft_search.execute({ query: "auth", topK: 5 }, sdkCtx);

    expect(output).toContain(
      "Search status: more results available; enumeration capped; fully degraded; partial/incomplete.",
    );
    const structured = parseStructuredOutput(output);
    expect(structured.complete).toBe(false);
  });

  test("throws semantic runtime errors with code and message", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const { tools } = createMockSemanticHarness({}, () => ({
      success: false,
      code: "semantic_search_unavailable",
      message: "Semantic search unavailable: ONNX Runtime not installed.",
    }));

    await expect(
      tools.aft_search.execute({ query: "authentication logic", topK: 5 }, sdkCtx),
    ).rejects.toThrow(
      "semantic_search: semantic_search_unavailable — Semantic search unavailable: ONNX Runtime not installed.",
    );
  });

  test("throws bridge failure envelopes with their message", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const { tools } = createMockSemanticHarness({}, () => ({
      success: false,
      code: "permission_required",
      message: "grep permission required",
    }));

    await expect(tools.aft_search.execute({ query: "TODO", topK: 5 }, sdkCtx)).rejects.toThrow(
      "semantic_search: permission_required — grep permission required",
    );
  });

  test("asks grep permission for regex literal and auto hints but not semantic", async () => {
    for (const hint of ["regex", "literal", "auto"] as const) {
      const ask = mockAsk();
      const sdkCtx = createMockSdkContext("/tmp/project", ask);
      const { sendCalls, tools } = createMockSemanticHarness({}, () => ({
        success: true,
        text: "ok",
      }));

      await tools.aft_search.execute({ query: "TODO", hint }, sdkCtx);

      expect(ask).toHaveBeenCalledTimes(1);
      expect(sendCalls[0].params.hint).toBe(hint);
    }

    const semanticAsk = mockAsk();
    const semanticCtx = createMockSdkContext("/tmp/project", semanticAsk);
    const { sendCalls, tools } = createMockSemanticHarness({}, () => ({
      success: true,
      text: "ok",
    }));

    await tools.aft_search.execute({ query: "auth flow", hint: "semantic" }, semanticCtx);

    expect(semanticAsk).not.toHaveBeenCalled();
    expect(sendCalls[0].params.hint).toBe("semantic");
  });

  test("permission denied returns an error envelope without bridge call", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project", mockAskDeny("Denied by policy"));
    const sendImpl = mock(() => ({ success: true, text: "should not call" }));
    const { sendCalls, tools } = createMockSemanticHarness({}, sendImpl);

    const output = await tools.aft_search.execute({ query: "TODO", hint: "literal" }, sdkCtx);

    expect(sendCalls).toEqual([]);
    expect(sendImpl).not.toHaveBeenCalled();
    expect(output).toContain("permission_denied");
    expect(output).toContain("Denied by policy");
  });
});
