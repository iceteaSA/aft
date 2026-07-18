#!/usr/bin/env bun
/**
 * Fleet-audit probe: fetch the daemon-served AFT module's status snapshot
 * (including runtime counters: live_watchers / live_actor_roots / open_routes)
 * over the subc route, without needing a live plugin session.
 *
 * Run: bun packages/aft-bridge/scripts/runtime-counters-probe.ts \
 *        [connection-file] [project-root]
 * Defaults: the standard CortexKit subc connection file and the aft repo.
 */

import { homedir } from "node:os";
import { join } from "node:path";
import { SubcTransportPool } from "../src/subc-transport.js";

const CONNECTION_FILE =
  process.argv[2] ?? join(homedir(), ".local/share/cortexkit/subc/run/subc-connection.json");
const PROJECT_ROOT = process.argv[3] ?? process.cwd();
const SESSION = `runtime-counters-probe-${Date.now().toString(36)}`;

async function main(): Promise<void> {
  const pool = new SubcTransportPool({
    connectionFile: CONNECTION_FILE,
    harness: "opencode",
    onBgEventsNudge: () => {},
  });
  try {
    const bridge = pool.getBridge(PROJECT_ROOT);
    // The subc envelope re-lifts the full flat response: status fields sit at
    // the TOP LEVEL of the result, not under .data.
    const result = (await bridge.toolCall(SESSION, "status", {})) as unknown as Record<
      string,
      unknown
    >;
    const runtime = (result.runtime ?? {}) as Record<string, unknown>;
    const memory = (result.memory ?? {}) as Record<string, unknown>;
    console.log(
      JSON.stringify(
        { version: result.version, runtime, memory_summary: memory.process ?? memory },
        null,
        2,
      ),
    );
  } finally {
    await pool.shutdown();
  }
}

main().catch((error) => {
  console.error(`probe failed: ${error}`);
  process.exit(1);
});
