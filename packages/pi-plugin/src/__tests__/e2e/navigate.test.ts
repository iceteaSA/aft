/**
 * E2E coverage for aft_callgraph (6 ops).
 * Each op is dispatched as its own Rust command name (call_tree, callers,
 * trace_to, trace_to_symbol, impact, trace_data).
 */

/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHarness, type Harness, prepareBinary } from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = initialBinary.binaryPath ? describe : describe.skip;

maybeDescribe("aft_callgraph (real bridge)", () => {
  let harness: Harness;

  beforeAll(async () => {
    harness = await createHarness(initialBinary);
  });

  afterAll(async () => {
    if (harness) await harness.cleanup();
  });

  test("call_tree returns calls from a function", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "call_tree",
      filePath: "sample.ts",
      symbol: "funcB",
    });
    const text = harness.text(result);
    // funcB calls normalize — should appear in call tree
    expect(text).toContain("normalize");
  });

  test("callers finds call sites of a symbol", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "callers",
      filePath: "sample.ts",
      symbol: "normalize",
    });
    const text = harness.text(result);
    // normalize is called by funcB
    expect(text).toContain("funcB");
  });

  test("impact returns blast radius info", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "impact",
      filePath: "sample.ts",
      symbol: "funcA",
    });
    const text = harness.text(result);
    // Response is JSON with affected items list — just dispatch worked
    expect(text.length).toBeGreaterThan(0);
  });

  test("trace_to walks upward to entry points", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "trace_to",
      filePath: "sample.ts",
      symbol: "decorate",
    });
    const text = harness.text(result);
    expect(text.length).toBeGreaterThan(0);
  });

  test("trace_to_symbol returns a path between reachable symbols", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "trace_to_symbol",
      filePath: "sample.ts",
      symbol: "funcC",
      toSymbol: "decorate",
      toFile: "sample.ts",
    });
    // aft_callgraph returns flat text to the agent (structured data is carried
    // in `details` for the themed renderer). Assert on the flat output.
    const text = harness.text(result);
    const hopMatch = text.match(/(\d+) hops?/);
    expect(hopMatch).not.toBeNull();
    expect(Number(hopMatch?.[1])).toBeGreaterThanOrEqual(2);
    expect(text).toContain("funcC");
    expect(text).toContain("decorate");
    // Path order: funcC (source) appears before decorate (target).
    expect(text.indexOf("funcC")).toBeLessThan(text.indexOf("decorate"));
    expect(text).toContain("sample.ts");
  });

  test("trace_to_symbol reports no_path_found for unreachable symbols", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "trace_to_symbol",
      filePath: "sample.ts",
      symbol: "funcA",
      toSymbol: "normalize",
      toFile: "sample.ts",
    });
    // Flat output for an unreachable pair: "No path (no_path_found)".
    const text = harness.text(result);
    expect(text).toContain("No path");
    expect(text).toContain("no_path_found");
  });

  test("trace_data requires expression", async () => {
    await expect(
      harness.callTool("aft_callgraph", {
        op: "trace_data",
        filePath: "sample.ts",
        symbol: "funcA",
      }),
    ).rejects.toThrow(/expression/);
  });

  test("trace_data follows a value through scopes", async () => {
    const result = await harness.callTool("aft_callgraph", {
      op: "trace_data",
      filePath: "sample.ts",
      symbol: "funcB",
      expression: "name",
    });
    const text = harness.text(result);
    // trace_data returns JSON with flow info — dispatch success
    expect(text.length).toBeGreaterThan(0);
  });
});
