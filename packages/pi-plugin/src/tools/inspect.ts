/**
 * aft_inspect — codebase health snapshot.
 */

import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, coerceOptionalInt, isEmptyParam, textResult } from "./_shared.js";
import {
  asNumber,
  asRecord,
  asString,
  extractStructuredPayload,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
} from "./render-helpers.js";

const InspectParams = Type.Object({
  sections: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code']). Use 'all' for every active category. Omit for summary-only mode.",
    }),
  ),
  scope: Type.Optional(
    Type.Union([Type.String(), Type.Array(Type.String())], {
      description:
        "Restrict drill-down items to paths under this scope (file or directory, absolute or relative to project root). Tier 2 categories scan project-wide regardless of scope and apply scope as a result filter.",
    }),
  ),
  topK: Type.Optional(
    Type.Number({ description: "Max drill-down items per category. Default 20, max 100." }),
  ),
});

type StringOrStringArray = string | string[];

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

function countFrom(summary: Record<string, unknown> | undefined, key: string): number | undefined {
  const section = asRecord(summary?.[key]);
  return asNumber(section?.count);
}

/** Exported for renderer unit tests. */
export function buildInspectSections(payload: unknown, theme: Theme): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No inspect snapshot available.")];

  const summary = asRecord(response.summary);
  const diagnostics = asRecord(summary?.diagnostics);
  const metrics = asRecord(summary?.metrics);
  const scannerState = asRecord(response.scanner_state);
  const stale = Array.isArray(scannerState?.stale_categories)
    ? scannerState.stale_categories.length
    : 0;
  const pending = Array.isArray(scannerState?.pending_categories)
    ? scannerState.pending_categories.length
    : 0;

  const parts = [
    `todos ${countFrom(summary, "todos") ?? 0}`,
    `diagnostics ${asNumber(diagnostics?.errors) ?? 0} errors/${asNumber(diagnostics?.warnings) ?? 0} warnings`,
    `metrics ${asNumber(metrics?.files) ?? 0} files/${asNumber(metrics?.symbols) ?? 0} symbols`,
    `dead code ${countFrom(summary, "dead_code") ?? 0}`,
    `unused exports ${countFrom(summary, "unused_exports") ?? 0}`,
    `duplicates ${countFrom(summary, "duplicates") ?? 0}`,
  ];

  const sections = [theme.fg("accent", parts.join(" · "))];
  if (stale > 0 || pending > 0) {
    sections.push(theme.fg("warning", `scanner state: ${stale} stale · ${pending} pending`));
  }

  const details = asRecord(response.details);
  if (details) {
    const names = Object.keys(details);
    sections.push(
      names.length > 0
        ? `details: ${names.join(", ")}`
        : theme.fg("muted", "No drill-down details returned."),
    );
  }

  const text = asString(response.text);
  if (text) sections.push(text);
  return sections;
}

/** Exported for renderer unit tests. */
export function renderInspectCall(
  args: Static<typeof InspectParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  const sections = Array.isArray(args.sections)
    ? `${args.sections.length} sections`
    : args.sections;
  const scope = Array.isArray(args.scope) ? `${args.scope.length} scopes` : args.scope;
  const summary = [sections, scope, args.topK ? `topK=${args.topK}` : undefined]
    .filter(Boolean)
    .join(" ");
  return renderToolCall(
    "inspect",
    summary ? theme.fg("toolOutput", summary) : undefined,
    theme,
    context,
  );
}

/** Exported for renderer unit tests. */
export function renderInspectResult(
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "inspect failed", theme, context);
  return renderSections(buildInspectSections(extractStructuredPayload(result), theme), context);
}

export function registerInspectTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_inspect",
    label: "inspect",
    description:
      "Codebase health snapshot. One call returns summary stats for: TODOs, file/symbol metrics, LSP diagnostics, dead code, unused exports, code duplicates. Pass `sections` for per-category drill-down details.\n\n" +
      "Categories run in tiers — Tier 1 (todos, metrics, diagnostics) return synchronously from cache. Tier 2 (dead_code, unused_exports, duplicates) run as background scans triggered on session idle; calls may return cached `stale_categories: [...]` results if a refresh is in progress (waits up to 1s for fresh data before falling back to cached).\n\n" +
      "Use when: starting work on unfamiliar code, before a refactor, before review, or to verify cleanup completeness.",
    parameters: InspectParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof InspectParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const sections = normalizeStringOrArray(params.sections);
      const scope = normalizeStringOrArray(params.scope);
      const topK = coerceOptionalInt(params.topK, "topK", 1, 100);
      const response = await callBridge(bridge, "inspect", { sections, scope, topK }, extCtx);
      return textResult(
        (response.text as string | undefined) ?? JSON.stringify(response, null, 2),
        response,
      );
    },
    renderCall(args, theme, context) {
      return renderInspectCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderInspectResult(result, theme, context);
    },
  });
}
