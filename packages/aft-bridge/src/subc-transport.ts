/**
 * Subconscious (subc) transport — the daemon-backed alternative to the standalone
 * NDJSON {@link BinaryBridge}. Implements the SAME {@link AftProjectTransport} /
 * {@link AftTransportPool} interfaces the plugins consume, so the entire tool /
 * hoisting / permission / UI surface stays transport-agnostic: only the ONE
 * construction site (BridgePool vs SubcTransportPool) differs.
 *
 * Standalone model: one `aft` child process per project root, session passed
 * per call. Subc model: ONE {@link SubcClient} per process (one authenticated
 * daemon connection), and a route opened+cached per `(project_root, harness,
 * session)` triple — exactly subc's {@link BindIdentity}. So the "pool" here is a
 * route cache over a single client, not N child processes.
 *
 * This module is S2 of B-FINAL: the tool-call route only. The bg_events idle-wake
 * subscription (S3) and the config gate that selects this transport (S4) build on
 * top of it. subc-client is a build-time path dependency bundled into the
 * published plugin dist; it is never a published runtime dependency.
 */

import {
  type BindIdentity,
  connectionFileExists,
  type RequestOptions,
  type RouteTarget,
  SubcCallError,
  SubcClient,
} from "@cortexkit/subc-client";
import type { StatusSnapshot } from "./bridge.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import { parseStatusBarCounts, type StatusBarCounts } from "./status-bar.js";
import type {
  AftProjectTransport,
  AftTransportOptions,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

/**
 * The minimal slice of {@link SubcClient} this transport depends on. Declared
 * structurally so a test can inject a fake client through the pool's `connect`
 * seam without standing up a daemon; the real `SubcClient` satisfies it.
 */
export interface SubcClientLike {
  routeOpen(target: RouteTarget, identity: BindIdentity): Promise<number>;
  request(routeChannel: number, body: unknown, opts?: RequestOptions): Promise<unknown>;
  close(): void;
}

/** The subc module id AFT registers under (matches the daemon manifest). */
const AFT_MODULE_ID = "aft";

/**
 * Session fallback when a tool runtime carries no session id, mirroring the Rust
 * `DEFAULT_SESSION_ID` (`protocol.rs`). Keeps undo/checkpoint/bash namespacing
 * identical to the standalone path for session-less calls.
 */
const DEFAULT_SESSION_ID = "__default__";

/**
 * Commands the plugin issues via `send()` that have NO meaning over subc and must
 * never hit the wire. `configure` is the prime case: under subc the RouteBind IS
 * the configure (AFT reads local `.cortexkit` config and ignores wire tiers — see
 * the unified-config model), so a `send("configure", …)` is satisfied locally
 * with a synthetic success rather than a route call.
 */
const LOCALLY_SATISFIED_COMMANDS = new Set(["configure"]);

export interface SubcTransportPoolOptions {
  /** Absolute path to the subc connection file (user-tier `subc.connection_file`). */
  connectionFile: string;
  /** Harness identity carried in every BindIdentity ("opencode" | "pi" | …). */
  harness: string;
  /** Handshake timeout forwarded to SubcClient.connect. */
  handshakeTimeoutMs?: number;
  /**
   * Connection factory seam. Defaults to the real `SubcClient.connect`. Tests
   * inject a fake to exercise route caching / Rd reconnect without a daemon.
   */
  connect?: (opts: {
    connectionFile: string;
    handshakeTimeoutMs?: number;
  }) => Promise<SubcClientLike>;
}

function identityKey(identity: BindIdentity): string {
  return `${identity.project_root}\u0000${identity.harness}\u0000${identity.session}`;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Re-lift the route reply into the flat {@link ToolCallResult} shape the standalone
 * `BinaryBridge.toolCall` returns. The Rust module wraps the full flat response
 * (`{id, success, …data, text}`) under `structuredContent` (S1 envelope), alongside
 * the MCP `{content, isError}` a generic host reads. The first-party plugin reads
 * `structuredContent`, so re-lifting it makes everything downstream (status_bar,
 * bg_completions, preview_diff, code, …) byte-identical to NDJSON. If the reply is
 * not the expected envelope (defensive — should not happen for a tool response),
 * fall back to treating the reply itself as the flat shape.
 */
function reliftReply(reply: unknown): Record<string, unknown> {
  if (isRecord(reply) && isRecord(reply.structuredContent)) {
    return reply.structuredContent;
  }
  if (isRecord(reply)) {
    return reply;
  }
  return { success: false, text: "" };
}

/**
 * One project root's view onto the shared subc client. Holds per-root status
 * caches (mirroring BinaryBridge) and routes every call through the pool's single
 * client, opening+caching a route per `(root, harness, session)`.
 */
class SubcTransport implements AftProjectTransport {
  private lastStatusBar: StatusBarCounts | undefined;
  private cachedStatus: StatusSnapshot | null = null;

  constructor(
    private readonly pool: SubcTransportPool,
    private readonly projectRoot: string,
  ) {}

  getCwd(): string {
    return this.projectRoot;
  }

  getStatusBar(): StatusBarCounts | undefined {
    return this.lastStatusBar;
  }

  getCachedStatus(): StatusSnapshot | null {
    return this.cachedStatus;
  }

  cacheStatusSnapshot(snapshot: StatusSnapshot): void {
    this.cachedStatus = snapshot;
  }

  private captureStatusBar(response: Record<string, unknown>): void {
    const parsed = parseStatusBarCounts(response.status_bar);
    if (parsed) this.lastStatusBar = parsed;
  }

  private identityFor(session: string | undefined): BindIdentity {
    return {
      project_root: this.projectRoot,
      harness: this.pool.harness,
      session: session && session.length > 0 ? session : DEFAULT_SESSION_ID,
    };
  }

  async toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    const { preview, timeoutMs, onProgress } = this.splitOptions(options);
    const body: Record<string, unknown> = { name, arguments: rawArgs };
    if (preview === true) body.preview = true;
    const reply = await this.pool.routeRequest(
      this.identityFor(sessionId),
      body,
      timeoutMs,
      onProgress,
    );
    const result = reliftReply(reply) as ToolCallResult;
    this.captureStatusBar(result);
    return result;
  }

  /**
   * Lifecycle / native-command path. Over subc there is no separate "native
   * command" channel — every command rides the tool_provider route as a
   * `{name, arguments}` Request and the module's gate decides validity (the 21
   * core tools plus the `bash_drain_completions` / `bash_ack_completions` plumbing
   * allowlist). The bind session is taken from `params.session_id` so a
   * session-scoped command (drain/ack) reaches the matching route — the module
   * re-injects the BIND session over any body session, so the route identity is
   * what scopes it. `configure` is satisfied locally (binding is the configure).
   */
  async send(
    command: string,
    params: Record<string, unknown> = {},
    options?: AftTransportOptions,
  ): Promise<Record<string, unknown>> {
    if (LOCALLY_SATISFIED_COMMANDS.has(command)) {
      return { success: true, command, subc_local: true };
    }
    const { timeoutMs, onProgress } = this.splitOptions(options);
    const session = typeof params.session_id === "string" ? params.session_id : undefined;
    const reply = await this.pool.routeRequest(
      this.identityFor(session),
      { name: command, arguments: params },
      timeoutMs,
      onProgress,
    );
    const response = reliftReply(reply);
    this.captureStatusBar(response);
    return response;
  }

  private splitOptions(options?: ToolCallOptions): {
    preview?: boolean;
    timeoutMs?: number;
    onProgress?: RequestOptions["onProgress"];
  } {
    if (!options) return {};
    const preview = (options as ToolCallOptions).preview;
    const timeoutMs = options.timeoutMs;
    const onProgress = (options as { onProgress?: RequestOptions["onProgress"] }).onProgress;
    return { preview, timeoutMs, onProgress };
  }
}

/**
 * Route cache over one authenticated subc client. Implements {@link AftTransportPool}
 * so it drops into the plugin in place of {@link BridgePool} behind the shared
 * interface. One client per process; routes keyed by `(root, harness, session)`.
 */
export class SubcTransportPool implements AftTransportPool {
  readonly harness: string;
  private readonly connectionFile: string;
  private readonly handshakeTimeoutMs?: number;
  private readonly connectFn: (opts: {
    connectionFile: string;
    handshakeTimeoutMs?: number;
  }) => Promise<SubcClientLike>;

  private client: SubcClientLike | null = null;
  /** Single-flight guard so concurrent first calls share one connect. */
  private connecting: Promise<SubcClientLike> | null = null;
  /** Cached route channels by identity key. */
  private readonly routes = new Map<string, number>();
  /** Per-root transport facades returned by getBridge/getActiveBridgeForRoot. */
  private readonly transports = new Map<string, SubcTransport>();
  private shuttingDown = false;

  constructor(options: SubcTransportPoolOptions) {
    this.connectionFile = options.connectionFile;
    this.harness = options.harness;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs;
    this.connectFn = options.connect ?? ((opts) => SubcClient.connect(opts));
  }

  /**
   * Fail-loud presence check (memory: present-but-unconnectable must never silently
   * downgrade to standalone). Returns false only when the file is genuinely absent.
   */
  static async connectionAvailable(connectionFile: string): Promise<boolean> {
    return connectionFileExists(connectionFile);
  }

  getBridge(projectRoot: string): SubcTransport {
    const key = canonicalizeProjectRoot(projectRoot);
    let transport = this.transports.get(key);
    if (!transport) {
      transport = new SubcTransport(this, key);
      this.transports.set(key, transport);
    }
    return transport;
  }

  getActiveBridgeForRoot(projectRoot: string): SubcTransport | null {
    const key = canonicalizeProjectRoot(projectRoot);
    if (!this.client) return null;
    return this.transports.get(key) ?? null;
  }

  async toolCall(
    projectRoot: string,
    runtime: { sessionID?: string },
    name: string,
    rawArgs: ToolCallArguments = {},
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.getBridge(projectRoot).toolCall(runtime.sessionID, name, rawArgs, options);
  }

  /**
   * Open-or-reuse a route for `identity` and send `body` as a data-plane Request.
   * Rd reconnect (mutation-safe by construction — NEVER auto-retries): on a
   * transport-level {@link SubcCallError} the cached channel is discarded and the
   * dead client cleared so the NEXT call re-establishes, but the failed call is
   * surfaced to the agent unchanged (identical to a standalone bridge death). Only
   * `SubcClient.request` transport failures throw here; a tool-level error comes
   * back as a normal reply with `success:false` and is returned, not thrown.
   */
  async routeRequest(
    identity: BindIdentity,
    body: Record<string, unknown>,
    timeoutMs?: number,
    onProgress?: RequestOptions["onProgress"],
  ): Promise<unknown> {
    const client = await this.ensureClient();
    const channel = await this.routeChannel(client, identity);
    try {
      return await client.request(channel, body, { timeoutMs, onProgress });
    } catch (err) {
      if (err instanceof SubcCallError) {
        // The route (and possibly the connection) is dead. Drop the cached
        // channel; if the client itself is the casualty, drop it too so the next
        // call reconnects. Do NOT retry here — surface as a tool error.
        this.routes.delete(identityKey(identity));
        if (err.kind === "not_sent" || err.kind === "outcome_unknown") {
          this.dropClient(client);
        }
      }
      throw err;
    }
  }

  private async ensureClient(): Promise<SubcClientLike> {
    if (this.shuttingDown) {
      throw new SubcCallError("terminal", "subc transport is shutting down");
    }
    if (this.client) return this.client;
    if (this.connecting) return this.connecting;
    this.connecting = this.connectFn({
      connectionFile: this.connectionFile,
      handshakeTimeoutMs: this.handshakeTimeoutMs,
    })
      .then((client) => {
        this.client = client;
        this.connecting = null;
        return client;
      })
      .catch((err) => {
        this.connecting = null;
        throw err;
      });
    return this.connecting;
  }

  private async routeChannel(client: SubcClientLike, identity: BindIdentity): Promise<number> {
    const key = identityKey(identity);
    const cached = this.routes.get(key);
    if (cached !== undefined) return cached;
    const channel = await client.routeOpen(
      { kind: "tool_provider", module_id: AFT_MODULE_ID },
      identity,
    );
    this.routes.set(key, channel);
    return channel;
  }

  /** Drop a dead client so the next call reconnects; clears all cached routes. */
  private dropClient(client: SubcClientLike): void {
    if (this.client === client) {
      this.client = null;
      this.routes.clear();
      try {
        client.close();
      } catch {
        // best-effort; the socket is already gone
      }
    }
  }

  /** No-op over subc: config is read locally by AFT (wire tiers are ignored). */
  setConfigureOverride(_key: string, _value: unknown): void {}

  /** No-op over subc: the daemon supervises the binary, not the plugin. */
  async replaceBinary(path: string): Promise<string> {
    return path;
  }

  async shutdown(): Promise<void> {
    this.shuttingDown = true;
    const client = this.client;
    this.client = null;
    this.routes.clear();
    this.transports.clear();
    if (client) {
      try {
        client.close();
      } catch {
        // best-effort
      }
    }
  }
}
