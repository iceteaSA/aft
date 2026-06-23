/**
 * Hoisted tool overrides — replace Pi's built-in read/write/edit/grep with
 * AFT-backed Rust implementations. Registering a tool with the same name as
 * a built-in replaces the built-in entirely.
 *
 * Each tool provides:
 *  - `promptSnippet` / `promptGuidelines`: teach the model our argument shape
 *    in Pi's system prompt (Pi's built-ins use generic one-liners otherwise).
 *  - `renderCall` / `renderResult` for `write` and `edit`: without these,
 *    Pi's ToolExecutionComponent falls back to the *built-in* renderer for
 *    same-named tools, which reads `path` and `edits[]` and garbles our
 *    `filePath` / `oldString` / `newString` output (issue #15).
 *  - Structured `details: { diff, firstChangedLine }` so the rendered diff
 *    also ends up in the agent's message stream, matching Pi's convention.
 *
 * `read` and `grep` keep the default text-only result rendering because our
 * payload (`path`, `pattern`) already aligns with Pi's built-in arg shape.
 */

import { stat } from "node:fs/promises";
import { homedir } from "node:os";
import { isAbsolute, relative, resolve, sep } from "node:path";
import {
  coerceBoolean,
  formatEditSummary,
  formatReadFooter as formatSharedReadFooter,
} from "@cortexkit/aft-bridge";
import {
  type AgentToolResult,
  type ExtensionAPI,
  renderDiff,
  type Theme,
} from "@earendil-works/pi-coding-agent";
import { type Component, Container, Spacer, Text } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, coerceOptionalInt, optionalInt, textResult } from "./_shared.js";
import { formatDiffForPi } from "./diff-format.js";

// Diagnostics on edit are config-driven only (`lsp.diagnostics_on_edit`).
// There is deliberately NO per-call `diagnostics` param: agents never used it,
// and the agent-facing diagnostics paths are the status bar (passive,
// automatic E/W on tool results) and aft_inspect (active pull). The config
// knob remains for users whose models won't call aft_inspect.
function diagnosticsOnEditDefault(ctx: PluginContext): boolean {
  return ctx.config.lsp?.diagnostics_on_edit ?? false;
}

/**
 * Local shape for Pi's render context — the real type is exposed by
 * `@earendil-works/pi-coding-agent`'s internals but not publicly exported.
 * We only read `lastComponent` and `isError` here; everything else is ignored.
 */
interface RenderContextLike {
  lastComponent: Component | undefined;
  isError: boolean;
}

type SearchPathArgSplit = { paths: string[]; missing: string[] };

function containsPath(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
}

/**
 * Expand a leading `~` to the user's home directory. Returns the path
 * unchanged if it does not start with `~`. Mirrors shell-style expansion so
 * agent calls like `grep ... in ~/Work/...` resolve before any filesystem
 * stat or permission check sees the literal tilde.
 */
function expandTilde(path: string): string {
  if (!path || !path.startsWith("~")) return path;
  if (path === "~") return homedir();
  if (path.startsWith(`~${sep}`) || path.startsWith("~/")) {
    return resolve(homedir(), path.slice(2));
  }
  return path;
}

function absoluteSearchPath(cwd: string, target: string): string {
  const expanded = expandTilde(target);
  return isAbsolute(expanded) ? expanded : resolve(cwd, expanded);
}

async function searchPathExists(cwd: string, target: string): Promise<boolean> {
  try {
    await stat(absoluteSearchPath(cwd, target));
    return true;
  } catch {
    return false;
  }
}

async function splitSearchPathArg(cwd: string, raw: string): Promise<SearchPathArgSplit> {
  if ((await searchPathExists(cwd, raw)) || !/\s/.test(raw)) {
    return { paths: [raw], missing: [] };
  }

  const fragments = raw.trim().split(/\s+/).filter(Boolean);
  if (fragments.length < 2) {
    return { paths: [raw], missing: [] };
  }

  const existing: string[] = [];
  const missing: string[] = [];
  for (const fragment of fragments) {
    if (await searchPathExists(cwd, fragment)) {
      existing.push(fragment);
    } else {
      missing.push(fragment);
    }
  }

  if (existing.length === 0) {
    return { paths: [raw], missing: [] };
  }

  return { paths: existing, missing };
}

async function bridgeSearchPathArg(cwd: string, split: SearchPathArgSplit): Promise<string> {
  if (split.paths.length === 1 && split.missing.length === 0) {
    return await resolvePathArg(cwd, split.paths[0]);
  }
  return split.paths.map((target) => absoluteSearchPath(cwd, target)).join(" ");
}

function formatSkippedSearchPaths(missing: string[]): string | undefined {
  if (missing.length === 0) return undefined;
  const noun = missing.length === 1 ? "path" : "paths";
  return `Skipped ${missing.length} ${noun} not found: ${missing.join(", ")}`;
}

function appendSkippedSearchPaths(text: string, missing: string[]): string {
  const note = formatSkippedSearchPaths(missing);
  if (!note) return text;
  return text.length > 0 ? `${text}\n\n${note}` : note;
}

/**
 * Enforce AFT's `restrict_to_project_root` isolation for an out-of-root target.
 *
 * Pi has no host-level permission/allow-list system to bubble to. So the knob
 * is binary: when `restrict_to_project_root` is false (Pi default) the path is
 * allowed (Rust accepts it); when true, the path is hard-blocked with a clear,
 * actionable error — never a prompt. A per-call grant could never override the
 * Rust-side boundary anyway, which is exactly the issue #125 footgun this
 * avoids. The thrown error surfaces as the tool result (Pi's user surface).
 */
export async function assertExternalDirectoryPermission(
  extCtx: { cwd: string },
  target: string,
  options: { restrictToProjectRoot?: boolean } = {},
): Promise<void> {
  if (!target) return;
  const expanded = expandTilde(target);
  const absoluteTarget = isAbsolute(expanded) ? expanded : resolve(extCtx.cwd, expanded);
  if (containsPath(extCtx.cwd, absoluteTarget)) return;

  // User has explicitly opted out of path restriction (the Pi default).
  // Pi has no host-level external_directory allow-list to consult, so a
  // ui.confirm prompt has no policy behind it — it would just nag the
  // user on every external path. Defer to Rust, which will accept the
  // path because `restrict_to_project_root` is false.
  if (options.restrictToProjectRoot === false) return;

  // restrict_to_project_root is AFT's full-isolation knob — NOT a per-call
  // permission. When it's on, an out-of-root path is hard-blocked: do NOT
  // prompt (a grant could never override the Rust-side boundary anyway — that
  // produced issue #125's "approved but still fails"). Throw the clear,
  // actionable denial; Pi renders it as the tool result, which IS the user
  // surface here (no separate ignored-panel channel like OpenCode).
  throw new Error(
    `Blocked: '${absoluteTarget}' is outside the project root and restrict_to_project_root is ` +
      "enabled (AFT full isolation). Not overridable per-call; set restrict_to_project_root: false " +
      "in aft.jsonc to allow external paths.",
  );
}

const ReadParams = Type.Object({
  path: Type.String({
    description: "Path to the file to read (absolute or relative to project root)",
  }),
  offset: optionalInt(1, Number.MAX_SAFE_INTEGER),
  limit: optionalInt(1, Number.MAX_SAFE_INTEGER),
});

const WriteParams = Type.Object({
  filePath: Type.String({
    description: "Path to the file to write (absolute or relative to project root)",
  }),
  content: Type.String({ description: "Full file contents to write" }),
});

const EditParams = Type.Object({
  filePath: Type.String({
    description: "Path to the file to edit (absolute or relative to project root)",
  }),
  oldString: Type.Optional(
    Type.String({ description: "Text to find (exact match, fuzzy fallback)" }),
  ),
  newString: Type.Optional(Type.String({ description: "Replacement text (omit to delete match)" })),
  replaceAll: Type.Optional(Type.Boolean({ description: "Replace every occurrence" })),
  occurrence: optionalInt(0, Number.MAX_SAFE_INTEGER),
  appendContent: Type.Optional(
    Type.String({
      description:
        "Append text to the end of the file (creates the file if missing, parent dirs auto-created). When set, oldString/newString are ignored.",
    }),
  ),
});

const GrepParams = Type.Object({
  pattern: Type.String({ description: "Regex pattern to search for" }),
  path: Type.Optional(
    Type.String({
      description: "Path scope (file or directory; absolute or relative to project root)",
    }),
  ),
  include: Type.Optional(
    Type.String({ description: "Glob filter for included files (e.g. '*.ts,*.tsx')" }),
  ),
  caseSensitive: Type.Optional(Type.Boolean({ description: "Case-sensitive matching" })),
});

export interface ToolSurfaceFlags {
  hoistRead: boolean;
  hoistWrite: boolean;
  hoistEdit: boolean;
  hoistGrep: boolean;
  /**
   * Mirrors the user's `restrict_to_project_root` AFT config (Pi default
   * `false`). When false, the user has explicitly opted into "no
   * restriction" — Pi has no host-level external_directory allow-list, so
   * a `ui.confirm` prompt has no policy to consult and would only annoy
   * the user. When true, Rust hard-rejects out-of-root paths before the
   * plugin layer sees them anyway, so the prompt is also unreachable. We
   * pass this through so `assertExternalDirectoryPermission` can skip the
   * prompt in the false case (the common one) and the helper stays in
   * place as a safety net for unusual contexts that opt into restriction
   * but still want a chance to allow a one-off external write.
   */
  restrictToProjectRoot: boolean;
}

/** Details surfaced to both renderer and agent message stream. */
interface FileMutationDetails {
  diff?: string;
  firstChangedLine?: number;
  additions: number;
  deletions: number;
  replacements?: number;
  diagnostics?: unknown[];
  /**
   * True when Rust returned `diff.truncated = true` — the before/after strings
   * were omitted because the file exceeded the diff size cap, so we have no
   * line-level diff to render. Both the agent-facing text and the TUI renderer
   * surface this explicitly rather than silently showing a summary.
   */
  truncated?: boolean;
  /**
   * Whether AFT's auto-formatter ran on the post-write content. Mirrors the
   * `data.formatted` field from the Rust write/edit response. When true,
   * the file content on disk is what the formatter produced; when false,
   * `formatSkippedReason` explains why.
   */
  formatted?: boolean;
  /**
   * Reason the formatter was skipped, when `formatted=false`. One of the
   * documented values from `crates/aft/src/format.rs::auto_format`:
   * `"unsupported_language"`, `"no_formatter_configured"`,
   * `"formatter_not_installed"`, `"formatter_excluded_path"`, `"timeout"`,
   * `"error"`. Pi agents read this to decide whether to retry, fix config,
   * or accept the unformatted result.
   */
  formatSkippedReason?: string;
  /**
   * v0.27.1: Rust returns `no_op: true` when the post-write file content
   * is byte-identical to the pre-write state. This separates "matched but
   * produced no change" from a real `+0/-0` failure mode in the UI.
   * See GitHub #45.
   */
  noOp?: boolean;
}

export function registerHoistedTools(
  pi: ExtensionAPI,
  ctx: PluginContext,
  surface: ToolSurfaceFlags,
): void {
  if (surface.hoistRead) {
    pi.registerTool({
      name: "read",
      label: "read",
      description:
        "Read file contents with line numbers. Backed by AFT's indexed Rust reader — faster than the built-in `read` on large repos and correctly handles images/PDFs as attachments.",
      promptSnippet: "Read file contents (supports offset/limit for large files)",
      promptGuidelines: ["Use read to examine files instead of cat or sed."],
      parameters: ReadParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof ReadParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const offset = coerceOptionalInt(params.offset, "offset", 1, Number.MAX_SAFE_INTEGER);
        const limit = coerceOptionalInt(params.limit, "limit", 1, Number.MAX_SAFE_INTEGER);
        // Resolve ~ / relative once and use the same value for the permission
        // check and the bridge. Without this, hoisted read bypassed Pi's
        // external-path prompt/deny layer while write/edit/grep were guarded.
        const filePath = await resolvePathArg(extCtx.cwd, params.path);
        await assertExternalDirectoryPermission(extCtx, filePath, {
          restrictToProjectRoot: surface.restrictToProjectRoot,
        });
        const req: Record<string, unknown> = {
          file: filePath,
        };
        if (offset !== undefined) {
          req.start_line = offset;
          if (limit !== undefined) {
            req.end_line = offset + limit - 1;
          }
        } else if (limit !== undefined) {
          req.end_line = limit;
        }
        const response = await callBridge(bridge, "read", req, extCtx);
        if (Array.isArray(response.entries)) {
          return textResult((response.entries as string[]).join("\n"));
        }
        let text = (response.content as string | undefined) ?? "";

        // Two-case footer (kept aligned with the OpenCode plugin's
        // formatReadFooter — see docs there for case A/B rationale).
        // Pi previously discarded `truncated`/`total_lines` entirely, so
        // an agent that read a 500-line file with no range got back
        // default-clamped 100 lines with NO signal that 400 more lines
        // existed. This restores Case A (hint when agent didn't choose)
        // while avoiding the patronizing hint when the agent already
        // chose a range (Case B → no footer).
        const agentSpecifiedRange = offset !== undefined || limit !== undefined;
        const footer = formatReadFooter(agentSpecifiedRange, response);
        if (footer) text += footer;
        return textResult(text);
      },
    });
  }

  if (surface.hoistWrite) {
    pi.registerTool<typeof WriteParams, FileMutationDetails>({
      name: "write",
      label: "write",
      description:
        "Write content to a file, creating it and parent directories automatically. Existing files are backed up before overwriting (undo via aft_safety) and auto-formatted when the project has a formatter configured. Uses `filePath` (not `path`). For partial edits, use the `edit` tool.",
      promptSnippet: "Create or overwrite files (uses filePath; auto-formats)",
      promptGuidelines: ["Use write only for new files or complete rewrites."],
      parameters: WriteParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof WriteParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        // Resolve ~ / relative ONCE and use the same value for the permission
        // check and the bridge. Previously the check expanded ~ but the bridge
        // got the raw `~/...`, which Rust treats literally — creating a literal
        // `~` directory under the project root instead of writing to home.
        const filePath = await resolvePathArg(extCtx.cwd, params.filePath);
        await assertExternalDirectoryPermission(extCtx, filePath, {
          restrictToProjectRoot: surface.restrictToProjectRoot,
        });
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const response = await callBridge(
          bridge,
          "write",
          {
            file: filePath,
            content: params.content,
            diagnostics: diagnosticsOnEditDefault(ctx),
            include_diff_content: true,
          },
          extCtx,
        );
        return buildMutationResult(response);
      },
      renderCall(args, theme, context) {
        return renderMutationCall("write", args?.filePath, theme, context);
      },
      renderResult(result, _options, theme, context) {
        return renderMutationResult(result, theme, context);
      },
    });
  }

  if (surface.hoistEdit) {
    pi.registerTool<typeof EditParams, FileMutationDetails>({
      name: "edit",
      label: "edit",
      description:
        "Find-and-replace edit with progressive fuzzy matching (handles whitespace and Unicode drift). Uses `filePath`, `oldString`, `newString`. Errors on multiple matches — use `occurrence` to pick one, or `replaceAll: true`.",
      promptSnippet:
        "Targeted find-and-replace (uses filePath/oldString/newString; occurrence or replaceAll for disambiguation; fuzzy whitespace matching). Pass appendContent to append to a file (creates if missing).",
      promptGuidelines: [
        "Prefer edit over write when changing part of an existing file.",
        "Include enough surrounding context in oldString to make the match unique, or set replaceAll/occurrence explicitly.",
        "Use appendContent (instead of read+write) when adding text to the end of a file.",
      ],
      parameters: EditParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof EditParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        // Resolve ~ / relative ONCE (see the write handler) and use it for the
        // permission check and the bridge, so Rust receives the real target
        // instead of a literal `~/...`.
        const filePath = await resolvePathArg(extCtx.cwd, params.filePath);
        await assertExternalDirectoryPermission(extCtx, filePath, {
          restrictToProjectRoot: surface.restrictToProjectRoot,
        });
        const bridge = bridgeFor(ctx, extCtx.cwd);

        // Append mode: explicitly route through the Rust `append` op, which
        // creates the file (and parent dirs) when missing and appends without
        // reading the whole file first. oldString/newString are ignored when
        // appendContent is set, matching the OpenCode-side hoisted edit shape.
        if (typeof params.appendContent === "string") {
          const req: Record<string, unknown> = {
            op: "append",
            file: filePath,
            append_content: params.appendContent,
            diagnostics: diagnosticsOnEditDefault(ctx),
            include_diff_content: true,
          };
          const response = await callBridge(bridge, "edit_match", req, extCtx);
          return buildMutationResult(response);
        }

        const req: Record<string, unknown> = {
          file: filePath,
          match: params.oldString ?? "",
          replacement: params.newString ?? "",
          diagnostics: diagnosticsOnEditDefault(ctx),
          include_diff_content: true,
        };
        // Coerce at the boundary: stringified replaceAll must forward true (coerceBoolean).
        if (coerceBoolean(params.replaceAll)) req.replace_all = true;
        const occurrence = coerceOptionalInt(
          params.occurrence,
          "occurrence",
          0,
          Number.MAX_SAFE_INTEGER,
        );
        if (occurrence !== undefined) req.occurrence = occurrence;

        const response = await callBridge(bridge, "edit_match", req, extCtx);
        return buildMutationResult(response);
      },
      renderCall(args, theme, context) {
        return renderMutationCall("edit", args?.filePath, theme, context);
      },
      renderResult(result, _options, theme, context) {
        return renderMutationResult(result, theme, context);
      },
    });
  }

  if (surface.hoistGrep) {
    pi.registerTool({
      name: "grep",
      label: "grep",
      description:
        "Search for a regex pattern across files. Uses AFT's trigram index inside the project root for fast repeated queries, and falls back to ripgrep for paths outside the project root.",
      promptSnippet: "Fast regex search across files (trigram-indexed inside the project root)",
      promptGuidelines: ["Prefer grep over bash-invoked find/rg for in-project searches."],
      parameters: GrepParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof GrepParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const req: Record<string, unknown> = { pattern: params.pattern };
        let pathSplit: SearchPathArgSplit | undefined;
        if (params.path) {
          pathSplit = await splitSearchPathArg(extCtx.cwd, params.path);
          for (const target of pathSplit.paths) {
            await assertExternalDirectoryPermission(
              extCtx,
              absoluteSearchPath(extCtx.cwd, target),
              {
                restrictToProjectRoot: surface.restrictToProjectRoot,
              },
            );
          }
          req.path = await bridgeSearchPathArg(extCtx.cwd, pathSplit);
        }
        if (params.include) req.include = splitIncludeGlobs(params.include);
        if (params.caseSensitive !== undefined) req.case_sensitive = params.caseSensitive;

        const response = await callBridge(bridge, "grep", req, extCtx);
        if (pathSplit && pathSplit.missing.length > 0) {
          response.complete = false;
        }
        const text = appendSkippedSearchPaths(
          (response.text as string | undefined) ?? "",
          pathSplit?.missing ?? [],
        );
        return textResult(text, response);
      },
    });
  }
}

// ---------------------------------------------------------------------------
// Mutation helpers — write and edit share result shape and rendering.
// ---------------------------------------------------------------------------

/**
 * Shape the bridge `edit_match` / `write` response into an `AgentToolResult`
 * Pi can render. Exported for unit tests covering truncation and diagnostics
 * behavior without spinning up a real bridge.
 */
export function buildMutationResult(
  response: Record<string, unknown>,
): AgentToolResult<FileMutationDetails> {
  const diffObj = response.diff as
    | {
        before?: string;
        after?: string;
        additions?: number;
        deletions?: number;
        truncated?: boolean;
      }
    | undefined;
  const additions = diffObj?.additions ?? 0;
  const deletions = diffObj?.deletions ?? 0;
  const replacements = response.replacements as number | undefined;
  const diagnostics = response.lsp_diagnostics as unknown[] | undefined;
  const truncated = diffObj?.truncated === true;
  // Rust v0.27.1: `no_op: true` when the file content is byte-identical to
  // the pre-write state — either the agent passed `oldString === newString`,
  // a formatter normalized the change away, or the replacement matched the
  // existing content. The match was satisfied (replacements > 0) but no net
  // file change landed. See GitHub #45.
  const noOp = response.no_op === true;
  // Format outcome — Rust writes return `formatted: bool` and, when
  // skipped, `format_skipped_reason: "<reason>"`. Forward both into
  // `details` so Pi agents can act on them (retry with different config,
  // accept the unformatted result, etc). The OpenCode plugin surfaces
  // these the same way; this is the Pi parity fix.
  const formatted = response.formatted as boolean | undefined;
  const formatSkippedReason = response.format_skipped_reason as string | undefined;
  const globFormatSkipReasons = response.format_skip_reasons as unknown;

  // Generate the Pi-style line-numbered diff when Rust gave us before/after
  // and the diff wasn't truncated. Truncated diffs carry `additions`/`deletions`
  // counts but no before/after strings, so we surface that explicitly in both
  // the agent-facing text and the TUI renderer instead of silently collapsing
  // to a summary-only output.
  let diffText: string | undefined;
  let firstChangedLine: number | undefined;
  if (
    diffObj &&
    !truncated &&
    typeof diffObj.before === "string" &&
    typeof diffObj.after === "string"
  ) {
    const piDiff = formatDiffForPi(diffObj.before, diffObj.after);
    diffText = piDiff.diff;
    firstChangedLine = piDiff.firstChangedLine;
  }

  // Agent-facing text: compact summary header (shared with OpenCode via
  // formatEditSummary) + conditional notices below. The header deliberately
  // omits the file path and the diff body: the agent supplied both the path
  // and the content, so echoing them back wastes tokens proportional to file
  // size. The line-numbered diff stays in `details.diff` for the TUI renderer
  // (which sources the path from the call args, not this text). This keeps the
  // OpenCode and Pi agent-facing edit output byte-identical in shape.
  let text = formatEditSummary(response as Record<string, unknown>);
  if (noOp) {
    // Surface the no-op signal explicitly so the agent can distinguish "the
    // tool failed silently" from "the edit matched but produced no net change".
    // Common causes: oldString equals newString, or a formatter normalized
    // the replacement back to the original.
    text +=
      "\n\nNote: no net file change \u2014 the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.";
  }
  // Surface non-benign format-skip reasons in agent-facing text. Benign
  // reasons (no formatter configured for the language, language unsupported)
  // are silent because the agent can't act on them. The actionable reasons
  // — formatter binary missing, formatter timed out, formatter crashed,
  // formatter excluded the path via project config — get a one-line note
  // pointing at the right remediation.
  const skipNote = formatSkipReasonNote(formatSkippedReason);
  if (skipNote) text += `\n\n${skipNote}`;
  const globSkipNote = formatGlobSkipReasonsNote(globFormatSkipReasons);
  if (globSkipNote) text += `\n\n${globSkipNote}`;
  if (diagnostics && diagnostics.length > 0) {
    text += `\n\nLSP diagnostics:\n${formatDiagnosticsText(diagnostics)}`;
  }

  return {
    content: [{ type: "text", text }],
    details: {
      diff: diffText,
      firstChangedLine,
      additions,
      deletions,
      replacements,
      diagnostics,
      truncated: truncated || undefined,
      formatted,
      formatSkippedReason,
      noOp: noOp || undefined,
    },
  };
}

function formatGlobSkipReasonsNote(reasons: unknown): string | undefined {
  if (!Array.isArray(reasons)) return undefined;
  const actionable = reasons
    .filter((reason): reason is string => typeof reason === "string")
    .filter((reason) =>
      ["formatter_not_installed", "formatter_excluded_path", "timeout", "error"].includes(reason),
    );
  if (actionable.length === 0) return undefined;
  return `Note: formatter skipped some glob edit result file(s): ${[...new Set(actionable)].sort().join(", ")}. See per-file format_skipped_reason values for details.`;
}

/**
 * Build a one-line agent-facing note for a non-benign format skip reason.
 * Returns undefined for benign reasons (no message worth surfacing) so the
 * caller can skip emitting a section header.
 */
function formatSkipReasonNote(reason: string | undefined): string | undefined {
  switch (reason) {
    case "formatter_not_installed":
      return "Note: formatter binary not installed; file written unformatted.";
    case "timeout":
      return "Note: formatter timed out; file written unformatted. Raise formatter_timeout_secs or check the formatter for hangs.";
    case "formatter_excluded_path":
      return "Note: formatter is configured to ignore this path (e.g. biome.json files.includes, .prettierignore). File written unformatted.";
    case "error":
      return "Note: formatter exited with an unrecognized error; file written unformatted.";
    default:
      // unsupported_language, no_formatter_configured, undefined → silent
      return undefined;
  }
}

function formatDiagnosticsText(diagnostics: unknown[]): string {
  // Diagnostics come back as an array of { line, severity, message, ... }.
  // Keep the format compact and human-readable; fall back to JSON if shape
  // is unexpected.
  try {
    return diagnostics
      .map((d) => {
        if (d && typeof d === "object") {
          const obj = d as Record<string, unknown>;
          const line = obj.line ?? obj.startLine ?? "?";
          const severity = obj.severity ?? "info";
          const msg = obj.message ?? JSON.stringify(obj);
          return `  [${severity}] line ${line}: ${msg}`;
        }
        return `  ${String(d)}`;
      })
      .join("\n");
  } catch {
    return JSON.stringify(diagnostics, null, 2);
  }
}

/**
 * Reuse a compatible `Text` from `lastComponent`, or create a fresh one.
 * The runtime `instanceof` guard prevents a cross-branch re-render from
 * trying to use a `Container` as a `Text` (or vice versa) — today Pi keeps
 * call/result slots separate and each slot's branch is stable per call, so
 * this is defensive hardening rather than a current-bug fix.
 */
function reuseText(last: Component | undefined): Text {
  return last instanceof Text ? last : new Text("", 0, 0);
}

function reuseContainer(last: Component | undefined): Container {
  return last instanceof Container ? last : new Container();
}

function renderMutationCall(
  toolName: "write" | "edit",
  filePath: string | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const pathDisplay = filePath
    ? theme.fg("accent", shortenPath(filePath))
    : theme.fg("toolOutput", "...");
  text.setText(`${theme.fg("toolTitle", theme.bold(toolName))} ${pathDisplay}`);
  return text;
}

function renderMutationResult(
  result: AgentToolResult<FileMutationDetails>,
  theme: Theme,
  context: RenderContextLike,
): Container | Text {
  // Errors: red text.
  if (context.isError) {
    const errorText = result.content
      .filter((c) => c.type === "text")
      .map((c) => (c as { text?: string }).text ?? "")
      .join("\n")
      .trim();
    const text = reuseText(context.lastComponent);
    text.setText(`\n${theme.fg("error", errorText || "edit failed")}`);
    return text;
  }

  const details = result.details;
  const diff = typeof details?.diff === "string" ? details.diff : undefined;

  // No diff (no-op edit or truncated diff): one-line summary. Truncation is
  // surfaced explicitly in muted text so the user isn't misled into thinking
  // a tiny summary reflects a tiny change. v0.27.1: when Rust signaled
  // `no_op: true`, attach a clear "no net change" suffix instead of a bare
  // `+0/-0` so the user can tell the agent's edit matched but produced no
  // file change (oldString === newString, or formatter reverted the diff).
  // See GitHub #45.
  if (!diff) {
    const additions = details?.additions ?? 0;
    const deletions = details?.deletions ?? 0;
    const text = reuseText(context.lastComponent);
    const summary = theme.fg("success", `+${additions}/-${deletions}`);
    let suffix = "";
    if (details?.truncated) {
      suffix = ` ${theme.fg("muted", "(diff truncated)")}`;
    } else if (details?.noOp) {
      suffix = ` ${theme.fg("muted", "(no net change)")}`;
    }
    text.setText(`\n${summary}${suffix}`);
    return text;
  }

  // Diff: render using Pi's built-in renderer for colored lines + intra-line
  // highlighting, wrapped in a Container with a top spacer for breathing room.
  const container = reuseContainer(context.lastComponent);
  container.clear();
  container.addChild(new Spacer(1));
  container.addChild(new Text(renderDiff(diff), 1, 0));
  return container;
}

function shortenPath(path: string): string {
  const home = homedir();
  if (path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}

/** Resolve a path argument to an absolute path if it exists, expanding `~`. */
export async function resolvePathArg(cwd: string, path: string): Promise<string> {
  const expanded = expandTilde(path);
  const abs = absoluteSearchPath(cwd, path);
  try {
    await stat(abs);
    return abs;
  } catch {
    return expanded;
  }
}

/**
 * Brace-aware split for OpenCode-style include args.
 *
 * Accepts:
 *   - "*.ts,*.tsx"            (comma-separated includes)
 *   - "**\/*.{vue,ts,tsx}"    (single glob with brace alternation)
 *   - "*.ts,**\/*.{vue,tsx}"  (mix of both)
 *
 * A naive split-by-`,` would chop `*.{vue,ts}` into `*.{vue` + `ts}`,
 * which then fails downstream globbing with
 * `unclosed alternate group; missing '}'`.
 */
export function splitIncludeGlobs(include: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let buf = "";
  for (const ch of include) {
    if (ch === "{") {
      depth++;
      buf += ch;
      continue;
    }
    if (ch === "}") {
      if (depth > 0) depth--;
      buf += ch;
      continue;
    }
    if (ch === "," && depth === 0) {
      const trimmed = buf.trim();
      if (trimmed.length > 0) out.push(trimmed);
      buf = "";
      continue;
    }
    buf += ch;
  }
  const tail = buf.trim();
  if (tail.length > 0) out.push(tail);
  return out;
}

/**
 * Build the navigation footer for a `read` response.
 *
 * The pure clamping/range logic lives in aft-bridge. Pi keeps the
 * host-specific parameter hint (`offset/limit`) here so existing agent-facing
 * output stays byte-for-byte identical.
 */
export function formatReadFooter(
  agentSpecifiedRange: boolean,
  data: Record<string, unknown>,
): string {
  return formatSharedReadFooter(agentSpecifiedRange, data, { rangeHint: "offset/limit" });
}
