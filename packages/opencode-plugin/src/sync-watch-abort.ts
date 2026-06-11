/**
 * Per-session abort flags for sync bash_watch waits.
 *
 * When a user sends a message while the agent is blocked in a sync
 * bash_watch wait, the `chat.message` hook sets an abort flag for that
 * session. The sync wait poll loop checks the flag each iteration and
 * on abort: (1) stops blocking, (2) auto-registers the equivalent async
 * watch so the completion/pattern notification still arrives, (3) returns
 * text telling the agent the wait converted to async.
 *
 * Edge cases:
 * - The flag is cleared at sync-wait start so stale flags from previous
 *   turns don't insta-abort a new wait.
 * - A wait that already matched/exited wins over abort (the flag is
 *   checked only at the top of the poll loop, after match/exit checks).
 * - Subagent sessions behave identically — the flag is keyed by sessionID.
 */

const abortFlags = new Map<string, boolean>();

/** Signal that a sync watch for this session should abort. */
export function signalSyncWatchAbort(sessionID: string | undefined): void {
  const key = sessionID || "__default__";
  abortFlags.set(key, true);
}

/** Clear any stale abort flag at the start of a new sync wait. */
export function clearSyncWatchAbort(sessionID: string | undefined): void {
  const key = sessionID || "__default__";
  abortFlags.delete(key);
}

/** Check whether the abort flag is set for this session. Does NOT clear it. */
export function isSyncWatchAborted(sessionID: string | undefined): boolean {
  const key = sessionID || "__default__";
  return abortFlags.get(key) === true;
}

/** Test-only: reset all abort flags. */
export function __resetSyncWatchAbortForTests(): void {
  abortFlags.clear();
}
