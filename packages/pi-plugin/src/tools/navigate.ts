/**
 * aft_callgraph — call-graph relationship queries across files.
 * Ops: call_tree, callers, trace_to, trace_to_symbol, impact, trace_data.
 */

import { formatCallgraphSections, PLAIN_CALLGRAPH_THEME } from "@cortexkit/aft-bridge";
import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import {
  BridgeError,
  bridgeFor,
  callBridge,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  textResult,
} from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
  extractStructuredPayload,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

// Read-only navigation negatives that are legitimate answers, not failures:
// the symbol isn't defined here, or the store is still building. Returned as
// plain text (no red error), matching how grep-with-no-matches reads. Mirrors
// the OpenCode plugin's set. ("no path between symbols" is already a success
// response with reason=no_path_found, never an error code.)
const CALLGRAPH_SOFT_CODES = new Set(["symbol_not_found", "callgraph_building"]);

function navigateParamsSchema() {
  return Type.Object({
    op: StringEnum(
      ["call_tree", "callers", "trace_to", "trace_to_symbol", "impact", "trace_data"] as const,
      {
        description: "Navigation operation",
      },
    ),
    filePath: Type.String({
      description: "Source file containing the symbol (absolute or relative to project root)",
    }),
    symbol: Type.String({ description: "Name of the symbol to analyze" }),
    depth: optionalInt(1, Number.MAX_SAFE_INTEGER),
    expression: Type.Optional(
      Type.String({ description: "Expression to track (required for trace_data)" }),
    ),
    toSymbol: Type.Optional(
      Type.String({
        description: "Target symbol for trace_to_symbol; the returned path ends here",
      }),
    ),
    toFile: Type.Optional(
      Type.String({
        description:
          "Optional target file for trace_to_symbol; required when toSymbol exists in multiple files",
      }),
    ),
  });
}

type NavigateArgs = Static<ReturnType<typeof navigateParamsSchema>>;

/** Exported for renderer unit tests. */
export function buildNavigateSections(
  args: NavigateArgs,
  payload: unknown,
  theme: Theme,
): string[] {
  const themeAdapter = {
    fg: (role: string, s: string) => theme.fg(role as Parameters<Theme["fg"]>[0], s),
  };
  return formatCallgraphSections(args.op, payload, themeAdapter);
}

/** Exported for renderer unit tests. */
export function renderNavigateCall(args: NavigateArgs, theme: Theme, context: RenderContextLike) {
  const summary = [
    theme.fg("accent", args.op),
    accentPath(theme, args.filePath),
    theme.fg("toolOutput", args.symbol),
    args.toSymbol ? theme.fg("toolOutput", `→ ${args.toSymbol}`) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("callgraph", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderNavigateResult(
  result: AgentToolResult<unknown>,
  args: NavigateArgs,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "navigate failed", theme, context);
  return renderSections(
    buildNavigateSections(args, extractStructuredPayload(result), theme),
    context,
  );
}

export function registerNavigateTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_callgraph",
    label: "callgraph",
    description:
      "Answer code-relationship questions from a real call graph — instead of grep + read chains. Reach for this whenever the question is about how symbols connect. All ops require both `filePath` and `symbol`. Use `callers` for call sites (before renaming/signature changes), `impact` for blast radius (what breaks if a symbol changes), `call_tree` for what a function calls, `trace_to` for how execution reaches a symbol from entry points, `trace_to_symbol` for the shortest path from one symbol to another (requires `toSymbol`; if ambiguous, the error returns candidate files — retry with `toFile`), `trace_data` to follow a value across assignments/params.",
    parameters: navigateParamsSchema(),
    async execute(_toolCallId: string, params: NavigateArgs, _signal, _onUpdate, extCtx) {
      if (isEmptyParam(params.filePath)) {
        throw new Error(`op='${params.op}' requires a \`filePath\``);
      }
      if (isEmptyParam(params.symbol)) {
        throw new Error(`op='${params.op}' requires a \`symbol\``);
      }
      if (params.op === "trace_data" && isEmptyParam(params.expression)) {
        throw new Error("op='trace_data' requires an `expression`");
      }
      if (params.op === "trace_to_symbol" && isEmptyParam(params.toSymbol)) {
        throw new Error("op='trace_to_symbol' requires a `toSymbol`");
      }
      const filePath = await resolvePathArg(extCtx.cwd, params.filePath);
      const toFile = !isEmptyParam(params.toFile)
        ? await resolvePathArg(extCtx.cwd, params.toFile as string)
        : undefined;
      const checked = new Set<string>();
      for (const target of [filePath, ...(toFile !== undefined ? [toFile] : [])]) {
        if (checked.has(target)) continue;
        checked.add(target);
        await assertExternalDirectoryPermission(extCtx, target, "read", {
          restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
        });
      }

      const bridge = bridgeFor(ctx, extCtx.cwd);
      const req: Record<string, unknown> = {
        op: params.op,
        file: filePath,
        symbol: params.symbol,
      };
      const depth = coerceOptionalInt(params.depth, "depth", 1, Number.MAX_SAFE_INTEGER);
      if (depth !== undefined) req.depth = depth;
      if (!isEmptyParam(params.expression)) req.expression = params.expression;
      if (!isEmptyParam(params.toSymbol)) req.toSymbol = params.toSymbol;
      if (toFile !== undefined) req.toFile = toFile;
      try {
        const response = await callBridge(bridge, params.op, req, extCtx);
        return textResult(
          formatCallgraphSections(params.op, response, PLAIN_CALLGRAPH_THEME).join("\n"),
          response,
        );
      } catch (error) {
        // Read-only navigation negatives ("symbol isn't here", "no path between
        // them", "store still building") are legitimate answers, not failures —
        // return them as a plain (non-error) result so the UI doesn't render red.
        // Genuine errors (invalid_request, boundary violations, unknown codes)
        // keep throwing so they surface as errors.
        if (error instanceof BridgeError && CALLGRAPH_SOFT_CODES.has(error.code)) {
          return textResult(error.message);
        }
        throw error;
      }
    },
    renderCall(args, theme, context) {
      return renderNavigateCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderNavigateResult(result, context.args, theme, context);
    },
  });
}
