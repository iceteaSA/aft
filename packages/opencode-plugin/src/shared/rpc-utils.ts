import { join } from "node:path";
import { projectRootKeyHash } from "@cortexkit/aft-bridge";

/**
 * Compute a stable hash for a project directory, used to scope RPC port files
 * per-project so multiple OpenCode Desktop instances don't overwrite each
 * other.
 *
 * Delegates to the shared `projectRootKeyHash`, which CANONICALIZES (realpath +
 * Windows normalization) before hashing — the same identity the bridge pool
 * routes by. Previously this hashed the RAW directory string, so a symlinked or
 * raw-spelled launch dir scoped its port file to a different directory than the
 * bridge routed to, leaving the sidebar unable to discover the live server
 * (stale-port / wrong-project class).
 */
export function projectHash(directory: string): string {
  return projectRootKeyHash(directory);
}

/**
 * Legacy per-project RPC port file path (single file).
 *
 * Kept exported for backward-compatibility readers — when an older plugin
 * instance is running alongside a newer one, the older one still writes
 * to this path. New code prefers `rpcPortFileDir` (one file per instance)
 * so that two plugin instances under `opencode --port 0` don't overwrite
 * each other's port info. The client falls back to the legacy file if
 * the new directory has no entries.
 */
export function rpcPortFilePath(storageDir: string, directory: string): string {
  const hash = projectHash(directory);
  return join(storageDir, "rpc", hash, "port");
}

/**
 * Per-project RPC port directory. Each plugin instance writes a file
 * `<instance-id>.json` into this directory so the client can discover
 * ALL active plugin instances (e.g. the two created by OpenCode TUI when
 * launched with `--port 0`). The client tries each port and uses the
 * first one whose bridge is warm.
 *
 * Replaces the single `port` file used pre-v0.28.2 (which suffered from
 * last-write-wins racing under `--port 0`).
 */
export function rpcPortFileDir(storageDir: string, directory: string): string {
  const hash = projectHash(directory);
  return join(storageDir, "rpc", hash, "ports");
}

/**
 * Contents of a per-instance RPC port file. `pid` + `started_at` let the client
 * skip files whose owning process is dead (no health-check round-trip) and pick
 * the freshest live server, instead of wading through every crash/restart
 * leftover. Older files carry only `{ port, token }` (no pid); those are treated
 * as "can't prove dead" and fall back to the health-check path.
 */
export interface RpcPortRecord {
  port: number;
  token: string | null;
  /** PID of the plugin process that owns this server, if recorded. */
  pid?: number;
  /** `Date.now()` when this server started, for newest-first ordering. */
  started_at?: number;
}

/** True if `pid` names a live process. `process.kill(pid, 0)` sends no signal. */
export function isPidAlive(pid: number | undefined): boolean {
  if (typeof pid !== "number" || !Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    // EPERM = process exists but we can't signal it (still alive).
    return (err as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * Parse a port file's contents into a record. Accepts the current JSON shape
 * (`{ port, token, pid?, started_at? }`) and the legacy bare-integer format
 * (unauthenticated, pre-v0.28.2). Returns `null` for unusable contents.
 */
export function parseRpcPortRecord(content: string): RpcPortRecord | null {
  const trimmed = content.trim();
  if (trimmed.length === 0) return null;
  if (trimmed.startsWith("{")) {
    try {
      const parsed = JSON.parse(trimmed) as {
        port?: unknown;
        token?: unknown;
        pid?: unknown;
        started_at?: unknown;
      };
      const port = typeof parsed.port === "number" ? parsed.port : Number.NaN;
      if (!Number.isInteger(port) || port <= 0 || port > 65535) return null;
      return {
        port,
        token: typeof parsed.token === "string" ? parsed.token : null,
        pid:
          typeof parsed.pid === "number" && Number.isInteger(parsed.pid) ? parsed.pid : undefined,
        started_at: typeof parsed.started_at === "number" ? parsed.started_at : undefined,
      };
    } catch {
      return null;
    }
  }
  const port = Number.parseInt(trimmed, 10);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) return null;
  return { port, token: null };
}
