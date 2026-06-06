// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows. Mirrors packages/opencode-plugin/src/workflow-hints.ts;
// scheduled to consolidate into a shared package in v0.19 alongside the
// bridge-extraction refactor (see ctx_note #53).
// ---------------------------------------------------------------------------

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { log } from "./logger.js";

export interface WorkflowHintsOpts {
  toolSurface: "minimal" | "recommended" | "all";
  hoistBuiltins: boolean;
  semanticEnabled: boolean;
  bashBackgroundEnabled: boolean;
  bashCompressionEnabled: boolean;
  /** Set of tool names KNOWN-ABSENT from the registered surface. */
  absentTools: Set<string>;
}

const HEADING = "## Prefer AFT tools for token efficiency";

export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const sections: string[] = [];

  // Pi: hoisted built-ins keep their original names (read/grep/bash).
  // Non-hoisted Pi mode is currently not supported — Pi installs hoisted
  // wrappers unconditionally — but we keep the toggle for parity with the
  // OpenCode plugin and v0.19 shared-package extraction.
  const grepName = opts.hoistBuiltins ? "grep" : "aft_grep";
  const bashName = opts.hoistBuiltins ? "bash" : "aft_bash";

  const hasOutline = !opts.absentTools.has("aft_outline");
  const hasZoom = !opts.absentTools.has("aft_zoom");
  const hasGrep = opts.toolSurface !== "minimal" && !opts.absentTools.has(grepName);
  const hasSearch =
    opts.toolSurface !== "minimal" && opts.semanticEnabled && !opts.absentTools.has("aft_search");
  const hasNavigate = opts.toolSurface === "all" && !opts.absentTools.has("aft_callgraph");
  const hasInspect = opts.toolSurface !== "minimal" && !opts.absentTools.has("aft_inspect");
  const hasBash = !opts.absentTools.has(bashName);
  const hasBgBash = opts.bashBackgroundEnabled && hasBash && !opts.absentTools.has("bash_status");

  if (hasBash && opts.bashCompressionEnabled) {
    sections.push(
      "When AFT bash output compression is on, do NOT pipe test/build commands through grep/head/tail (e.g. `bun test | grep fail`) to summarize. The compressor already keeps failures and the summary; piping hides failures. Run the command bare.",
    );
  }

  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ target: "<url>" })\` first for structure, then \`aft_zoom({ url: "<url>", symbols: "<heading>" })\` for the specific section.`,
    );
  }

  // See the OpenCode copy for the rationale — kept byte-identical for parity.
  // Lead imperatively (DO NOT) with the two reflexes agents get wrong:
  // serializing independent lookups, and shelling out to grep for code search.
  // aft_search is named alone when available (it auto-routes literals too);
  // only when absent do we point at the grep TOOL.
  if (hasOutline && hasZoom && (hasGrep || hasSearch)) {
    const locate = hasSearch
      ? '`aft_search` is the primary code-search tool: one call auto-routes concepts, identifiers, regex, error strings, and literals (pass `hint: "regex"`/`"literal"`/`"semantic"` to force a lane).'
      : `\`${grepName}\` (the tool — indexed and ranked) locates code.`;
    sections.push(
      `**Code exploration**: fire independent lookups in ONE parallel tool-call wave — do NOT serialize them. ${locate} Then \`aft_outline\` for structure → \`aft_zoom\` for symbol(s). DO NOT run \`grep\`/\`rg\`/\`find\` through \`bash\` to locate code — the bash path is unindexed, unranked, serial, and routinely surfaces the wrong hit. Keep \`bash\` for shell facts (git state, file metadata, processes).`,
    );
  }

  if (hasInspect) {
    sections.push(
      "**Codebase health & diagnostics**: AFT does not surface compile/type errors automatically after edits — pull them with `aft_inspect`. Run it after a batch of edits and before you run tests or commit, when starting in unfamiliar code, or before a refactor/review. One call summarizes diagnostics (compile/type errors), TODOs, metrics, dead code, unused exports, and duplicates; pass `sections` for focused drill-down and `scope` to actively pull diagnostics for a specific file or directory. Its diagnostics are a fast checkpoint, not the authority — a clean `tsc` / `cargo check` / `pyright` run is the real gate. Treat `stale_categories` as a genuine stale-cache signal while an async Tier 2 refresh catches up.",
    );
    sections.push(
      "**AFT status bar**: tool results may end with a one-line health bar `[AFT E<errors> W<warnings> | D<dead-code> U<unused-exports> C<clone/dup-groups> | T<todos>]` — an IDE-style glance that appears when a count changes. `E`/`W` are live LSP diagnostics for files touched this session (your universal compile-error signal across every language with an LSP). A `~` before `D` means the dead-code/unused/dup counts predate your latest edit — run `aft_inspect` for current numbers and detail. When `E>0`, you likely just introduced errors; investigate before moving on.",
    );
  }

  if (hasNavigate) {
    sections.push(
      [
        "Use `aft_callgraph` for code-relationship questions instead of grep + read chains:",
        "- `callers` — find all call sites before changing a function signature",
        "- `impact` — blast radius (which functions/files will need updates)",
        "- `trace_to` — how execution reaches this code from entry points (routes, exports, main)",
        "- `trace_to_symbol` — shortest call path from one symbol to another",
        "- `trace_data` — follow a value through assignments and parameters across files",
      ].join("\n"),
    );
  }

  if (hasBgBash) {
    sections.push(
      `**Long-running commands** (builds, installs, full test suites): \`${bashName}({ background: true })\` returns immediately with a \`task_id\`, then **end your turn** — a completion reminder arrives automatically when it finishes. Do not poll \`bash_status({ task_id })\`, and do not sync-wait with \`bash_watch\` for a long task: blocking freezes your turn and locks the user out until it ends. For an early non-blocking ping on a specific output line, register an async watch \`bash_watch({ task_id, pattern, background: true })\`. \`bash_watch\` synchronous mode is only for short bounded waits (seconds, e.g. a dev server printing a readiness line), never for multi-minute jobs.`,
    );
    sections.push(
      `**PTY / interactive commands**: PTY mode is for interactive REPLs and terminal apps (python, node, bash itself, vim). Start with \`${bashName}({ command: "python", pty: true, background: true })\`, read the screen with \`bash_status({ task_id, output_mode: "screen" })\`, and send input with \`bash_write({ task_id, input: "..." })\`.`,
    );
  }

  if (sections.length === 0) {
    return null;
  }

  // Parallel-tool-call discipline frames the whole block (parity with OpenCode):
  // firing independent read-only calls together is the single biggest efficiency
  // win. Prepended so it leads, and only when there's real content below it.
  sections.unshift(
    "**Parallel tool calls**: when several read-only operations are independent, emit them in ONE response instead of serializing — file reads, structure and symbol lookups, code search, diagnostics, and git status/diff/log. Sequence only when a call depends on a prior result or when a command mutates state.",
  );

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

export function buildHintsFromConfig(
  config: AftConfig,
  absentTools: Set<string>,
  hoistBuiltins: boolean,
): string | null {
  // Background-bash gating reads the resolved bash config so the new
  // graduated `bash: true` / `bash: { background: true }` shapes enable
  // the hint, not just the legacy `experimental.bash.background: true`
  // path. See `resolveBashConfig` in config.ts.
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    bashCompressionEnabled: resolveBashConfig(config).compress,
    absentTools,
  });
}

// ---------------------------------------------------------------------------
// Pi extension registration
// ---------------------------------------------------------------------------

interface ToolSurfaceFlags {
  outline: boolean;
  zoom: boolean;
  semantic: boolean;
  navigate: boolean;
  inspect: boolean;
  hoistGrep: boolean;
  hoistBash: boolean;
}

/**
 * Register the workflow-hints extension on Pi via `before_agent_start`.
 *
 * Pi assembles a fresh system prompt for every turn, then fires
 * `before_agent_start` with the assembled prompt. Our handler appends the
 * AFT workflow hints block to that prompt. If multiple extensions return a
 * `systemPrompt`, Pi chains them — so we always append (never replace).
 */
export function registerWorkflowHints(
  pi: ExtensionAPI,
  config: AftConfig,
  surface: ToolSurfaceFlags,
): void {
  // Build the absent-tools set from the resolved tool surface. Pi always
  // hoists built-ins (read/grep/bash), so `hoistBuiltins=true`.
  const absent = new Set<string>();
  if (!surface.outline) absent.add("aft_outline");
  if (!surface.zoom) absent.add("aft_zoom");
  if (!surface.semantic) absent.add("aft_search");
  if (!surface.navigate) absent.add("aft_callgraph");
  if (!surface.inspect) absent.add("aft_inspect");
  if (!surface.hoistGrep) absent.add("grep");
  if (!surface.hoistBash) {
    absent.add("bash");
    absent.add("bash_status");
  }

  const hintsBlock = buildHintsFromConfig(config, absent, /* hoistBuiltins */ true);
  if (!hintsBlock) return;

  log(`Workflow hints injected (${hintsBlock.length} chars)`);

  // Pi's `before_agent_start` handler can return `systemPrompt` to chain
  // an additional system prompt onto the assembled one. We always APPEND
  // — never overwrite — so other extensions' prompt contributions survive.
  (
    pi.on as (
      event: "before_agent_start",
      handler: (event: { systemPrompt: string }) => unknown,
    ) => void
  )("before_agent_start", (event) => {
    return { systemPrompt: `${event.systemPrompt}\n\n${hintsBlock}` };
  });
}
