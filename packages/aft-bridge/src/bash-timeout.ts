// Shared resolution of the hard-kill timeout sent to the bridge for a bash
// command. Used by both harness plugins so the semantics stay identical.

/**
 * Resolve the hard-kill timeout to forward to the bridge for a foreground bash
 * command.
 *
 * A model-supplied `timeout` shorter than the foreground wait window is
 * incoherent: the task would be killed before (or exactly at) the moment we
 * promote it to background, which is never what the caller wants. This is the
 * #102 bug, where a model passed `timeout: 100` and the command was killed at
 * 100ms while `foreground_wait_window_ms` was silently overridden to 100ms by a
 * `Math.min(timeout, window)`.
 *
 * Treat such sub-window values as unset (return `undefined`) so the bridge
 * applies its default 30-minute kill cap and the foreground poll runs the full
 * wait window. A `timeout` at or above the window is a coherent cap and is
 * honored as-is. Callers use the returned value for the bridge payload, the
 * promotion message, and the subagent inline-wait, so all three agree.
 */
export function resolveBashKillTimeout(
  modelTimeout: number | undefined,
  foregroundWaitMs: number,
): number | undefined {
  if (modelTimeout !== undefined && modelTimeout >= foregroundWaitMs) {
    return modelTimeout;
  }
  return undefined;
}
