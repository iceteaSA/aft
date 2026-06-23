/**
 * Unit tests for aft_search argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { TSchema } from "typebox";
import { Value } from "typebox/value";
import { registerSemanticTool } from "../tools/semantic.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

function schemaAccepts(schema: unknown, value: unknown): boolean {
  return Value.Check(schema as TSchema, value);
}

describe("aft_search adapter", () => {
  test("maps topK and hint to bridge params and carries structured details", async () => {
    const { api, tools } = makeMockApi();
    const bridgeResponse = {
      success: true,
      text: "ready results",
      interpreted_as: "hybrid",
      semantic_status: "ready",
      more_available: true,
      engine_capped: false,
      fully_degraded: false,
      warnings: ["short_query_rerouted"],
      results: [{ file: "src/search.ts", kind: "function", source: "semantic" }],
    };
    const { bridge, calls } = makeMockBridge(() => bridgeResponse);
    registerSemanticTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_search")!, {
      query: "retry logic",
      topK: 7,
      hint: "literal",
      includeTests: true,
    })) as { content: Array<{ text: string }>; details: Record<string, unknown> };

    expect(calls[0].command).toBe("semantic_search");
    expect(calls[0].params).toEqual({
      query: "retry logic",
      top_k: 7,
      hint: "literal",
      include_tests: true,
    });
    expect(result.content[0].text).toBe("ready results");
    expect(result.details.interpreted_as).toBe("hybrid");
    expect(result.details.semantic_status).toBe("ready");
    expect(result.details.more_available).toBe(true);
    expect(result.details.engine_capped).toBe(false);
    expect(result.details.fully_degraded).toBe(false);
    expect(result.details.warnings).toEqual(["short_query_rerouted"]);
    const detailsResults = result.details.results as Array<Record<string, unknown>>;
    expect(detailsResults[0].source).toBe("semantic");
  });

  test("throws bridge failure envelopes so Pi renders them through its error path", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "semantic_search_unavailable",
      message: "Semantic search unavailable: ONNX Runtime not installed.",
    }));
    registerSemanticTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_search")!, { query: "retry logic" })).rejects.toThrow(
      "semantic_search_unavailable",
    );
  });

  test("omits top_k when topK is not provided to preserve Rust defaults", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerSemanticTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_search")!, { query: "auth flow" });

    expect(calls[0].params).toEqual({ query: "auth flow" });
  });

  test("rejects blank queries before bridge calls", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "should not call" }));
    registerSemanticTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_search")!, { query: "   " })).rejects.toThrow(
      "invalid params",
    );

    expect(calls).toEqual([]);
  });

  test("topK schema accepts only bounded integers", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerSemanticTool(api, makePluginContext(bridge));
    const schema = tools.get("aft_search")!.parameters;

    expect(schemaAccepts(schema, { query: "auth", topK: 1 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", topK: 100 })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth" })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", topK: 0 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: 101 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: 1.5 })).toBe(false);
    expect(schemaAccepts(schema, { query: "auth", topK: "10" })).toBe(false);
  });

  test("includeTests schema accepts booleans only", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge();
    registerSemanticTool(api, makePluginContext(bridge));
    const schema = tools.get("aft_search")!.parameters;

    expect(schemaAccepts(schema, { query: "auth", includeTests: true })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", includeTests: false })).toBe(true);
    expect(schemaAccepts(schema, { query: "auth", includeTests: "true" })).toBe(false);
  });
});
