/**
 * aft_navigate — call-graph navigation across files.
 * Ops: call_tree, callers, trace_to, trace_to_symbol, impact, trace_data.
 */

import { StringEnum } from "@earendil-works/pi-ai";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, coerceOptionalInt, optionalInt, textResult } from "./_shared.js";
import {
  accentPath,
  asBoolean,
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

function navigateParamsSchema() {
  return Type.Object({
    op: StringEnum(
      ["call_tree", "callers", "trace_to", "trace_to_symbol", "impact", "trace_data"] as const,
      {
        description: "Navigation operation",
      },
    ),
    filePath: Type.String({ description: "Source file containing the symbol" }),
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

function treeLine(depth: number, text: string): string {
  return `${"  ".repeat(depth)}${depth === 0 ? "" : "↳ "}${text}`;
}

function renderCallTreeNode(node: Record<string, unknown>, depth: number, lines: string[]): void {
  const name = asString(node.name) ?? "(unknown)";
  const file = shortenPath(asString(node.file) ?? "(unknown file)");
  const line = asNumber(node.line);
  lines.push(treeLine(depth, `${name} ${line !== undefined ? `[${file}:${line}]` : `[${file}]`}`));
  asRecords(node.children).forEach((child) => {
    renderCallTreeNode(child, depth + 1, lines);
  });
}

function depthWarning(
  response: Record<string, unknown>,
  theme: Theme,
  depthField = "depth_limited",
  truncatedField = "truncated",
): string {
  const limited = asBoolean(response[depthField]);
  const truncated = asNumber(response[truncatedField]) ?? 0;
  if (!limited && truncated === 0) return "";
  const detail = truncated > 0 ? `, ${truncated} truncated` : "";
  return theme.fg("warning", `(depth limited${detail})`);
}

function renderTracePath(path: Record<string, unknown>, index: number, lines: string[]): void {
  lines.push(`Path ${index + 1}`);
  asRecords(path.hops).forEach((hop, hopIndex) => {
    const symbol = asString(hop.symbol) ?? "(unknown)";
    const file = shortenPath(asString(hop.file) ?? "(unknown file)");
    const line = asNumber(hop.line);
    const entry = hop.is_entry_point === true ? " [entry]" : "";
    lines.push(
      treeLine(
        hopIndex + 1,
        `${symbol}${entry} ${line !== undefined ? `[${file}:${line}]` : `[${file}]`}`,
      ),
    );
  });
}

/** Exported for renderer unit tests. */
export function buildNavigateSections(
  args: NavigateArgs,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No navigation result.")];

  if (args.op === "call_tree") {
    const lines: string[] = [];
    renderCallTreeNode(response, 0, lines);
    const warning = depthWarning(response, theme);
    if (warning) lines.push(warning);
    return lines.length > 0 ? lines : [theme.fg("muted", "No call tree available.")];
  }

  if (args.op === "callers") {
    const groups = asRecords(response.callers);
    const warning = depthWarning(response, theme);
    const sections = [
      `${theme.fg("success", `${asNumber(response.total_callers) ?? 0} caller${(asNumber(response.total_callers) ?? 0) === 1 ? "" : "s"}`)} ${theme.fg("muted", `${groups.length} file group${groups.length === 1 ? "" : "s"}`)} ${warning}`.trim(),
    ];
    groups.forEach((group) => {
      const file = shortenPath(asString(group.file) ?? "(unknown file)");
      const lines = [theme.fg("accent", file)];
      asRecords(group.callers).forEach((caller) => {
        lines.push(
          `  ↳ ${asString(caller.symbol) ?? "(unknown)"} ${theme.fg("muted", `line ${asNumber(caller.line) ?? "?"}`)}`,
        );
      });
      sections.push(lines.join("\n"));
    });
    return sections;
  }

  if (args.op === "trace_to_symbol") {
    const path = asRecords(response.path);
    const complete = asBoolean(response.complete);
    const reason = asString(response.reason);
    if (path.length === 0) {
      const prefix =
        complete === false ? theme.fg("warning", "No complete path") : theme.fg("muted", "No path");
      return [`${prefix}${reason ? ` (${reason})` : ""}`];
    }
    const lines = [theme.fg("success", `${path.length} hop${path.length === 1 ? "" : "s"}`)];
    path.forEach((hop, index) => {
      const symbol = asString(hop.symbol) ?? "(unknown)";
      const file = shortenPath(asString(hop.file) ?? "(unknown file)");
      const line = asNumber(hop.line);
      lines.push(
        treeLine(index + 1, `${symbol} ${line !== undefined ? `[${file}:${line}]` : `[${file}]`}`),
      );
    });
    return lines;
  }

  if (args.op === "trace_to") {
    const paths = asRecords(response.paths);
    const warning = depthWarning(response, theme, "max_depth_reached", "truncated_paths");
    const sections = [
      `${theme.fg("success", `${asNumber(response.total_paths) ?? paths.length} path${(asNumber(response.total_paths) ?? paths.length) === 1 ? "" : "s"}`)} ${theme.fg("muted", `${asNumber(response.entry_points_found) ?? 0} entry point${(asNumber(response.entry_points_found) ?? 0) === 1 ? "" : "s"}`)} ${warning}`.trim(),
    ];
    if (paths.length === 0) sections.push(theme.fg("muted", "No entry paths found."));
    paths.forEach((path, index) => {
      const lines: string[] = [];
      renderTracePath(path, index, lines);
      sections.push(lines.join("\n"));
    });
    return sections;
  }

  if (args.op === "impact") {
    const callers = asRecords(response.callers);
    const warning = depthWarning(response, theme);
    const sections = [
      `${theme.fg("warning", `${asNumber(response.total_affected) ?? callers.length} affected call site${(asNumber(response.total_affected) ?? callers.length) === 1 ? "" : "s"}`)} ${theme.fg("muted", `${asNumber(response.affected_files) ?? 0} file${(asNumber(response.affected_files) ?? 0) === 1 ? "" : "s"}`)} ${warning}`.trim(),
    ];
    if (callers.length === 0) sections.push(theme.fg("muted", "No impacted callers found."));
    callers.forEach((caller) => {
      const file = shortenPath(asString(caller.caller_file) ?? "(unknown file)");
      const symbol = asString(caller.caller_symbol) ?? "(unknown)";
      const line = asNumber(caller.line) ?? 0;
      const entry = caller.is_entry_point === true ? ` ${theme.fg("warning", "[entry]")}` : "";
      const expression = asString(caller.call_expression);
      const params = Array.isArray(caller.parameters)
        ? caller.parameters.map(String).join(", ")
        : "";
      sections.push(
        [
          `${theme.fg("accent", file)}:${line}`,
          `  ↳ ${symbol}${entry}`,
          expression ? `  ${theme.fg("muted", expression)}` : undefined,
          params ? `  ${theme.fg("muted", `params: ${params}`)}` : undefined,
        ]
          .filter(Boolean)
          .join("\n"),
      );
    });
    return sections;
  }

  const hops = asRecords(response.hops);
  const sections = [
    `${theme.fg("success", `${hops.length} hop${hops.length === 1 ? "" : "s"}`)} ${asBoolean(response.depth_limited) ? theme.fg("warning", "(depth limited)") : ""}`.trim(),
  ];
  if (hops.length === 0) sections.push(theme.fg("muted", "No data-flow hops found."));
  hops.forEach((hop, index) => {
    const file = shortenPath(asString(hop.file) ?? "(unknown file)");
    const symbol = asString(hop.symbol) ?? "(unknown)";
    const variable = asString(hop.variable) ?? "(unknown)";
    const line = asNumber(hop.line) ?? 0;
    const approximate = hop.approximate === true ? ` ${theme.fg("warning", "[approx]")}` : "";
    sections.push(
      treeLine(
        index,
        `${variable} ${theme.fg("muted", `${asString(hop.flow_type) ?? "flow"}`)} ${symbol} [${file}:${line}]${approximate}`,
      ),
    );
  });
  return sections;
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
  return renderToolCall("navigate", summary, theme, context);
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
    name: "aft_navigate",
    label: "navigate",
    description:
      "Navigate code structure across files using call graph analysis. All ops require both `filePath` and `symbol`. Use `call_tree` for what a function calls, `callers` for call sites, `trace_to` for entry points, `trace_to_symbol` for a path from one symbol to another (requires `toSymbol`; if ambiguous, the error returns candidate files — retry with `toFile`), `impact` for blast radius, `trace_data` to follow a value.",
    parameters: navigateParamsSchema(),
    async execute(_toolCallId: string, params: NavigateArgs, _signal, _onUpdate, extCtx) {
      if (params.op === "trace_data" && !params.expression) {
        throw new Error("op='trace_data' requires an `expression`");
      }
      if (params.op === "trace_to_symbol" && !params.toSymbol) {
        throw new Error("op='trace_to_symbol' requires a `toSymbol`");
      }
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const req: Record<string, unknown> = {
        op: params.op,
        file: params.filePath,
        symbol: params.symbol,
      };
      const depth = coerceOptionalInt(params.depth, "depth", 1, Number.MAX_SAFE_INTEGER);
      if (depth !== undefined) req.depth = depth;
      if (params.expression !== undefined) req.expression = params.expression;
      if (params.toSymbol !== undefined) req.toSymbol = params.toSymbol;
      if (params.toFile !== undefined) req.toFile = params.toFile;
      const response = await callBridge(bridge, params.op, req, extCtx);
      return textResult(JSON.stringify(response, null, 2));
    },
    renderCall(args, theme, context) {
      return renderNavigateCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderNavigateResult(result, context.args, theme, context);
    },
  });
}
