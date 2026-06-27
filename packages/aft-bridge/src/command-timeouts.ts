/**
 * Per-command transport timeout overrides (milliseconds), shared by every
 * harness adapter AND the bridge's own config clamping.
 *
 * Commands not listed fall back to the bridge-wide default (30s). Only extend
 * budgets for operations that legitimately walk the project file tree or wait
 * on external I/O (embedding API, index build). The goal is to absorb slow
 * first-call spikes without masking real hangs.
 *
 * This table lives in aft-bridge (not the plugins) so the semantic-timeout
 * clamp in bridge.ts and the per-call overrides in the plugins can never
 * drift apart again: the clamp must know the REAL transport budget of
 * `semantic_search`, which is this table's value — not the bridge default.
 */
export const LONG_RUNNING_COMMAND_TIMEOUT_MS: Record<string, number> = {
  callers: 60_000,
  callgraph: 60_000,
  trace_to: 60_000,
  trace_to_symbol: 60_000,
  trace_data: 60_000,
  impact: 60_000,
  inspect: 60_000,
  grep: 60_000,
  glob: 60_000,
  search: 60_000,
  semantic_search: 60_000,
};

/** Returns the per-command timeout override, or undefined to use the bridge default. */
export function timeoutForCommand(command: string): number | undefined {
  return LONG_RUNNING_COMMAND_TIMEOUT_MS[command];
}

/**
 * Passive health-check commands that must NEVER count toward bridge hang
 * escalation. The TUI sidebar polls `status` roughly every 1.5s; when the
 * bridge is busy with legitimate work (a Tier-2 dead_code scan, a big grep)
 * every request queues behind it. With the default 30s timeout and a hang
 * threshold of 2, two queued `status` polls time out and SIGKILL the bridge —
 * aborting the user's in-flight request that was waiting on the very same work
 * (issue #117: "the bridge keeps getting killed" during Edit/Read).
 *
 * A passive poll is not evidence of a hung bridge — it's evidence the bridge is
 * busy. These commands therefore (1) always get `keepBridgeOnTimeout` semantics
 * regardless of caller, so a timeout rejects only that poll and never kills, and
 * (2) use a short timeout so the poll falls back to the cached snapshot fast
 * instead of blocking the full 30s twice. State transitions still push fresh
 * snapshots (1s debounce), so a dropped poll costs nothing.
 */
export const PASSIVE_COMMANDS: ReadonlySet<string> = new Set(["status"]);

/** Short transport budget for passive polls — fail fast to cached, never block. */
export const PASSIVE_COMMAND_TIMEOUT_MS = 5_000;

/** True for passive health-check commands that must never escalate to a kill. */
export function isPassiveCommand(command: string): boolean {
  return PASSIVE_COMMANDS.has(command);
}
