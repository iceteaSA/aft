import type { AftProjectTransport, AftTransportPool } from "@cortexkit/aft-bridge";
import { log, warn } from "./logger.js";
import { BASH_TRANSPORT_TIMEOUT_MS } from "./tools/_shared.js";

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot" | "activeBridges">;

async function sendBashWaitDetach(
  bridge: AftProjectTransport,
  sessionID: string,
): Promise<boolean> {
  const response = await bridge.send(
    "bash_wait_detach",
    { session_id: sessionID },
    { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
  );
  if (response.success === false) {
    throw new Error(String(response.message ?? "bash_wait_detach failed"));
  }
  // success:true with detached:false means no active wait was found under
  // this session on that bridge — possibly the wrong bridge. Callers use
  // this to keep fanning out instead of treating delivery as done.
  return response.detached === true;
}

export async function signalBashWaitDetachForProject(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  // Try the exact root first, but keep fanning out while no bridge reports an
  // actually-detached wait: success:true + detached:false from the exact-root
  // bridge means the wait lives elsewhere (root-key mismatch), not "done".
  const exact = pool.getActiveBridgeForRoot(projectRoot);
  const all = pool.activeBridges();
  const targets = exact ? [exact, ...all.filter((bridge) => bridge !== exact)] : all;
  if (targets.length === 0) {
    warn(`[bash_wait_detach] no live bridge for session ${sessionID} (root ${projectRoot})`);
    return;
  }
  let signaled = 0;
  let lastError: unknown = null;
  for (const bridge of targets) {
    try {
      signaled += 1;
      if (await sendBashWaitDetach(bridge, sessionID)) {
        log(
          `[bash_wait_detach] detached wait for session ${sessionID} (bridge ${signaled}/${targets.length})`,
        );
        return;
      }
    } catch (err) {
      lastError = err;
    }
  }
  if (lastError !== null) {
    warn(
      `[bash_wait_detach] failed for session ${sessionID}: ${lastError instanceof Error ? lastError.message : String(lastError)}`,
    );
  } else {
    log(
      `[bash_wait_detach] no active wait found for session ${sessionID} (signaled ${signaled} bridge(s))`,
    );
  }
}
