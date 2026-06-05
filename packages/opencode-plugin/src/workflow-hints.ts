// ---------------------------------------------------------------------------
// Workflow hints — short system prompt block teaching the agent
// token-efficient AFT workflows.
//
// Conditional on the actual tool surface so we never advertise tools the
// agent doesn't have. Tool name resolution honors `hoist_builtin_tools`:
// when hoisting is on (default) the agent sees `read`/`grep`/`bash`; when
// off it sees `aft_read`/`aft_grep`/`aft_bash`.
// ---------------------------------------------------------------------------

import { type AftConfig, resolveBashConfig } from "./config.js";

export interface WorkflowHintsOpts {
  /** `tool_surface` setting — controls which tools are registered. */
  toolSurface: "minimal" | "recommended" | "all";
  /** `hoist_builtin_tools` setting — affects tool name (read vs aft_read). */
  hoistBuiltins: boolean;
  /** `experimental.semantic_search` — gates `aft_search` mention. */
  semanticEnabled: boolean;
  /** `experimental.bash.background` — gates background-bash paragraph. */
  bashBackgroundEnabled: boolean;
  /** Set of disabled tool names (after surface filtering). */
  disabledTools: Set<string>;
}

const HEADING = "## Prefer AFT tools for token efficiency";

/**
 * Build the workflow hints block. Returns `null` when no hints are
 * applicable for the configured surface (e.g. `tool_surface: "minimal"`
 * with no aft_outline/aft_zoom available — only safety tool is registered).
 */
export function buildWorkflowHints(opts: WorkflowHintsOpts): string | null {
  const sections: string[] = [];

  // Tool name resolution. When hoisting is on, OpenCode sees built-in
  // names; when off, agent-visible names are aft-prefixed.
  const grepName = opts.hoistBuiltins ? "grep" : "aft_grep";
  const bashName = opts.hoistBuiltins ? "bash" : "aft_bash";
  const bashStatusName = "bash_status";
  const bashWriteName = "bash_write";

  // aft_outline and aft_zoom are present at "minimal" + above. They're never
  // hoisted (always aft-prefixed).
  const hasOutline = !opts.disabledTools.has("aft_outline");
  const hasZoom = !opts.disabledTools.has("aft_zoom");
  const hasGrep = opts.toolSurface !== "minimal" && !opts.disabledTools.has(grepName);
  const hasSearch =
    opts.toolSurface !== "minimal" && opts.semanticEnabled && !opts.disabledTools.has("aft_search");
  // aft_callgraph is "all"-tier only.
  const hasNavigate = opts.toolSurface === "all" && !opts.disabledTools.has("aft_callgraph");
  const hasInspect = opts.toolSurface !== "minimal" && !opts.disabledTools.has("aft_inspect");
  const hasBash = !opts.disabledTools.has(bashName);
  const hasBgBash =
    hasBash && opts.bashBackgroundEnabled && !opts.disabledTools.has(bashStatusName);

  // Web/URL access — needs aft_outline + aft_zoom.
  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ target: url })\` first for structure, then \`aft_zoom({ url, symbols: "<heading>" })\` for the specific section.`,
    );
  }

  // Code exploration — needs at least aft_outline + aft_zoom + (grep or aft_search).
  if (hasOutline && hasZoom && (hasGrep || hasSearch)) {
    let codeExploration: string;
    if (hasSearch) {
      const grepFallback = hasGrep
        ? ` Use \`${grepName}\` directly only when you need exhaustive enumeration of literal text (every TODO, every import of X) without ranking.`
        : "";
      codeExploration = `**Code exploration**: \`aft_search\` is the primary code-search tool. It auto-routes by query shape — exact identifiers, regex, error messages, natural language all use the same call. Very short queries fall back to literal scans; pass \`hint: "regex"\` / \`hint: "literal"\` / \`hint: "semantic"\` to override routing if needed. Then \`aft_outline\` for structure → \`aft_zoom\` for symbol(s).${grepFallback}`;
    } else {
      codeExploration = `**Code exploration**: \`${grepName}\` to locate → \`aft_outline\` for structure → \`aft_zoom\` for symbol(s).`;
    }
    // Generic anti-pattern steer (all models): locating code by batching
    // grep/find/sed in one bash call is unranked and routinely picks the wrong
    // hit. Route discovery through the AFT tools, fired in parallel.
    codeExploration +=
      " For code discovery, call these AFT tools (in parallel when the lookups are independent) rather than batching `grep`/`find`/`sed` in one `bash` call: the raw-bash batch is unranked and routinely surfaces the wrong code. Keep `bash` for shell facts (git state, file metadata, running commands).";
    sections.push(codeExploration);
  }

  // Codebase health & diagnostics — needs aft_inspect (recommended+).
  // Lead with the behavioral change: AFT no longer auto-surfaces compile/type
  // errors on edit, so the agent MUST pull them. Anchor to the edit→test/commit
  // moment, and be explicit that aft_inspect diagnostics are a checkpoint, not
  // the authority (the project checker is).
  if (hasInspect) {
    sections.push(
      "**Codebase health & diagnostics**: AFT does not surface compile/type errors automatically after edits — pull them with `aft_inspect`. Run it after a batch of edits and before you run tests or commit, when starting in unfamiliar code, or before a refactor/review. One call summarizes diagnostics (compile/type errors), TODOs, metrics, dead code, unused exports, and duplicates; pass `sections` for focused drill-down and `scope` to actively pull diagnostics for a specific file or directory. Its diagnostics are a fast checkpoint, not the authority — a clean `tsc` / `cargo check` / `pyright` run is the real gate. Treat `stale_categories` as a genuine stale-cache signal while an async Tier 2 refresh catches up.",
    );
    // Status-bar legend — taught once here so the per-call bar is just compact
    // values (~18 tokens). The bar is appended to tool results on change.
    sections.push(
      "**AFT status bar**: tool results may end with a one-line health bar `[AFT E<errors> W<warnings> | D<dead-code> U<unused-exports> C<clone/dup-groups> | T<todos>]` — an IDE-style glance that appears when a count changes. `E`/`W` are live LSP diagnostics for files touched this session (your universal compile-error signal across every language with an LSP). A `~` before `D` means the dead-code/unused/dup counts predate your latest edit — run `aft_inspect` for current numbers and detail. When `E>0`, you likely just introduced errors; investigate before moving on.",
    );
  }

  // Relationship questions — needs aft_callgraph ("all" surface).
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

  // Bash long-running guidance — only add the background-pattern hint when
  // background bash is enabled. Foreground bash now auto-promotes after a
  // short wait-window, so agents never need to know about timeouts up front;
  // there's no "30s default" to warn about anymore.
  if (hasBash && hasBgBash) {
    sections.push(
      `**Long-running commands** (builds, installs, full test suites): \`${bashName}({ background: true })\` returns immediately with a \`taskId\`, then **end your turn** — a completion reminder arrives automatically when it finishes. Do not poll \`${bashStatusName}({ taskId })\`, and do not sync-wait with \`bash_watch\` for a long task: blocking freezes your turn and locks the user out until it ends. For an early non-blocking ping on a specific output line, register an async watch \`bash_watch({ taskId, pattern, background: true })\`. \`bash_watch\` synchronous mode is only for short bounded waits (seconds, e.g. a dev server printing a readiness line), never for multi-minute jobs.`,
    );
    sections.push(
      `**PTY / interactive commands**: PTY mode is for interactive REPLs and terminal apps (python, node, bash itself, vim). Start with \`${bashName}({ command: "python", pty: true, background: true })\`, read the screen with \`${bashStatusName}({ taskId, outputMode: "screen" })\`, and send input with \`${bashWriteName}({ taskId, input: "..." })\`.`,
    );
  }

  if (sections.length === 0) {
    return null;
  }

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

/**
 * Resolve workflow-hints opts from a loaded AftConfig and the active
 * disabled-tools set computed at registration time.
 *
 * Background-bash gating reads the resolved bash config so the new
 * graduated `bash: true` / `bash: { background: true }` shapes enable the
 * hint, not just the legacy `experimental.bash.background: true` path.
 */
export function buildHintsFromConfig(config: AftConfig, disabledTools: Set<string>): string | null {
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins: config.hoist_builtin_tools !== false,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    disabledTools,
  });
}
