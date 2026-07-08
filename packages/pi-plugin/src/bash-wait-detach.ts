import type { AftProjectTransport, AftTransportPool } from "@cortexkit/aft-bridge";
import { warn } from "./logger.js";

const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot">;

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
  const bridge = pool.getActiveBridgeForRoot(projectRoot);
  if (!bridge) return;
  try {
    await sendBashWaitDetach(bridge, sessionID);
  } catch (err) {
    warn(
      `[bash_wait_detach] failed for session ${sessionID}: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}
