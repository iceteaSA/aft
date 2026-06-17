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
  /** `bash.background` — gates background-bash paragraph. */
  bashBackgroundEnabled: boolean;
  /** Resolved bash compression flag. */
  bashCompressionEnabled: boolean;
  /** Set of disabled tool names (after surface filtering). */
  disabledTools: Set<string>;
}

const HEADING = "## IMPORTANT NOTICE about your tools";

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

  if (hasBash && opts.bashCompressionEnabled) {
    // The section itself is config-gated, so the text never hedges with
    // "when compression is on" — the agent can't check the config; we can.
    sections.push(
      [
        "**Test/build output**: bash output is auto-compressed — failures and the summary are always kept. DO NOT pipe test/build commands through filters to summarize; that hides failures:",
        "- `bun test | grep fail` → run `bun test`",
        "- `cargo test 2>&1 | tail -20` → run `cargo test`",
        "- `npm run build | head -50` → run `npm run build`",
      ].join("\n"),
    );
  }

  // Web/URL access — needs aft_outline + aft_zoom.
  if (hasOutline && hasZoom) {
    sections.push(
      `**Web/URL access**: \`aft_outline({ target: url })\` first for structure, then \`aft_zoom({ url, symbols: "<heading>" })\` for the specific section.`,
    );
  }

  // Code exploration — needs at least aft_outline + aft_zoom + (grep or aft_search).
  // Lead with the two behaviors agents reliably get wrong: serializing
  // independent lookups, and shelling out to grep for code search. Both are
  // stated imperatively (DO NOT) because soft "prefer" wording does not change
  // the reflex. When aft_search is available it is named alone — it auto-routes
  // literals too, so naming the grep tool would only dilute the redirect; only
  // when aft_search is absent do we point at the grep TOOL as the indexed,
  // ranked alternative to raw bash grep.
  if (hasOutline && hasZoom && (hasGrep || hasSearch)) {
    const searchName = hasSearch ? "aft_search" : grepName;
    const locate = hasSearch
      ? '`aft_search` is the primary code-search tool: one call auto-routes concepts, identifiers, regex, error strings, and literals (pass `hint: "regex"`/`"literal"`/`"semantic"` to force a lane).'
      : `\`${grepName}\` (the tool — indexed and ranked) locates code.`;
    const readName = opts.hoistBuiltins ? "read" : "aft_read";
    sections.push(
      [
        `**Code exploration**: ${locate} Then \`aft_outline\` for structure → \`aft_zoom\` for symbol(s). DO NOT run \`grep\`/\`rg\`/\`find\`/\`sed\`/\`cat\` through \`bash\` to locate or read code — the bash path is unindexed, unranked, serial, and routinely surfaces the wrong hit. Keep \`bash\` for shell facts (git state, file metadata, processes). Reflex translations:`,
        `- \`grep -rn "handleAuth" src/\` in bash → \`${searchName}({ query: "handleAuth" })\``,
        `- \`find . -name "*.ts" | xargs grep watcher\` in bash → \`${searchName}({ query: "watcher invalidation" })\` (concepts work too)`,
        `- \`sed -n '100,160p' app.ts\` / \`cat app.ts\` in bash → \`${readName}({ filePath: "app.ts", startLine: 100, endLine: 160 })\``,
      ].join("\n"),
    );
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
    // Positive-first framing (user feedback: "the admonishment to not poll
    // focuses on what NOT to do, rather than what TO do" — models acknowledged
    // the rule, then polled anyway). Give the waiting urge three sanctioned
    // outlets and give bash_status a legitimate role, so it isn't forbidden
    // fruit the model reaches for the moment it feels blocked.
    sections.push(
      [
        `**Long-running commands** (builds, installs, full test suites): \`${bashName}({ background: true })\` returns a \`taskId\` immediately. Then do ONE of these:`,
        "1. Keep working on something independent of the result.",
        "2. End your turn — a completion reminder with the result arrives automatically.",
        `3. Need to react to a specific output line early? Register \`bash_watch({ taskId, pattern, background: true })\`, then end your turn — it pings you the moment the pattern appears.`,
        `\`${bashStatusName}({ taskId })\` is for inspecting output AFTER the reminder arrives, or one quick look at a live task — never call it repeatedly to wait: the reminder already delivers the result, and each poll wastes a turn. Sync \`bash_watch\` (without \`background: true\`) blocks your turn and locks the user out — reserve it for waits of a few seconds (a dev server printing its ready line), never for builds or test suites.`,
      ].join("\n"),
    );
    sections.push(
      `**PTY / interactive commands**: PTY mode is for interactive REPLs and terminal apps (python, node, bash itself, vim). Start with \`${bashName}({ command: "python", pty: true, background: true })\`, read the screen with \`${bashStatusName}({ taskId, outputMode: "screen" })\`, and send input with \`${bashWriteName}({ taskId, input: "..." })\`.`,
    );
  }

  if (sections.length === 0) {
    return null;
  }

  // The opening notice frames the whole block: these are not ordinary
  // CLI-equivalent tools, and the single biggest efficiency win is firing
  // independent read-only calls together. Prepended so it leads, and only
  // when there's real content below it (never emitted alone).
  sections.unshift(
    "You are equipped with a non-standard tool set: indexed code search, symbol-level reading, structural editing, and code analysis that are faster, more precise, and far cheaper in tokens than stitching together command-line utilities in bash. Always reach for these tools first.\n\n**Parallel tool calls**: when several read-only operations are independent, emit them in ONE response instead of serializing — file reads, structure and symbol lookups, code search, diagnostics, and git status/diff/log. Sequence only when a call depends on a prior result or when a command mutates state.",
  );

  return `${HEADING}\n\n${sections.join("\n\n")}`;
}

/**
 * Resolve workflow-hints opts from a loaded AftConfig and the active
 * disabled-tools set computed at registration time.
 *
 * Background-bash gating reads the resolved bash config so the graduated
 * `bash.background` setting controls whether the hint appears.
 */
export function buildHintsFromConfig(config: AftConfig, disabledTools: Set<string>): string | null {
  return buildWorkflowHints({
    toolSurface: config.tool_surface ?? "recommended",
    hoistBuiltins: config.hoist_builtin_tools !== false,
    semanticEnabled: config.semantic_search === true,
    bashBackgroundEnabled: resolveBashConfig(config).background,
    bashCompressionEnabled: resolveBashConfig(config).compress,
    disabledTools,
  });
}
