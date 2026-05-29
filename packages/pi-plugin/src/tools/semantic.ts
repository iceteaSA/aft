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
    description: [
      "Find code with unified semantic, lexical, literal, and regex search. Returns ranked symbol/file results or exact matching lines, with routing metadata.",
      "",
      "When to reach for it:",
      "- Exploring an unfamiliar area: 'where is rate limiting handled', 'how does auth flow work'",
      "- Concept doesn't appear as a literal string: 'retry logic', 'cache invalidation', 'graceful shutdown'",
      "- Filename-shaped concepts: 'the bridge spawn helper', 'the session detection module'",
      "- Regex-shaped or exact text queries when you want AFT to classify and route automatically",
      "- You know roughly what the function does but not what it's named",
      "",
      "When NOT to use:",
      "- You need exhaustive literal enumeration → use grep directly",
      "- You want the file/module structure → use aft_outline",
      "- You're following a call chain → use aft_navigate",
      "",
      "Set hint to 'regex', 'literal', 'semantic', or 'auto' to override or document routing intent.",
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
      // Pi has no grep-style permission prompt; callBridge throws success:false
      // envelopes so the host renders them via renderErrorResult below.
      const response = await callBridge(bridge, "semantic_search", req, extCtx);
      return textResult(
        (response.text as string | undefined) ?? JSON.stringify(response, null, 2),
        response,
      );
    },
    renderCall(args, theme, context) {
      return renderSemanticCall(args, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderSemanticResult(result, context.args, theme, context);
    },
  });
}
