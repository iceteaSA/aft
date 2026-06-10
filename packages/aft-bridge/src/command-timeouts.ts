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
  trace_to: 60_000,
  trace_to_symbol: 60_000,
  trace_data: 60_000,
  impact: 60_000,
  grep: 60_000,
  glob: 60_000,
  semantic_search: 60_000,
};

/** Returns the per-command timeout override, or undefined to use the bridge default. */
export function timeoutForCommand(command: string): number | undefined {
  return LONG_RUNNING_COMMAND_TIMEOUT_MS[command];
}
