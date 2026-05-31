/**
 * aft_refactor — workspace-wide refactoring.
 * Ops: move (symbol across files), extract (lines → function), inline (call site).
 */

import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import {
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
  asNumber,
  asRecord,
  asRecords,
  asString,
  extractStructuredPayload,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

const RefactorParams = Type.Object({
  op: StringEnum(["move", "extract", "inline"] as const, { description: "Refactoring operation" }),
  filePath: Type.String({
    description: "Source file (absolute or relative to project root)",
  }),
  symbol: Type.Optional(Type.String({ description: "Symbol name (for move, inline)" })),
  destination: Type.Optional(Type.String({ description: "Target file (for move)" })),
  scope: Type.Optional(Type.String({ description: "Disambiguation scope for move op" })),
  name: Type.Optional(Type.String({ description: "New function name (for extract)" })),
  startLine: optionalInt(1, Number.MAX_SAFE_INTEGER),
  endLine: optionalInt(1, Number.MAX_SAFE_INTEGER),
  callSiteLine: optionalInt(1, Number.MAX_SAFE_INTEGER),
});

/** Exported for renderer unit tests. */
export function buildRefactorSections(
  args: Static<typeof RefactorParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No refactor result.")];

  if (args.op === "move") {
    const results = asRecords(response.results);
    return [
      `${theme.fg("success", "moved symbol")} ${theme.fg("toolOutput", args.symbol ?? "(symbol)")}`,
      `${theme.fg("muted", "files modified")} ${asNumber(response.files_modified) ?? results.length}`,
      `${theme.fg("muted", "consumers updated")} ${asNumber(response.consumers_updated) ?? 0}`,
      results.length > 0
        ? results
            .map((entry) => `  ↳ ${shortenPath(asString(entry.file) ?? "(unknown file)")}`)
            .join("\n")
        : theme.fg("muted", "No files reported."),
    ];
  }

  if (args.op === "extract") {
    return [
      `${theme.fg("success", "extracted")} ${theme.fg("toolOutput", asString(response.name) ?? args.name ?? "(function)")}`,
      `${theme.fg("muted", "file")} ${theme.fg("accent", shortenPath(asString(response.file) ?? args.filePath))}`,
      `${theme.fg("muted", "params")} ${Array.isArray(response.parameters) ? response.parameters.join(", ") || "none" : "none"}`,
      `${theme.fg("muted", "return type")} ${asString(response.return_type) ?? "unknown"}`,
    ];
  }

  return [
    `${theme.fg("success", "inlined")} ${theme.fg("toolOutput", asString(response.symbol) ?? args.symbol ?? "(symbol)")}`,
    `${theme.fg("muted", "file")} ${theme.fg("accent", shortenPath(asString(response.file) ?? args.filePath))}`,
    `${theme.fg("muted", "context")} ${asString(response.call_context) ?? "unknown"}`,
    `${theme.fg("muted", "substitutions")} ${asNumber(response.substitutions) ?? 0}`,
  ];
}

/** Exported for renderer unit tests. */
export function renderRefactorCall(
  args: Static<typeof RefactorParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const summary = [
    theme.fg("accent", args.op),
    accentPath(theme, args.filePath),
    args.symbol ? theme.fg("toolOutput", args.symbol) : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return renderToolCall("refactor", summary, theme, context);
}

/** Exported for renderer unit tests. */
export function renderRefactorResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof RefactorParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "refactor failed", theme, context);
  return renderSections(
    buildRefactorSections(args, extractStructuredPayload(result), theme),
    context,
  );
}

export function registerRefactorTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_refactor",
    label: "refactor",
    description:
      "Workspace-wide refactoring that updates imports and references across files. `move` relocates a top-level symbol; `extract` pulls a line range into a new function; `inline` replaces a call site. Use aft_safety checkpoint/undo before risky refactors.",
    parameters: RefactorParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof RefactorParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      const commandMap: Record<string, string> = {
        move: "move_symbol",
        extract: "extract_function",
        inline: "inline_symbol",
      };
      // Per-op required-field validation using isEmptyParam so empty strings
      // ("") sent by GPT-family models trigger the proper "required" error
      // instead of being passed through to Rust as a valid empty value.
      if ((params.op === "move" || params.op === "inline") && isEmptyParam(params.symbol)) {
        throw new Error(`'symbol' is required for '${params.op}' op`);
      }
      if (params.op === "move" && isEmptyParam(params.destination)) {
        throw new Error("'destination' is required for 'move' op");
      }
      if (params.op === "extract" && isEmptyParam(params.name)) {
        throw new Error("'name' is required for 'extract' op");
      }

      const filePath = await resolvePathArg(extCtx.cwd, params.filePath);
      const destination = !isEmptyParam(params.destination)
        ? await resolvePathArg(extCtx.cwd, params.destination as string)
        : undefined;
      const permissionTargets =
        params.op === "move" && destination !== undefined ? [filePath, destination] : [filePath];
      const checked = new Set<string>();
      for (const target of permissionTargets) {
        if (checked.has(target)) continue;
        checked.add(target);
        await assertExternalDirectoryPermission(extCtx, target, "modify", {
          restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
        });
      }

      const bridge = bridgeFor(ctx, extCtx.cwd);
      const req: Record<string, unknown> = { file: filePath };
      // Use isEmptyParam everywhere so "" / [] / null don't slip through as
      // valid string params that Rust then has to deal with.
      if (!isEmptyParam(params.symbol)) req.symbol = params.symbol;
      if (destination !== undefined) req.destination = destination;
      if (!isEmptyParam(params.scope)) req.scope = params.scope;
      if (!isEmptyParam(params.name)) req.name = params.name;
      const startLine = coerceOptionalInt(
        params.startLine,
        "startLine",
        1,
        Number.MAX_SAFE_INTEGER,
      );
      const endLine = coerceOptionalInt(params.endLine, "endLine", 1, Number.MAX_SAFE_INTEGER);
      const callSiteLine = coerceOptionalInt(
        params.callSiteLine,
        "callSiteLine",
        1,
        Number.MAX_SAFE_INTEGER,
      );
      if (startLine !== undefined) req.start_line = startLine;
      // Agent uses inclusive end_line; Rust extract_function expects exclusive.
      if (endLine !== undefined) {
        req.end_line = params.op === "extract" ? endLine + 1 : endLine;
      }
      if (callSiteLine !== undefined) req.call_site_line = callSiteLine;
      const response = await callBridge(bridge, commandMap[params.op], req, extCtx);
      return textResult(JSON.stringify(response, null, 2));
    },
    renderCall(args, theme, context) {
      return renderRefactorCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderRefactorResult(result, context.args, theme, context);
    },
  });
}
