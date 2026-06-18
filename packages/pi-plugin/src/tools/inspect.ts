/**
 * aft_inspect — codebase health snapshot.
 */

import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, isEmptyParam, textResult } from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  asNumber,
  asRecord,
  asRecords,
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
        "Restrict scan/results to paths under this scope (file or directory, absolute or relative to project root). Tier 1 scopes the scan; Tier 2 scans project-wide and applies scope as a result filter.",
    }),
  ),
  topK: Type.Optional(
    Type.Integer({
      minimum: 1,
      maximum: 100,
      default: 20,
      description: "Max drill-down items per category. Default 20, max 100.",
    }),
  ),
});

type StringOrStringArray = string | string[];

const TIER2_INSPECT_CATEGORIES = new Set(["dead_code", "unused_exports", "duplicates"]);
const INSPECT_TIER2_RUN_TIMEOUT_MS = 5 * 60_000;
// Pi has no session.idle hook like OpenCode, so on-demand Tier 2 warmups are
// rate-limited per bridge/category to the same default idle window (4 minutes).
const INSPECT_TIER2_MIN_TRIGGER_INTERVAL_MS = 4 * 60_000;
const runningTier2Categories = new WeakMap<object, Set<string>>();
const lastTier2TriggerAtByBridge = new WeakMap<object, Map<string, number>>();

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

async function resolveAndGateScope(
  extCtx: ExtensionContext,
  ctx: PluginContext,
  scope: StringOrStringArray | undefined,
): Promise<StringOrStringArray | undefined> {
  if (scope === undefined) return undefined;
  const values = Array.isArray(scope) ? scope : [scope];
  const resolved = await Promise.all(
    values
      .filter((value): value is string => typeof value === "string" && value.length > 0)
      .map((value) => resolvePathArg(extCtx.cwd, value)),
  );
  const checked = new Set<string>();
  for (const target of resolved) {
    if (checked.has(target)) continue;
    checked.add(target);
    await assertExternalDirectoryPermission(extCtx, target, "read", {
      restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
    });
  }
  return Array.isArray(scope) ? resolved : resolved[0];
}

function validateOptionalTopK(value: unknown): number | undefined {
  if (value === undefined || value === null || value === "") return undefined;
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new Error("topK must be an integer between 1 and 100");
  }
  if (value < 1 || value > 100) {
    throw new Error("topK must be between 1 and 100");
  }
  return value;
}

function diagnosticsServerSummary(section: Record<string, unknown>): string {
  const pending = Array.isArray(section.servers_pending)
    ? section.servers_pending.filter((item): item is string => typeof item === "string")
    : [];
  const notInstalled = Array.isArray(section.servers_not_installed)
    ? section.servers_not_installed.filter((item): item is string => typeof item === "string")
    : [];
  const parts: string[] = [];
  if (pending.length > 0) parts.push(`pending: ${pending.join(", ")}`);
  if (notInstalled.length > 0) parts.push(`not installed: ${notInstalled.join(", ")}`);
  return parts.length > 0 ? parts.join("; ") : "none reported";
}

function diagnosticsSummaryPart(summary: Record<string, unknown> | undefined): string | undefined {
  const section = asRecord(summary?.diagnostics);
  if (!section) return undefined;

  const errors = asNumber(section.errors);
  const warnings = asNumber(section.warnings);
  const info = asNumber(section.info);
  const hints = asNumber(section.hints);
  const hasCounts = [errors, warnings, info, hints].some((value) => value !== undefined);
  const counts = `${errors ?? 0} errors/${warnings ?? 0} warnings/${info ?? 0} info/${hints ?? 0} hints`;
  const status = asString(section.status);

  // Partial result: show counts-so-far alongside the pending/incomplete signal
  // so already-found diagnostics aren't hidden behind a bare sentinel.
  if (status === "pending") {
    return hasCounts
      ? `diagnostics ${counts} so far — still pending (servers: ${diagnosticsServerSummary(section)})`
      : `diagnostics pending (servers: ${diagnosticsServerSummary(section)})`;
  }
  if (status === "incomplete") {
    return hasCounts
      ? `diagnostics ${counts} (incomplete — servers: ${diagnosticsServerSummary(section)})`
      : `diagnostics unavailable (status incomplete; servers: ${diagnosticsServerSummary(section)})`;
  }

  if (hasCounts) {
    return `diagnostics ${counts}`;
  }

  return undefined;
}

function diagnosticLocation(diagnostic: Record<string, unknown>): string {
  const file = asString(diagnostic.file) ?? "(unknown file)";
  const line = asNumber(diagnostic.line);
  const column = asNumber(diagnostic.column);
  if (line === undefined) return file;
  if (column === undefined) return `${file}:${line}`;
  return `${file}:${line}:${column}`;
}

function diagnosticsDetailSection(
  details: Record<string, unknown> | undefined,
): string | undefined {
  const diagnostics = asRecords(details?.diagnostics);
  if (diagnostics.length === 0) return undefined;

  const lines = ["diagnostics"];
  for (const diagnostic of diagnostics) {
    const severity = asString(diagnostic.severity) ?? "information";
    const message = asString(diagnostic.message) ?? "(no message)";
    const source = asString(diagnostic.source);
    const suffix = source ? ` [${source}]` : "";
    lines.push(`- ${diagnosticLocation(diagnostic)} ${severity} ${message}${suffix}`);
  }
  return lines.join("\n");
}

function countFrom(summary: Record<string, unknown> | undefined, key: string): number | undefined {
  const section = asRecord(summary?.[key]);
  return asNumber(section?.count);
}

function tier2SummaryPart(
  summary: Record<string, unknown> | undefined,
  key: string,
  label: string,
): string {
  const section = asRecord(summary?.[key]);
  const count = asNumber(section?.count);
  if (count !== undefined) return `${label} ${count}`;

  const status = asString(section?.status);
  return `${label} ${status ?? "unavailable"}`;
}

/** Short basename for a `path:line-line` duplicate occurrence. */
function shortDupOccurrence(entry: string): string {
  const [path] = entry.split(":");
  return path?.split("/").pop() ?? entry;
}

/**
 * Compact TUI preview of the highest-signal Tier-2 findings (the ranked `top`
 * field), so the one-glance view shows what to act on, not just totals.
 */
function tier2TopPreview(
  summary: Record<string, unknown> | undefined,
  theme: Theme,
): string | undefined {
  const lines: string[] = [];

  const dup = asRecord(summary?.duplicates);
  const dupTop = Array.isArray(dup?.top) ? dup.top : [];
  for (const group of dupTop) {
    const record = asRecord(group);
    const files = Array.isArray(record?.files) ? record.files : [];
    const cost = asNumber(record?.cost);
    if (files.length < 2) continue;
    const a = shortDupOccurrence(String(files[0]));
    const b = shortDupOccurrence(String(files[1]));
    lines.push(`  dup ${a} ↔ ${b}${cost !== undefined ? ` (${cost})` : ""}`);
  }

  for (const [key, label] of [
    ["dead_code", "dead"],
    ["unused_exports", "unused"],
  ] as const) {
    const section = asRecord(summary?.[key]);
    const top = Array.isArray(section?.top) ? section.top : [];
    for (const item of top) {
      const record = asRecord(item);
      const file = asString(record?.file);
      const symbol = asString(record?.symbol);
      if (!file || !symbol) continue;
      lines.push(`  ${label} ${symbol} (${file.split("/").pop()})`);
    }
  }

  if (lines.length === 0) return undefined;
  return `${theme.fg("muted", "top findings:")}\n${lines.join("\n")}`;
}

function tier2RefreshCategories(response: Record<string, unknown>): string[] {
  const scannerState = asRecord(response.scanner_state);
  const categories = new Set<string>();

  for (const key of ["pending_categories", "stale_categories"] as const) {
    const values = scannerState?.[key];
    if (!Array.isArray(values)) continue;

    for (const category of values) {
      if (typeof category === "string" && TIER2_INSPECT_CATEGORIES.has(category)) {
        categories.add(category);
      }
    }
  }

  return [...categories];
}

function runPendingTier2Categories(
  bridge: ReturnType<typeof bridgeFor>,
  categories: string[],
  extCtx: ExtensionContext,
): void {
  const now = Date.now();
  const running = runningTier2Categories.get(bridge) ?? new Set<string>();
  const lastTriggerAt = lastTier2TriggerAtByBridge.get(bridge) ?? new Map<string, number>();
  const toRun = categories.filter((category) => {
    if (running.has(category)) return false;
    const previousTriggerAt = lastTriggerAt.get(category);
    return (
      previousTriggerAt === undefined ||
      previousTriggerAt + INSPECT_TIER2_MIN_TRIGGER_INTERVAL_MS <= now
    );
  });
  if (toRun.length === 0) return;

  for (const category of toRun) {
    running.add(category);
    lastTriggerAt.set(category, now);
  }
  runningTier2Categories.set(bridge, running);
  lastTier2TriggerAtByBridge.set(bridge, lastTriggerAt);

  void callBridge(bridge, "inspect_tier2_run", { categories: toRun }, extCtx, {
    transportTimeoutMs: INSPECT_TIER2_RUN_TIMEOUT_MS,
  })
    .catch(() => {
      // Quiet background warmup: a later aft_inspect call can retry after the cooldown.
    })
    .finally(() => {
      const active = runningTier2Categories.get(bridge);
      if (!active) return;
      for (const category of toRun) {
        active.delete(category);
      }
      if (active.size === 0) {
        runningTier2Categories.delete(bridge);
      }
    });
}

/** Exported for renderer unit tests. */
export function buildInspectSections(payload: unknown, theme: Theme): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No inspect snapshot available.")];

  const summary = asRecord(response.summary);
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
    diagnosticsSummaryPart(summary),
    `metrics ${asNumber(metrics?.files) ?? 0} files/${asNumber(metrics?.symbols) ?? 0} symbols`,
    tier2SummaryPart(summary, "dead_code", "dead code"),
    tier2SummaryPart(summary, "unused_exports", "unused exports"),
    tier2SummaryPart(summary, "duplicates", "duplicates"),
  ].filter((part): part is string => Boolean(part));

  const sections = [theme.fg("accent", parts.join(" · "))];
  if (stale > 0 || pending > 0) {
    sections.push(theme.fg("warning", `scanner state: ${stale} stale · ${pending} pending`));
  }

  const topPreview = tier2TopPreview(summary, theme);
  if (topPreview) sections.push(topPreview);

  const details = asRecord(response.details);
  if (details) {
    const names = Object.keys(details);
    sections.push(
      names.length > 0
        ? `details: ${names.join(", ")}`
        : theme.fg("muted", "No drill-down details returned."),
    );
    const diagnosticsDetails = diagnosticsDetailSection(details);
    if (diagnosticsDetails) sections.push(diagnosticsDetails);
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
      "Codebase health snapshot. One call returns summary stats for: TODOs, diagnostics, file/symbol metrics, dead code, unused exports, code duplicates. Pass `sections` for per-category drill-down details.\n\n" +
      "Categories run in tiers — Tier 1 (todos, metrics) return synchronously from cache. Tier 2 (dead_code, unused_exports, duplicates) run asynchronously on demand: when a call sees cold `pending_categories: [...]` or stale `stale_categories: [...]`, Pi quietly starts a background Tier 2 warmup (deduped while in-flight and rate-limited per category to at most once every 4 minutes, matching OpenCode's default idle window). The current call may still return pending results while the cache warms; a later call can use cached data.\n\n" +
      "Use when: starting work on unfamiliar code, after multi-edit batches to check diagnostics, before a refactor, before review, or to verify cleanup completeness.\n\n" +
      "Treat `dead_code` as a hint, not proof: reachability is call-based, so symbols reached only via method dispatch or referenced only in type position may be false positives — verify before deleting.",
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
      const scope = await resolveAndGateScope(extCtx, ctx, normalizeStringOrArray(params.scope));
      const topK = validateOptionalTopK(params.topK);
      const response = await callBridge(bridge, "inspect", { sections, scope, topK }, extCtx);
      runPendingTier2Categories(bridge, tier2RefreshCategories(response), extCtx);
      const body = response.text as string | undefined;
      if (typeof body === "string") {
        // Rust builds the compact body (duplicates/dead_code/unused_exports/
        // todos). The diagnostics line is rendered plugin-side (it owns the
        // partial/pending honesty logic) and appended after the body, matching
        // the OpenCode `appendRenderedDiagnostics` flow. The status bar is added
        // by the global tool_result hook.
        // Diagnostics summary line + (when present) the detail rows. Rust now
        // always includes diagnostics detail in `details` (a bare count isn't
        // actionable), so render it for the agent here — matching OpenCode's
        // appendRenderedDiagnostics. The detail section self-suppresses when
        // there are no diagnostics, so the clean case stays summary-only.
        const diagnosticsSummary = diagnosticsSummaryPart(asRecord(response.summary));
        const diagnosticsDetail = diagnosticsDetailSection(asRecord(response.details));
        const diagnostics = [diagnosticsSummary, diagnosticsDetail].filter(Boolean).join("\n");
        const text = diagnostics ? (body ? `${body}\n\n${diagnostics}` : diagnostics) : body;
        return textResult(text, response);
      }
      return textResult(JSON.stringify(response, null, 2), response);
    },
    renderCall(args, theme, context) {
      return renderInspectCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderInspectResult(result, theme, context);
    },
  });
}
