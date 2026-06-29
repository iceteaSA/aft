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
  isConsumerReconnectTransient,
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

/** A held-open event subscription — the slice of subc-client's Subscription we use. */
export interface SubcSubscriptionLike {
  /** Cancel the subscription (sends Cancel; idempotent); the provider unwinds with StreamEnd. */
  unsubscribe(): void;
  /** Resolves on StreamEnd (intentional close); REJECTS on Error / route GOODBYE / socket drop. */
  readonly closed: Promise<void>;
}

/**
 * The minimal slice of {@link SubcClient} this transport depends on. Declared
 * structurally so a test can inject a fake client through the pool's `connect`
 * seam without standing up a daemon; the real `SubcClient` satisfies it.
 */
export interface SubcClientLike {
  routeOpen(target: RouteTarget, identity: BindIdentity): Promise<number>;
  request(routeChannel: number, body: unknown, opts?: RequestOptions): Promise<unknown>;
  subscribe(
    routeChannel: number,
    body: unknown,
    onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike;
  closeRouteChannel(channel: number, opts?: { drain?: boolean }): Promise<void>;
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
  /**
   * Called when an idle bg-completion WAKE arrives for `(projectRoot, session)`
   * (a `{op:"bg_events"}` StreamData nudge), AND immediately after each
   * (re)subscribe (the durable-outbox replay trigger). The nudge carries NO
   * payload — the handler MUST force a DRAIN (bash_drain_completions) to fetch
   * the actual completions. When set, the transport opens a dedicated bg_events
   * subscription per session and drives its reconnect independently of tool
   * calls (so an idle agent whose socket drops is still resubscribed + drained).
   * Absent ⇒ no bg subscriptions are opened.
   */
  onBgEventsNudge?: (projectRoot: string, session: string) => void;
  /** Test seam: backoff sleeper for the bg resubscribe loop (default real timer). */
  bgBackoffSleep?: (ms: number) => Promise<void>;
}

function identityKey(identity: BindIdentity): string {
  return `${identity.project_root}\u0000${identity.harness}\u0000${identity.session}`;
}

/**
 * One session's held-open bg_events subscription with its OWN reconnect driver.
 *
 * The idle-stranding fix (Oracle bg_fc2d4119 #3): the resubscribe loop is
 * INDEPENDENT of tool calls. When the subscription's `closed` promise rejects (a
 * socket drop / route GOODBYE / Error), the loop itself reconnects (via the pool's
 * shared single-flight client) and resubscribes — it never waits for a future tool
 * call. So an idle agent (no tool traffic) whose connection drops is still woken
 * for a completion that landed while disconnected (the durable Rust registry holds
 * it until acked; resubscribe + the immediate forced-drain replay it).
 *
 * The loop is a single sequential async task (only one attempt in flight at a
 * time), so no numeric generation guard is needed — `stopped` plus one-instance-
 * per-identity (the pool's bgSubs map) prevents duplicate or stale subscribes.
 */
class BgSubscription {
  private stopped = false;
  private current: SubcSubscriptionLike | null = null;
  private currentClient: SubcClientLike | null = null;
  private currentChannel: number | null = null;
  private readonly loop: Promise<void>;

  constructor(
    private readonly identity: BindIdentity,
    private readonly acquireClient: () => Promise<SubcClientLike>,
    private readonly onNudge: () => void,
    private readonly sleep: (ms: number) => Promise<void>,
  ) {
    this.loop = this.run();
  }

  async stop(): Promise<void> {
    this.stopped = true;
    const sub = this.current;
    const client = this.currentClient;
    const channel = this.currentChannel;
    this.current = null;
    this.currentClient = null;
    this.currentChannel = null;
    if (sub) {
      try {
        sub.unsubscribe();
      } catch {
        // best-effort; the socket may already be gone
      }
    }
    if (client && channel !== null) {
      try {
        await client.closeRouteChannel(channel);
      } catch {
        // best-effort; a dropped connection releases the route daemon-side
      }
    }
    await this.loop.catch(() => undefined);
  }

  private async run(): Promise<void> {
    let attempt = 0;
    while (!this.stopped) {
      let client: SubcClientLike;
      try {
        client = await this.acquireClient();
      } catch {
        await this.backoff(attempt++);
        continue;
      }
      if (this.stopped) return;

      let channel: number;
      try {
        // A SECOND, dedicated routeOpen (NOT the tool route cache): the daemon
        // mints a fresh channel per route.open, so the bg_events subscribe rides
        // its own channel, isolated from the tool route's credit window.
        channel = await client.routeOpen(
          { kind: "tool_provider", module_id: AFT_MODULE_ID },
          this.identity,
        );
      } catch {
        await this.backoff(attempt++);
        continue;
      }
      if (this.stopped) {
        void client.closeRouteChannel(channel).catch(() => undefined);
        return;
      }

      const sub = client.subscribe(channel, { op: "bg_events" }, () => {
        if (!this.stopped) this.onNudge();
      });
      this.current = sub;
      this.currentClient = client;
      this.currentChannel = channel;
      attempt = 0; // a successful (re)subscribe resets backoff

      // Immediate forced-drain replay: a completion that landed while we were
      // disconnected is recovered now (resubscribe == the outbox replay trigger).
      if (!this.stopped) this.onNudge();

      try {
        await sub.closed;
        // StreamEnd = an intentional close (our unsubscribe or module teardown).
        // Do NOT resubscribe.
        return;
      } catch {
        // Dropped (socket death / route GOODBYE / Error). Resubscribe — this is
        // the independent reconnect driver that fixes idle-stranding.
        this.current = null;
        this.currentClient = null;
        this.currentChannel = null;
        if (this.stopped) return;
        await this.backoff(attempt++);
      }
    }
  }

  private async backoff(attempt: number): Promise<void> {
    const ms = Math.min(100 * 2 ** Math.min(attempt, 6), 2000);
    await this.sleep(ms);
  }
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

  private readonly onBgEventsNudge?: (projectRoot: string, session: string) => void;
  private readonly bgBackoffSleep: (ms: number) => Promise<void>;

  private client: SubcClientLike | null = null;
  /** Single-flight guard so concurrent first calls share one connect. */
  private connecting: Promise<SubcClientLike> | null = null;
  /** Cached route channels by identity key. */
  private readonly routes = new Map<string, number>();
  /** One bg_events subscription per identity key (idempotent: never duplicated). */
  private readonly bgSubs = new Map<string, BgSubscription>();
  /** Per-root transport facades returned by getBridge/getActiveBridgeForRoot. */
  private readonly transports = new Map<string, SubcTransport>();
  private shuttingDown = false;

  constructor(options: SubcTransportPoolOptions) {
    this.connectionFile = options.connectionFile;
    this.harness = options.harness;
    this.handshakeTimeoutMs = options.handshakeTimeoutMs;
    this.connectFn = options.connect ?? ((opts) => SubcClient.connect(opts));
    this.onBgEventsNudge = options.onBgEventsNudge;
    this.bgBackoffSleep =
      options.bgBackoffSleep ?? ((ms) => new Promise((resolve) => setTimeout(resolve, ms)));
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
      const reply = await client.request(channel, body, { timeoutMs, onProgress });
      // Lazy-open the dedicated bg_events subscription on first successful route
      // use for this identity (Oracle Q4: a bg bash task requires a prior tool
      // call, so by the time any completion can land the session is subscribed).
      // Idempotent — only opens once per identity.
      this.ensureBgSubscription(identity);
      return reply;
    } catch (err) {
      // The raw `request()` path does NOT classify failures into SubcCallError
      // (that is only the managed `call()` path); it rejects with a base SubcError
      // (timeout / route GOODBYE / daemon Error frame) or a socket error
      // (closed / reset / refused / pre-send write failure). So distinguishing a
      // dead CONNECTION from a dead ROUTE must use the library's own classifier
      // `isConsumerReconnectTransient`, NOT `instanceof SubcCallError`.
      //
      // Any request failure makes the cached route suspect → drop it so the next
      // call re-opens. Drop the shared CLIENT only when the failure signals a dead
      // connection (transient: socket closed/reset/refused, or a not_sent pre-send
      // write failure). A plain timeout or route GOODBYE is a NON-transient
      // SubcError → the connection is presumed alive, so keep the client (this is
      // the Q1 "keep on outcome_unknown" decision: a lost response does not prove
      // the client is dead; a genuinely dead client surfaces on the NEXT call as a
      // transient socket error and is dropped then). NEVER auto-retry here — the
      // failed call is surfaced to the agent, mutation-safe by construction.
      this.routes.delete(identityKey(identity));
      if (isConsumerReconnectTransient(err)) {
        this.dropClient(client);
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

  /**
   * Open the dedicated bg_events subscription for `identity` once. Idempotent —
   * a second call for the same identity is a no-op (one sub per session, the
   * duplicate-sub guard from Oracle #2). No-op when no nudge handler is wired
   * (the transport isn't driving bg completions) or during shutdown.
   */
  private ensureBgSubscription(identity: BindIdentity): void {
    if (this.shuttingDown || !this.onBgEventsNudge) return;
    const key = identityKey(identity);
    if (this.bgSubs.has(key)) return;
    const onNudge = (): void => this.onBgEventsNudge?.(identity.project_root, identity.session);
    const sub = new BgSubscription(
      identity,
      () => this.ensureClient(),
      onNudge,
      this.bgBackoffSleep,
    );
    this.bgSubs.set(key, sub);
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
    // Stop every bg subscription FIRST (unsubscribe + closeRouteChannel) while the
    // client is still alive, so each releases its held daemon request credit.
    const subs = Array.from(this.bgSubs.values());
    this.bgSubs.clear();
    await Promise.allSettled(subs.map((sub) => sub.stop()));
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

  /**
   * Tear down a single session's bg subscription (and tool route) — the
   * per-session close hook (Oracle #5). Idempotent. Wired to OpenCode session-end
   * / Pi equivalent in S4; until then, shutdown() covers teardown.
   */
  async closeSession(projectRoot: string, session: string): Promise<void> {
    const identity: BindIdentity = {
      project_root: canonicalizeProjectRoot(projectRoot),
      harness: this.harness,
      session: session && session.length > 0 ? session : DEFAULT_SESSION_ID,
    };
    const key = identityKey(identity);
    const sub = this.bgSubs.get(key);
    if (sub) {
      this.bgSubs.delete(key);
      await sub.stop();
    }
    const channel = this.routes.get(key);
    if (channel !== undefined && this.client) {
      this.routes.delete(key);
      try {
        await this.client.closeRouteChannel(channel);
      } catch {
        // best-effort; a dropped connection releases the route daemon-side
      }
    }
  }
}
