/**
 * aft_search — unified code search.
 * Only registered when config.semantic_search is enabled AND
 * the ONNX runtime / configured backend is available.
 */

import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, isEmptyParam, textResult } from "./_shared.js";
import {
  asNumber,
  asRecord,
  asRecords,
  asString,
  extractStructuredPayload,
  groupByFile,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

function semanticHonestyNote(response: Record<string, unknown>, theme: Theme): string | undefined {
  const notes: string[] = [];
  if (response.more_available === true) notes.push("more results available");
  if (response.engine_capped === true) notes.push("enumeration capped");
  if (response.fully_degraded === true) notes.push("fully degraded");
  if (response.complete === false) notes.push("partial/incomplete");
  return notes.length > 0 ? theme.fg("warning", `Search status: ${notes.join("; ")}.`) : undefined;
}

/**
 * Degraded/partial flags NOT already conveyed by Rust's `text` (which carries
 * the count line and the "more results available; raise topK" note). Appended
 * to the agent text so the agent doesn't over-trust degraded results. The rich
 * TUI renderer keeps its own fuller status line separately.
 */
function extraAgentHonestyNote(response: Record<string, unknown>): string | undefined {
  const notes: string[] = [];
  if (response.fully_degraded === true) notes.push("fully degraded");
  if (response.complete === false) notes.push("partial/incomplete");
  return notes.length > 0 ? `Search status: ${notes.join("; ")}.` : undefined;
}

const SearchParams = Type.Object({
  query: Type.String({
    description:
      "Concept, regex, literal text, filename, or capability to find. Examples: 'fuzzy match with whitespace tolerance', '^export', 'Cargo.lock'.",
  }),
  topK: Type.Optional(
    Type.Integer({
      minimum: 1,
      maximum: 100,
      default: 10,
      description: "Maximum number of results (default: 10, max: 100)",
    }),
  ),
  hint: Type.Optional(
    Type.Union(
      [
        Type.Literal("regex"),
        Type.Literal("literal"),
        Type.Literal("semantic"),
        Type.Literal("auto"),
      ],
      {
        description: "Optional routing hint. Defaults to 'auto'.",
      },
    ),
  ),
  includeTests: Type.Optional(
    Type.Boolean({
      default: false,
      description:
        "Include test-support, fixture, mock, snapshot, and corpus files. Defaults to false.",
    }),
  ),
});

/** Exported for renderer unit tests. */
export function buildSemanticSections(
  args: Static<typeof SearchParams>,
  payload: unknown,
  theme: Theme,
): string[] {
  const response = asRecord(payload);
  if (!response) return [theme.fg("muted", "No search result.")];

  const status = asString(response.status) ?? "unknown";
  const semanticStatus = asString(response.semantic_status) ?? status;
  const interpretedAs = asString(response.interpreted_as) ?? "unknown";
  const queryKind = asString(response.query_kind);
  const sections = [
    `${theme.fg(semanticStatus === "ready" ? "success" : "warning", `semantic: ${semanticStatus}`)} ${theme.fg("muted", `mode=${interpretedAs}${queryKind ? ` kind=${queryKind}` : ""} query=${JSON.stringify(args.query)} topK=${args.topK ?? 10}`)}`,
  ];

  const warnings = Array.isArray(response.warnings)
    ? response.warnings.filter((warning): warning is string => typeof warning === "string")
    : [];
  if (warnings.length > 0) {
    sections.push(warnings.map((warning) => theme.fg("warning", `⚠ ${warning}`)).join("\n"));
  }

  const honestyNote = semanticHonestyNote(response, theme);
  if (honestyNote) sections.push(honestyNote);

  const results = asRecords(response.results);
  if (status !== "ready" && results.length === 0) {
    sections.push(asString(response.text) ?? theme.fg("muted", "Semantic index is not ready."));
    return sections;
  }

  if (results.length === 0) {
    sections.push(theme.fg("muted", "No matches found."));
    return sections;
  }

  const grouped = groupByFile(results, (result) => asString(result.file));
  for (const [file, fileResults] of grouped.entries()) {
    const lines = [theme.fg("accent", shortenPath(file))];
    fileResults.forEach((result) => {
      if (asString(result.kind) === "GrepLine") {
        const line = asNumber(result.line);
        const column = asNumber(result.column);
        const lineText = asString(result.line_text) ?? "";
        const location =
          line !== undefined ? `${line}${column !== undefined ? `:${column}` : ""}` : "?";
        lines.push(`  ↳ ${theme.fg("muted", `line ${location}`)} ${lineText}`);
        return;
      }

      const score = asNumber(result.score);
      const source = asString(result.source);
      const kind = asString(result.kind) ?? "symbol";
      const location = asString(result.location);
      if (source === "lexical") {
        lines.push(
          `  ↳ ${theme.fg("muted", `[lexical match${score !== undefined ? ` — score ${score.toFixed(3)}` : ""}]`)}`,
        );
        const snippet = asString(result.snippet);
        if (snippet) {
          lines.push(...snippet.split("\n").map((line) => `     ${line}`));
        }
        return;
      }
      if (kind === "file_summary" || location === "[file summary]") {
        const summary = asString(result.snippet) ?? asString(result.name) ?? "(no summary)";
        lines.push(
          `  ↳ ${summary} ${theme.fg("muted", `[file summary${score !== undefined ? ` score ${score.toFixed(3)}` : ""}]`)}`,
        );
        return;
      }
      const startLine = asNumber(result.start_line);
      const endLine = asNumber(result.end_line);
      const range =
        startLine !== undefined
          ? `${startLine}${endLine && endLine !== startLine ? `-${endLine}` : ""}`
          : "?";
      const name = asString(result.name) ?? "(unknown)";
      lines.push(
        `  ↳ ${name} ${theme.fg("muted", `[${kind}] lines ${range}${score !== undefined ? ` score ${score.toFixed(3)}` : ""}`)}`,
      );
      const snippet = asString(result.snippet);
      if (snippet) {
        lines.push(...snippet.split("\n").map((line) => `     ${line}`));
      }
    });
    sections.push(lines.join("\n"));
  }

  return sections;
}

/** Exported for renderer unit tests. */
export function renderSemanticCall(
  args: Static<typeof SearchParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  return renderToolCall("search", theme.fg("toolOutput", args.query), theme, context);
}

/** Exported for renderer unit tests. */
export function renderSemanticResult(
  result: AgentToolResult<unknown>,
  args: Static<typeof SearchParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) return renderErrorResult(result, "search failed", theme, context);
  return renderSections(
    buildSemanticSections(args, extractStructuredPayload(result), theme),
    context,
  );
}

export function registerSemanticTool(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerTool({
    name: "aft_search",
    label: "search",
    // Lean and positive on purpose (parity with OpenCode): this is the
    // primary code-search tool, so the description must not push agents
    // elsewhere. The old "When NOT to use: ... use grep directly" line fed
    // the exact bash-grep reflex the system prompt works to suppress.
    description: [
      "Search code with one tool: concepts, identifiers, error strings, regex, literals, and filenames are auto-routed to the right engine and returned ranked. Use it for any code search — including when you only know what the code does, not what it's named ('where is rate limiting handled', 'retry logic', '^export', 'Cargo.lock').",
      "",
      "Set hint to 'regex', 'literal', or 'semantic' to force a lane.",
    ].join("\n"),
    parameters: SearchParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof SearchParams>,
      _signal,
      _onUpdate,
      extCtx,
    ) {
      if (
        isEmptyParam(params.query) ||
        typeof params.query !== "string" ||
        params.query.trim().length === 0
      ) {
        throw new Error("semantic_search: invalid params: `query` must be a non-empty string");
      }

      const bridge = bridgeFor(ctx, extCtx.cwd);
      const req: Record<string, unknown> = { query: params.query };
      if (params.topK !== undefined) req.top_k = params.topK;
      if (params.hint !== undefined) req.hint = params.hint;
      if (params.includeTests !== undefined) req.include_tests = params.includeTests;
      // Pi has no grep-style permission prompt; callBridge throws success:false
      // envelopes so the host renders them via renderErrorResult below.
      const response = await callBridge(bridge, "semantic_search", req, extCtx);
      // Rust's `text` is the clean agent-facing rendering (ranked rows,
      // rank-tiered snippets, count line, more-available + zoom hints). The
      // full structured response still flows to the rich TUI renderer below.
      // Only degraded/partial flags that `text` doesn't already carry are
      // appended to the agent text.
      // Never fall back to JSON.stringify(response) — that re-opens the exact
      // structured dump the redesign removed (full paths/scores/ids the agent
      // never acts on). On the should-never-happen path where Rust returns
      // success without `text`, emit a minimal note instead, matching the
      // OpenCode plugin's fallback.
      let agentText = (response.text as string | undefined) ?? "No results.";
      const extra = extraAgentHonestyNote(response);
      if (extra) agentText = `${agentText}\n${extra}`;
      return textResult(agentText, response);
    },
    renderCall(args, theme, context) {
      return renderSemanticCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderSemanticResult(result, context.args, theme, context);
    },
  });
}
