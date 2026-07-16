/**
 * The SINGLE injection point that selects AFT's transport: standalone NDJSON
 * (spawn the `aft` binary, today's default) or the Subconscious daemon (talk to
 * AFT as a supervised module). Both plugins call this one factory so the choice
 * lives in exactly one place and everything downstream (tool registration,
 * hoisting, permission UI, sidebar) stays transport-agnostic behind the shared
 * {@link AftTransportPool} interface.
 *
 * Selection is by the USER-tier `subc.connection_file` config key (a project
 * config can never set it — enforced in each plugin's config loader). Present +
 * the file exists ⇒ subc; absent/empty ⇒ standalone (the default). Present but
 * the file is MISSING ⇒ FAIL LOUD (throw) — never a silent downgrade to
 * standalone, which would split-brain a user who meant to run under the daemon.
 */

import { homedir } from "node:os";
import { isAbsolute, join } from "node:path";

import type { ConsumerIdentity } from "@cortexkit/subc-client";

import { BridgePool, type PoolOptions } from "./pool.js";
import { RevivableTransportPool } from "./revivable-transport.js";
import { SubcTransportPool } from "./subc-transport.js";
import type { AftTransportPool } from "./transport.js";

export interface AftTransportFactoryOptions {
  /** Harness identity ("opencode" | "pi"). Carried in every subc BindIdentity. */
  harness: string;
  /** Standalone path: resolved `aft` binary. */
  binaryPath: string;
  /** Standalone path: pool/bridge options (callbacks, timeouts, project loader). */
  poolOptions: PoolOptions;
  /** Standalone path: global configure overrides baked into every bridge. */
  configOverrides: Record<string, unknown>;
  /**
   * USER-tier `subc.connection_file` (already stripped of any project override).
   * Present + existing ⇒ subc transport; absent/empty ⇒ standalone. Tilde and
   * relative paths are resolved against the user's home directory.
   */
  subcConnectionFile?: string;
  /** Test/in-process override for route principal identity. Production leaves this undefined. */
  subcConsumerIdentity?: ConsumerIdentity | null;
  /**
   * Subc path: idle bg-completion wake handler (a `{op:"bg_events"}` nudge). The
   * handler MUST force a drain (the nudge is payload-less). Ignored standalone.
   */
  onBgEventsNudge?: (projectRoot: string, session: string) => void;
}

function resolveConnectionFilePath(raw: string): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith("~")) {
    return join(homedir(), trimmed.slice(1).replace(/^[/\\]/, ""));
  }
  if (isAbsolute(trimmed)) return trimmed;
  // A bare/relative path is resolved against home, not the project cwd — this is
  // a per-machine daemon endpoint, never a project-relative artifact.
  return join(homedir(), trimmed);
}

/**
 * Construct the transport pool for this plugin process. Async because the subc
 * presence check stats the connection file. Returns a pool satisfying
 * {@link AftTransportPool} either way; the caller's downstream code does not
 * branch on which. The returned ownership layer replaces a terminal subc or
 * standalone instance when a later tool call arrives after host teardown.
 */
export async function createAftTransportPool(
  opts: AftTransportFactoryOptions,
): Promise<AftTransportPool> {
  let binaryPath = opts.binaryPath;
  const createPool = () => createConcreteAftTransportPool({ ...opts, binaryPath });
  return new RevivableTransportPool(await createPool(), createPool, (path) => {
    binaryPath = path;
  });
}

async function createConcreteAftTransportPool(
  opts: AftTransportFactoryOptions,
): Promise<AftTransportPool> {
  const raw = opts.subcConnectionFile?.trim();
  if (raw && raw.length > 0) {
    const connectionFile = resolveConnectionFilePath(raw);
    const available = await SubcTransportPool.connectionAvailable(connectionFile);
    if (!available) {
      // FAIL LOUD: the user explicitly selected subc but the daemon's connection
      // file is absent. Downgrading to standalone here would split-brain a user
      // who expects the daemon to own indexes/caches — surface the error so they
      // start the daemon or clear the config.
      throw new Error(
        `subc.connection_file is set to "${raw}" (resolved: ${connectionFile}) but no subc ` +
          `connection file exists there. Start the Subconscious daemon, correct the path, ` +
          `or remove subc.connection_file from your user config to use the standalone bridge.`,
      );
    }
    return new SubcTransportPool({
      connectionFile,
      harness: opts.harness,
      consumerIdentity: opts.subcConsumerIdentity,
      onBgEventsNudge: opts.onBgEventsNudge,
    });
  }
  return new BridgePool(opts.binaryPath, opts.poolOptions, opts.configOverrides);
}
