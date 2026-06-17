/**
 * Unit tests for aft_callgraph argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { registerNavigateTool } from "../tools/navigate.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

async function expectRejectMessage(action: () => Promise<unknown>): Promise<string> {
  try {
    await action();
  } catch (error) {
    expect(error).toBeInstanceOf(Error);
    return (error as Error).message;
  }
  throw new Error("expected action to reject");
}

describe("aft_callgraph adapter", () => {
  test("dispatches to the selected op and maps filePath to file", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, total_affected: 0 }));
    registerNavigateTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_callgraph")!, {
      op: "impact",
      filePath: "src/app.ts",
      symbol: "run",
      depth: 4,
    })) as {
      content: Array<{ type: string; text: string }>;
      details?: Record<string, unknown>;
    };

    expect(result.content[0]?.text).toContain("affected call site");
    expect(result.content[0]?.text).not.toContain('"success"');
    expect(result.details).toMatchObject({ success: true, total_affected: 0 });
    expect(calls[0].command).toBe("impact");
    expect(calls[0].params).toEqual({
      op: "impact",
      file: "src/app.ts",
      symbol: "run",
      depth: 4,
    });
  });

  test("trace_data requires expression before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerNavigateTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_data",
        filePath: "src/app.ts",
        symbol: "run",
      }),
    ).rejects.toThrow("requires an `expression`");
    expect(calls).toHaveLength(0);
  });

  test("trace_data forwards expression when present", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerNavigateTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_callgraph")!, {
      op: "trace_data",
      filePath: "src/app.ts",
      symbol: "run",
      expression: "config.apiKey",
    });

    expect(calls[0].command).toBe("trace_data");
    expect(calls[0].params).toMatchObject({ expression: "config.apiKey" });
  });

  test("trace_to_symbol requires and forwards target fields", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerNavigateTool(api, makePluginContext(bridge));

    await expect(
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
      }),
    ).rejects.toThrow("toSymbol");

    await executeTool(tools.get("aft_callgraph")!, {
      op: "trace_to_symbol",
      filePath: "src/app.ts",
      symbol: "run",
      toSymbol: "target",
      toFile: "src/target.ts",
      depth: 3,
    });

    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("trace_to_symbol");
    expect(calls[0].params).toMatchObject({
      toSymbol: "target",
      toFile: "src/target.ts",
      depth: 3,
    });
  });

  test("trace_to_symbol ambiguous_target errors include candidates (Rust top-level shape)", async () => {
    // Rust's error_with_data() merges extras into the top-level response,
    // so production traffic has `candidates` next to `code`/`message`, NOT
    // nested under `data`.
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      candidates: [
        { file: "file1.rs", line: 42, symbol: "target" },
        { file: "file2.rs", line: 78, symbol: "target" },
      ],
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
        toSymbol: "target",
      }),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42\n  - file2.rs:78',
    );
  });

  test("trace_to_symbol ambiguous_target also works with nested data.candidates (forward compat)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "ambiguous_target",
      message: 'multiple symbols named "target"',
      data: {
        candidates: [{ file: "file1.rs", line: 42 }],
      },
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
        toSymbol: "target",
      }),
    );

    expect(message).toBe(
      'trace_to_symbol: ambiguous_target — multiple symbols named "target". Pass toFile to disambiguate:\n  - file1.rs:42',
    );
  });

  test("trace_to_symbol target_symbol_not_in_file lists alternate files", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "target_symbol_not_in_file",
      message: "trace_to_symbol: target symbol 'foo' is not defined in toFile: wrong.rs",
      candidates: [
        { file: "right1.rs", line: 12 },
        { file: "right2.rs", line: 99 },
      ],
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "trace_to_symbol",
        filePath: "src/app.ts",
        symbol: "run",
        toSymbol: "foo",
        toFile: "wrong.rs",
      }),
    );

    expect(message).toContain("target_symbol_not_in_file");
    expect(message).toContain("Try one of these files for toFile");
    expect(message).toContain("right1.rs:12");
    expect(message).toContain("right2.rs:99");
  });

  test("generic bridge errors keep code, message, and structured extras visible", async () => {
    // A genuine error code (not a soft negative) still throws so it renders as
    // an error, with code/message plus any top-level structured extras visible.
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: false,
      code: "internal",
      message: "resolver crashed",
      file: "src/app.ts",
      symbol: "run",
    }));
    registerNavigateTool(api, makePluginContext(bridge));

    const message = await expectRejectMessage(() =>
      executeTool(tools.get("aft_callgraph")!, {
        op: "callers",
        filePath: "src/app.ts",
        symbol: "run",
      }),
    );

    expect(message).toContain("callers: internal — resolver crashed");
    expect(message).toContain('"file": "src/app.ts"');
    expect(message).toContain('"symbol": "run"');
  });

  test("read-only negatives (symbol_not_found, callgraph_building) return text, not an error", async () => {
    // These are legitimate "no result" / "retry shortly" answers from a
    // read-only query tool — they must NOT throw (which would render red in the
    // host UI). The agent still gets the full honest message as the result.
    for (const code of ["symbol_not_found", "callgraph_building"]) {
      const { api, tools } = makeMockApi();
      const { bridge } = makeMockBridge(() => ({
        success: false,
        code,
        message: `${code} happened`,
        file: "src/app.ts",
        symbol: "run",
      }));
      registerNavigateTool(api, makePluginContext(bridge));

      const result = (await executeTool(tools.get("aft_callgraph")!, {
        op: "callers",
        filePath: "src/app.ts",
        symbol: "run",
      })) as { content: Array<{ type: string; text: string }> };

      expect(result.content[0]?.text).toContain(`callers: ${code} — ${code} happened`);
    }
  });

  test("returns flat text and structured details for themed render", async () => {
    const { api, tools } = makeMockApi();
    const payload = {
      success: true,
      total_callers: 2,
      callers: [
        {
          file: "src/a.ts",
          callers: [
            { symbol: "fn", line: 10 },
            { symbol: "fn", line: 20 },
          ],
        },
      ],
    };
    const { bridge } = makeMockBridge(() => payload);
    registerNavigateTool(api, makePluginContext(bridge));

    const result = (await executeTool(tools.get("aft_callgraph")!, {
      op: "callers",
      filePath: "src/a.ts",
      symbol: "target",
    })) as {
      content: Array<{ type: string; text: string }>;
      details?: Record<string, unknown>;
    };

    expect(result.content[0]?.text).toContain("2 callers");
    expect(result.content[0]?.text).toContain("↳ fn:10, 20");
    expect(result.details).toEqual(payload);
  });
});
