import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { queryLspHints } from "../lsp.js";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  resolvePathArg,
} from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveRelativePattern,
  resolveRelativePatterns,
  workspacePattern,
} from "./permissions.js";

const z = tool.schema;

/**
 * Tool definitions for refactoring commands: move_symbol, extract_function, inline_symbol.
 */
export function refactoringTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_refactor: {
      // Per-op parameter requirements live on the param descriptions — the
      // tool description names the ops and their one defining behavior each.
      description:
        "Workspace-wide refactoring that updates imports and references across files.\n\n" +
        "Ops:\n" +
        "- 'move': move a top-level symbol (not nested functions or class methods) to another file, rewriting imports workspace-wide. A checkpoint is created first. To move/rename a whole file, use aft_move.\n" +
        "- 'extract': extract a line range into a new function with auto-detected parameters (TS/JS/TSX, Python).\n" +
        "- 'inline': replace a function call with the function's body.",
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z.enum(["move", "extract", "inline"]).describe("Refactoring operation"),
        filePath: z
          .string()
          .describe("Path to the source file (absolute or relative to project root)"),
        symbol: z
          .string()
          .optional()
          .describe("Symbol name — required for 'move' and 'inline' ops"),
        // move
        destination: z.string().optional().describe("Target file path — required for 'move' op"),
        // scope disambiguates overloaded top-level names, NOT nested symbols.
        // "Only works on top-level exports" in the description is correct — scope selects
        // among multiple top-level symbols that share a name, not class methods.
        scope: z
          .string()
          .optional()
          .describe(
            "Disambiguation scope for 'move' op — when multiple top-level symbols share the same name, specify the containing scope to disambiguate (e.g. 'MyClass'). Does NOT enable access to nested symbols or class methods.",
          ),
        // extract
        name: z.string().optional().describe("New function name — required for 'extract' op"),
        startLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "1-based start line — required for 'extract' op",
        ),
        // The endLine value supplied to this tool is inclusive; the lower-level
        // extract implementation expects the line immediately after the range.
        endLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "1-based end line (inclusive) — required for 'extract' op",
        ),
        // inline
        callSiteLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "1-based call site line — required for 'inline' op",
        ),
      },
      execute: async (args, context): Promise<string> => {
        const op = args.op as string;
        const startLine = coerceOptionalInt(
          args.startLine,
          "startLine",
          1,
          Number.MAX_SAFE_INTEGER,
        );
        const endLine = coerceOptionalInt(args.endLine, "endLine", 1, Number.MAX_SAFE_INTEGER);
        const callSiteLine = coerceOptionalInt(
          args.callSiteLine,
          "callSiteLine",
          1,
          Number.MAX_SAFE_INTEGER,
        );

        // Use isEmptyParam so empty strings (GPT-family models send "" for omitted
        // required string params) trigger the proper "required" error instead of
        // being silently accepted as a string and crashing downstream.
        if ((op === "move" || op === "inline") && isEmptyParam(args.symbol)) {
          throw new Error(`'symbol' is required for '${op}' op`);
        }
        if (op === "move" && isEmptyParam(args.destination)) {
          throw new Error("'destination' is required for 'move' op");
        }
        if (op === "extract") {
          if (isEmptyParam(args.name)) throw new Error("'name' is required for 'extract' op");
          if (startLine === undefined) throw new Error("'startLine' is required for 'extract' op");
          if (endLine === undefined) throw new Error("'endLine' is required for 'extract' op");
        }
        if (op === "inline" && callSiteLine === undefined) {
          throw new Error("'callSiteLine' is required for 'inline' op");
        }

        const filePath = await resolvePathArg(ctx, context, args.filePath as string);
        const destination =
          op === "move"
            ? await resolvePathArg(ctx, context, args.destination as string)
            : undefined;
        const patterns =
          op === "move"
            ? resolveRelativePatterns(context, [
                workspacePattern(context),
                filePath,
                ...(destination !== undefined ? [destination] : []),
              ])
            : [resolveRelativePattern(context, filePath)];
        const metadata = patterns.length === 1 ? { filepath: filePath } : {};

        // External-directory check first (mirrors opencode-native edit.ts:68).
        {
          const affectedPaths =
            op === "move" && destination !== undefined ? [filePath, destination] : [filePath];
          const asked = new Set<string>();
          for (const affectedPath of affectedPaths) {
            if (asked.has(affectedPath)) continue;
            asked.add(affectedPath);
            const denial = await assertExternalDirectoryPermission(ctx, context, affectedPath);
            if (denial) return permissionDeniedResponse(denial);
          }
        }

        const permissionError = await askEditPermission(context, patterns, metadata);
        if (permissionError) return permissionDeniedResponse(permissionError);

        const rawArgs: Record<string, unknown> = { op, filePath };

        switch (op) {
          case "move":
            rawArgs.symbol = args.symbol;
            rawArgs.destination = destination;
            if (args.scope !== undefined) rawArgs.scope = args.scope;
            break;
          case "extract":
            if (startLine === undefined || endLine === undefined) {
              throw new Error("'startLine' and 'endLine' are required for 'extract' op");
            }
            rawArgs.name = args.name;
            // Forward the inclusive endLine value directly; the receiving
            // implementation adds 1 to make the exclusive end line it needs.
            rawArgs.startLine = startLine;
            rawArgs.endLine = endLine;
            break;
          case "inline":
            rawArgs.symbol = args.symbol;
            rawArgs.callSiteLine = callSiteLine;
            break;
        }

        const hints = await queryLspHints(
          ctx.client,
          (args.symbol ?? args.name) as string,
          undefined,
          context.sessionID,
        );
        if (hints) rawArgs.lsp_hints = hints;

        const response = await callToolCall(ctx, context, "aft_refactor", rawArgs);
        if (response.success === false) {
          throw new Error((response.message as string) || `${op} failed`);
        }
        return response.text;
      },
    },
  };
}
