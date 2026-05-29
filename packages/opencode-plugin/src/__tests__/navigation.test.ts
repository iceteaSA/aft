/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { navigationTools } from "../tools/navigation.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };

function makeMockBridge(
  handler: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse = () => ({ success: true }),
): { bridge: BinaryBridge; calls: SendCall[] } {
  const calls: SendCall[] = [];
  const bridge = {
    async send(command: string, params: Record<string, unknown>) {
      calls.push({ command, params });
      return handler(command, params);
    },
  } as unknown as BinaryBridge;
  return { bridge, calls };
}

function makePluginContext(bridge: BinaryBridge): PluginContext {
  return {
    pool: { getBridge: () => bridge } as unknown as PluginContext["pool"],
    client: {
      lsp: { status: async () => ({ data: [] }) },
      find: { symbols: async () => ({ data: [] }) },
    } as unknown as PluginContext["client"],
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-opencode-tests",
  };
}

function makeToolContext(): ToolContext {
  return {
    messageID: "message-id",
    agent: "test",
    directory: "/repo",
    worktree: "/repo",
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  } as unknown as ToolContext;
}

async function expectRejectMessage(action: () => Promise<unknown>): Promise<string> {
  try {
    await action();
  } catch (error) {
    expect(error).toBeInstanceOf(Error);
    return (error as Error).message;
  }
  throw new Error("expected action to reject");
}

describe("aft_callgraph OpenCode adapter", () => {
  test("success path dispatches to the selected op and maps filePath to file", async () => {
    const { bridge, calls } = makeMockBridge((command, params) => ({
      success: true,
      command,
      params,
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const raw = await tools.aft_callgraph.execute(
      {
        op: "impact",
        filePath: "src/app.ts",
        symbol: "run",
        depth: 4,
      },
      makeToolContext(),
    );

    expect(JSON.parse(raw)).toMatchObject({ success: true, command: "impact" });
    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({
      command: "impact",
      params: {
        file: "src/app.ts",
        symbol: "run",
        depth: 4,
      },
    });
  });

  test("trace_to_symbol ambiguous_target errors include candidates (Rust top-level shape)", async () => {
    // Rust's error_with_data() merges extras into the top-level response,
    // so production traffic has `candidates` next to `code`/`message`, NOT
    // nested under `data`.
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      candidates: [
        { file: "file1.rs", line: 42, symbol: "target" },
        { file: "file2.rs", line: 78, symbol: "target" },
      ],
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "trace_to_symbol",
          filePath: "src/app.ts",
          symbol: "run",
          toSymbol: "target",
        },
        makeToolContext(),
      ),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42\n  - file2.rs:78',
    );
  });

  test("trace_to_symbol ambiguous_target also works with nested data.candidates (forward compat)", async () => {
    // Keep parsing nested data.* shape too in case any future handler uses it.
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      data: {
        candidates: [{ file: "file1.rs", line: 42 }],
      },
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "trace_to_symbol",
          filePath: "src/app.ts",
          symbol: "run",
          toSymbol: "target",
        },
        makeToolContext(),
      ),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42',
    );
  });

  test("trace_to_symbol target_symbol_not_in_file lists alternate files", async () => {
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "target_symbol_not_in_file",
      message: "trace_to_symbol: target symbol 'foo' is not defined in toFile: wrong.rs",
      candidates: [
        { file: "right1.rs", line: 12 },
        { file: "right2.rs", line: 99 },
      ],
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "trace_to_symbol",
          filePath: "src/app.ts",
          symbol: "run",
          toSymbol: "foo",
          toFile: "wrong.rs",
        },
        makeToolContext(),
      ),
    );

    expect(message).toContain("target_symbol_not_in_file");
    expect(message).toContain("Try one of these files for toFile");
    expect(message).toContain("right1.rs:12");
    expect(message).toContain("right2.rs:99");
  });

  test("generic bridge errors keep code, message, and structured extras visible", async () => {
    // Rust's symbol_not_found also returns top-level extras (`file`, `symbol`)
    // alongside `code`/`message`, not under `data`.
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "symbol_not_found",
      message: "symbol missing",
      file: "src/app.ts",
      symbol: "run",
    }));
    const tools = navigationTools(makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      tools.aft_callgraph.execute(
        {
          op: "callers",
          filePath: "src/app.ts",
          symbol: "run",
        },
        makeToolContext(),
      ),
    );

    expect(message).toContain("callers: symbol_not_found — symbol missing");
    expect(message).toContain('"file": "src/app.ts"');
    expect(message).toContain('"symbol": "run"');
  });
});
