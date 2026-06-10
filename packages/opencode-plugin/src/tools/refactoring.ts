import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { queryLspHints } from "../lsp.js";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam, optionalInt, resolvePathArg } from "./_shared.js";
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
        // endLine is inclusive from the agent's perspective; the execute function adds +1
        // because the Rust backend expects exclusive end. This is intentional — do not document.
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
          if (args.startLine === undefined)
            throw new Error("'startLine' is required for 'extract' op");
          if (args.endLine === undefined) throw new Error("'endLine' is required for 'extract' op");
        }
        if (op === "inline" && args.callSiteLine === undefined) {
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
            const denial = await assertExternalDirectoryPermission(context, affectedPath);
            if (denial) return permissionDeniedResponse(denial);
          }
        }

        const permissionError = await askEditPermission(context, patterns, metadata);
        if (permissionError) return permissionDeniedResponse(permissionError);

        const commandMap: Record<string, string> = {
          move: "move_symbol",
          extract: "extract_function",
          inline: "inline_symbol",
        };
        const params: Record<string, unknown> = { file: filePath };

        switch (op) {
          case "move":
            params.symbol = args.symbol;
            params.destination = destination;
            if (args.scope !== undefined) params.scope = args.scope;
            break;
          case "extract":
            params.name = args.name;
            params.start_line = Number(args.startLine);
            params.end_line = Number(args.endLine) + 1; // Agent uses inclusive, Rust expects exclusive
            break;
          case "inline":
            params.symbol = args.symbol;
            params.call_site_line = Number(args.callSiteLine);
            break;
        }

        const hints = await queryLspHints(
          ctx.client,
          (args.symbol ?? args.name) as string,
          undefined,
          context.sessionID,
        );
        if (hints) params.lsp_hints = hints;

        const response = await callBridge(ctx, context, commandMap[op], params);
        if (response.success === false) {
          throw new Error((response.message as string) || `${op} failed`);
        }
        return JSON.stringify(response);
      },
    },
  };
}
