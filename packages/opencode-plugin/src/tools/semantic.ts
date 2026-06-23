import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam, optionalInt } from "./_shared.js";
import { askGrepPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

function semanticHonestyNote(response: Record<string, unknown>): string | undefined {
  const notes: string[] = [];
  if (response.more_available === true) notes.push("more results available");
  if (response.engine_capped === true) notes.push("enumeration capped");
  if (response.fully_degraded === true) notes.push("fully degraded");
  if (response.complete === false) notes.push("partial/incomplete");
  return notes.length > 0 ? `Search status: ${notes.join("; ")}.` : undefined;
}

/**
 * Honesty flags NOT already conveyed by Rust's `text` (which carries the count
 * line and the "more results available; raise topK" note). Only degraded /
 * partial states need appending so the agent doesn't over-trust the results.
 */
function extraHonestyNote(response: Record<string, unknown>): string | undefined {
  const notes: string[] = [];
  if (response.fully_degraded === true) notes.push("fully degraded");
  if (response.complete === false) notes.push("partial/incomplete");
  return notes.length > 0 ? `Search status: ${notes.join("; ")}.` : undefined;
}

type ToolArg = ToolDefinition["args"][string];

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

export function semanticTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const searchTool: ToolDefinition = {
    // Lean and positive on purpose: this is the primary code-search tool, so
    // the description must not push agents elsewhere. The old "When NOT to
    // use: ... use grep directly" line fed the exact bash-grep reflex the
    // system prompt works to suppress, and sibling tools (aft_outline,
    // aft_callgraph) already describe themselves.
    description: [
      "Search code with one tool: concepts, identifiers, error strings, regex, literals, and filenames are auto-routed to the right engine and returned ranked. Use it for any code search — including when you only know what the code does, not what it's named ('where is rate limiting handled', 'retry logic', '^export', 'Cargo.lock').",
      "",
      "Set hint to 'regex', 'literal', or 'semantic' to force a lane.",
    ].join("\n"),
    args: {
      query: arg(
        z
          .string()
          .describe(
            "Concept, regex, literal text, filename, or capability to find. Examples: 'fuzzy match with whitespace tolerance', '^export', 'Cargo.lock'.",
          ),
      ),
      topK: arg(optionalInt(1, 100).describe("Number of results (default: 10, max: 100)")),
      hint: arg(
        z
          .enum(["regex", "literal", "semantic", "auto"])
          .optional()
          .describe("Optional routing hint. Defaults to 'auto'."),
      ),
      includeTests: arg(
        z
          .boolean()
          .optional()
          .describe(
            "Include test-support, fixture, mock, snapshot, and corpus files. Defaults to false.",
          ),
      ),
    },
    execute: async (args, context): Promise<string> => {
      if (
        isEmptyParam(args.query) ||
        typeof args.query !== "string" ||
        args.query.trim().length === 0
      ) {
        throw new Error("semantic_search: invalid params: `query` must be a non-empty string");
      }
      const query = args.query;
      const hint = typeof args.hint === "string" ? args.hint : undefined;

      // Match grep permission behavior for every mode that might inspect file contents.
      // This intentionally over-asks for auto/NL queries but never under-asks for regex/literal.
      if (hint !== "semantic") {
        const denied = await askGrepPermission(context, query);
        if (denied) return permissionDeniedResponse(denied);
      }

      const bridgeParams: Record<string, unknown> = {
        query,
        top_k: args.topK ?? 10,
      };
      if (hint) bridgeParams.hint = hint;
      if (typeof args.includeTests === "boolean") bridgeParams.include_tests = args.includeTests;
      const response = await callBridge(ctx, context, "semantic_search", bridgeParams);

      if (response.success === false) {
        const message =
          typeof response.message === "string" && response.message.length > 0
            ? response.message
            : "semantic_search failed";
        const code =
          typeof response.code === "string" && response.code.length > 0 ? response.code : undefined;
        throw new Error(code ? `semantic_search: ${code} — ${message}` : message);
      }

      // Rust's `text` is the agent-facing rendering: ranked rows, rank-tiered
      // snippets, count line, more-available note, and a conditional zoom hint.
      // We deliberately do NOT dump the structured response — the full-path,
      // score, semantic_score, hybrid_boosted JSON was pure clutter the agent
      // never acted on (and inflated token cost). Honesty flags (degraded /
      // partial) that aren't already in `text` are appended as a short note.
      if (typeof response.text === "string" && response.text.length > 0) {
        const note = extraHonestyNote(response);
        return note ? `${response.text}\n${note}` : response.text;
      }

      // No text (shouldn't happen on success) — fall back to a minimal note.
      return semanticHonestyNote(response) ?? "No results.";
    },
  };

  return {
    aft_search: searchTool,
  };
}
