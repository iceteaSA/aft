import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, formatBridgeErrorMessage, optionalInt } from "./_shared.js";

const z = tool.schema;

/**
 * Tool definitions for navigation commands: configure, call_tree, callers, trace_to, trace_to_symbol, impact, and trace_data.
 */
export function navigationTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_navigate: {
      description:
        "Navigate code structure across files using call graph analysis.\n\n" +
        "Ops:\n" +
        "- 'call_tree': See what a function calls (forward traversal). Use to understand dependencies before modifying a function.\n" +
        "- 'callers': Find all call sites of a symbol. Use before renaming or changing a function's signature.\n" +
        "- 'trace_to': Trace how execution reaches a function from entry points (routes, exports, main). Use to understand context around deeply-nested code.\n" +
        "- 'trace_to_symbol': Find the shortest call path from one symbol to another symbol across the codebase. Requires 'toSymbol'. If multiple targets match, the error returns candidate files; retry with 'toFile' to disambiguate.\n" +
        "- 'impact': Analyze what breaks if a symbol changes. Returns affected callers with signatures and entry point status.\n" +
        "- 'trace_data': Follow a value through variable assignments and function parameters across files. Requires 'symbol' (scope to trace from) and 'expression'.\n\n" +
        "All ops require both 'filePath' and 'symbol'. The 'expression' parameter is additionally required for trace_data; 'toSymbol' is additionally required for trace_to_symbol.\n\n",
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z
          .enum(["call_tree", "callers", "trace_to", "trace_to_symbol", "impact", "trace_data"])
          .describe("Navigation operation"),
        filePath: z
          .string()
          .describe(
            "Path to the source file containing the symbol (absolute or relative to project root)",
          ),
        symbol: z.string().describe("Name of the symbol to analyze"),
        depth: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Max traversal depth (default: call_tree=5, callers=1, trace_to=10, trace_to_symbol=10 capped at 16, impact=5, trace_data=5)",
        ),
        expression: z
          .string()
          .optional()
          .describe("Expression to track through data flow (required for trace_data op)"),
        toSymbol: z
          .string()
          .optional()
          .describe(
            "Target symbol name for trace_to_symbol; the returned path ends at this symbol",
          ),
        toFile: z
          .string()
          .optional()
          .describe(
            "Optional target file for trace_to_symbol; required when toSymbol exists in multiple files",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const params: Record<string, unknown> = {
          file: args.filePath,
          symbol: args.symbol,
        };
        if (args.depth !== undefined) params.depth = Number(args.depth);
        if (args.expression !== undefined) params.expression = args.expression;
        if (args.toSymbol !== undefined) params.toSymbol = args.toSymbol;
        if (args.toFile !== undefined) params.toFile = args.toFile;
        if (args.op === "trace_data" && typeof args.expression !== "string") {
          throw new Error("'expression' is required for 'trace_data' op");
        }
        if (args.op === "trace_to_symbol" && typeof args.toSymbol !== "string") {
          throw new Error("'toSymbol' is required for 'trace_to_symbol' op");
        }
        const response = await callBridge(ctx, context, args.op as string, params);
        if (response.success === false) {
          throw new Error(formatBridgeErrorMessage(args.op as string, response, params));
        }
        return JSON.stringify(response);
      },
    },
  };
}
