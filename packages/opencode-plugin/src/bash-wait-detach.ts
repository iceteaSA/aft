import type { AftProjectTransport, AftTransportPool } from "@cortexkit/aft-bridge";
import { log, warn } from "./logger.js";
import { BASH_TRANSPORT_TIMEOUT_MS } from "./tools/_shared.js";

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot" | "activeBridges">;

async function sendBashWaitDetach(bridge: AftProjectTransport, sessionID: string): Promise<void> {
  const response = await bridge.send(
    "bash_wait_detach",
    { session_id: sessionID },
    { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
  );
  if (response.success === false) {
    throw new Error(String(response.message ?? "bash_wait_detach failed"));
  }
}

export async function signalBashWaitDetachForProject(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  // Root-key resolution can disagree with the key the bash tool call used
  // (canonicalization, worktrees, cwd fallbacks). The command is scoped by
  // session_id on the Rust side, so when the exact root has no live bridge,
  // fan out to every live bridge instead of silently dropping the signal.
  const exact = pool.getActiveBridgeForRoot(projectRoot);
  const targets = exact ? [exact] : pool.activeBridges();
  if (targets.length === 0) {
    warn(`[bash_wait_detach] no live bridge for session ${sessionID} (root ${projectRoot})`);
    return;
  }
  const results = await Promise.allSettled(
    targets.map((bridge: AftProjectTransport) => sendBashWaitDetach(bridge, sessionID)),
  );
  const failed = results.filter((result: PromiseSettledResult<void>) => result.status === "rejected");
  if (failed.length === results.length) {
    const err = (failed[0] as PromiseRejectedResult).reason;
    warn(
      `[bash_wait_detach] failed for session ${sessionID}: ${err instanceof Error ? err.message : String(err)}`,
    );
  } else {
    log(
      `[bash_wait_detach] signaled for session ${sessionID} via ${results.length - failed.length} bridge(s)${exact ? "" : " (root-key fallback)"}`,
    );
  }
}
