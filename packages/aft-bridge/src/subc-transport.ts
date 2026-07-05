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
  SubcError,
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
 * A run of consecutive NON-transient transport throws (timeout / route GOODBYE)
 * on the SAME client is presumed a dead half-open connection (local writes
 * succeed, no response ever arrives), so the client is dropped after this many.
 * A single throw does not drop the client (a slow tool can legitimately time out
 * once); the counter resets on any successful request. Tool-level errors never
 * count — they return `success:false`, they do not throw. (Audit B-#4.)
 */
const MAX_CONSECUTIVE_TRANSPORT_FAILURES = 3;

/**
 * A bg subscription that stayed up at least this long before dropping is treated
 * as "stable", so its reconnect backoff resets to zero. A subscription that fails
 * faster than this is escalating-broken and must keep backing off toward the cap
 * (otherwise a permanently-failing route resubscribes in a 100ms hot loop). (B-#2.)
 */
const BG_STABLE_MS = 5_000;

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
  /** The live subscription handle, read by stop() to wake the loop's `await closed`. */
  private current: SubcSubscriptionLike | null = null;
  private readonly loop: Promise<void>;

  constructor(
    private readonly identity: BindIdentity,
    private readonly acquireClient: () => Promise<SubcClientLike>,
    private readonly dropClient: (client: SubcClientLike) => void,
    private readonly onNudge: () => void,
    private readonly sleep: (ms: number) => Promise<void>,
  ) {
    this.loop = this.run();
  }

  async stop(): Promise<void> {
    this.stopped = true;
    // Wake a live `await sub.closed` so the loop unwinds via its StreamEnd path,
    // where the `finally` is the SOLE owner of closeRouteChannel (so the channel
    // is closed exactly once, never double-closed by stop() + the loop). If the
    // loop is between routeOpen and subscribe, its post-subscribe `stopped`
    // re-check (or the pre-subscribe check) closes/returns instead.
    const sub = this.current;
    if (sub) {
      try {
        sub.unsubscribe();
      } catch {
        // best-effort; the socket may already be gone
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
      } catch (err) {
        // routeOpen failed. If it signals a dead CONNECTION, drop the shared
        // client so the next `acquireClient` reconnects fresh — the idle-stranding
        // fix (B-#1): `acquireClient` returns the cached client, so without this an
        // idle bg loop would resubscribe forever onto the same dead socket.
        if (isConsumerReconnectTransient(err)) this.dropClient(client);
        await this.backoff(attempt++);
        continue;
      }
      if (this.stopped) {
        safeCloseRoute(client, channel);
        return;
      }

      // Channel lifetime: the `finally` guarantees closeRouteChannel on EVERY exit
      // path from here (StreamEnd return, drop+resubscribe, stopped) so the
      // dedicated route never leaks (B-#2).
      const subscribedAt = Date.now();
      try {
        const sub = client.subscribe(channel, { op: "bg_events" }, () => {
          if (!this.stopped) this.onNudge();
        });
        this.current = sub;
        // stop() may have fired between the pre-subscribe check and here; self-
        // unsubscribe so the await below resolves immediately (avoids a stop()
        // that hangs awaiting a subscription nobody woke).
        if (this.stopped) sub.unsubscribe();

        // Immediate forced-drain replay: a completion that landed while we were
        // disconnected is recovered now (resubscribe == the outbox replay trigger).
        if (!this.stopped) this.onNudge();

        await sub.closed;
        // StreamEnd = an intentional close (our unsubscribe or module teardown).
        // Do NOT resubscribe.
        return;
      } catch (err) {
        // Dropped (socket death / route GOODBYE / Error). Resubscribe — this is
        // the independent reconnect driver that fixes idle-stranding.
        if (this.stopped) return;
        // A dead-connection drop must replace the client (B-#1); a route-only
        // GOODBYE keeps the client and just re-opens a fresh route.
        if (isConsumerReconnectTransient(err)) this.dropClient(client);
        // Reset backoff ONLY if the subscription was stable before dropping;
        // otherwise a permanently-failing route would resubscribe in a 100ms hot
        // loop and never reach the cap (B-#2).
        if (Date.now() - subscribedAt >= BG_STABLE_MS) attempt = 0;
      } finally {
        this.current = null;
        safeCloseRoute(client, channel);
      }
      await this.backoff(attempt++);
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

function isUnknownChannelError(err: unknown): boolean {
  return err instanceof SubcError && err.code === "unknown_channel";
}

/**
 * Fire-and-forget route close that can never throw — neither synchronously (a
 * client that rejects/throws when closing a route on an already-dead socket) nor
 * via an unhandled rejection. Used on every best-effort teardown path.
 */
function safeCloseRoute(client: SubcClientLike, channel: number): void {
  try {
    void client.closeRouteChannel(channel).catch(() => undefined);
  } catch {
    // synchronous throw (e.g. closing a route on an already-closed client) — ignore
  }
}

/** Per-identity session lifecycle state, independent from transient route churn. */
interface SessionRecord {
  /** Current tool route for this session incarnation; replaced after route failures. */
  routeEntry: RouteEntry | null;
  /** Dedicated bg_events subscription, present only when background events are enabled. */
  bgSub: BgSubscription | null;
  /** Closed marker set synchronously so in-flight requests can see the close. */
  closed: boolean;
  /** Count of in-flight requests on this session's route; used for safe cleanup. */
  inflight: number;
}

/**
 * Per-identity tool-route state. Installed BEFORE the `routeOpen` await so two
 * concurrent first calls for the same identity share ONE open (singleflight)
 * instead of each minting a channel and leaking the loser (audit B-#3). `closed`
 * tombstones the entry when `closeSession`/`shutdown`/`dropClient` races an
 * in-flight open: the resolving open observes it and closes the just-opened
 * channel instead of caching a stale route.
 */
interface RouteEntry {
  /** Client that minted this route; closeSession must close channels on this owner. */
  client: SubcClientLike;
  /** In-flight routeOpen; non-null until it settles. Concurrent callers await it. */
  opening: Promise<number> | null;
  /** Resolved channel once open; null while still opening. */
  channel: number | null;
  /** Tombstone: a teardown raced the open — the resolving open must self-close. */
  closed: boolean;
}

/**
 * A route open that resolved AFTER its session was torn down (closeSession /
 * shutdown / client swap). The caller's request can't proceed, but this is an
 * intentional teardown — NOT a transport fault — so it must not drop the client
 * or count toward the half-open-socket failure budget (B-#3/B-#4).
 */
class RouteTornDownError extends Error {}

/**
 * Re-lift the route reply into the flat {@link ToolCallResult} shape the standalone
 * `BinaryBridge.toolCall` returns. The Rust module wraps the full flat response
 * (`{id, success, …data, text}`) under `structuredContent` (S1 envelope), alongside
 * the MCP `{content, isError}` a generic host reads. The first-party plugin reads
 * `structuredContent`, so re-lifting it makes everything downstream (status_bar,
 * bg_completions, preview_diff, code, …) byte-identical to NDJSON.
 *
 * Every AFT tool reply over subc carries this envelope with a boolean `success`
 * and string `text`. A reply missing the envelope, or whose lifted shape lacks a
 * boolean `success`, is a PROTOCOL VIOLATION — never a tool result — and is thrown
 * rather than coerced. Coercing it (the old `{success:false,text:""}` /
 * raw-record fallback) could let a malformed reply with `success === undefined`
 * read downstream as a successful tool result (audit B-#7). Surfacing it loudly is
 * the honest contract: a broken wire shape is a failure, not a silent empty pass.
 */
function reliftReply(reply: unknown): Record<string, unknown> {
  if (!isRecord(reply) || !isRecord(reply.structuredContent)) {
    throw new Error(
      "subc tool reply is missing the structuredContent envelope (protocol violation)",
    );
  }
  const flat = reply.structuredContent;
  if (typeof flat.success !== "boolean" || typeof flat.text !== "string") {
    throw new Error(
      "subc tool reply structuredContent lacks a boolean `success` / string `text` (protocol violation)",
    );
  }
  return flat;
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
    // Mirror BinaryBridge.send's budget precedence (transportTimeoutMs wins):
    // orchestrated bash passes its wait-aware budget as transportTimeoutMs, and
    // dropping it here would cap long tool executions at the subc client's
    // default unary deadline while the command keeps running module-side.
    const timeoutMs = options.transportTimeoutMs ?? options.timeoutMs;
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
  /** Per-session lifecycle records keyed by a string built from root, harness, and session. */
  private readonly sessions = new Map<string, SessionRecord>();
  /**
   * Consecutive NON-transient transport throws on the current client with no
   * success in between. Resets to 0 on any successful request. Trips a client drop
   * at {@link MAX_CONSECUTIVE_TRANSPORT_FAILURES} to recover a half-open socket
   * whose timeouts never classify transient (B-#4).
   */
  private transportFailures = 0;
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

  private getOrCreateSession(key: string): SessionRecord {
    let record = this.sessions.get(key);
    if (!record || record.closed) {
      record = { routeEntry: null, bgSub: null, closed: false, inflight: 0 };
      this.sessions.set(key, record);
    }
    return record;
  }

  private isCurrentSession(key: string, record: SessionRecord): boolean {
    return this.sessions.get(key) === record && !record.closed;
  }

  private deleteSessionIfEmpty(key: string, record: SessionRecord): void {
    if (
      this.sessions.get(key) === record &&
      !record.closed &&
      record.inflight === 0 &&
      record.routeEntry === null &&
      record.bgSub === null
    ) {
      this.sessions.delete(key);
    }
  }

  /**
   * Open-or-reuse a route for `identity` and send `body` as a data-plane Request.
   * Rd reconnect is mutation-safe by construction: transport failures surface to
   * the caller after clearing stale route/client state so the NEXT call
   * re-establishes. The only in-place retry is `unknown_channel`, where the daemon
   * proves the route was absent and the request was not delivered to the module.
   * Tool-level errors come back as normal replies with `success:false` and are
   * returned, not thrown.
   */
  async routeRequest(
    identity: BindIdentity,
    body: Record<string, unknown>,
    timeoutMs?: number,
    onProgress?: RequestOptions["onProgress"],
  ): Promise<unknown> {
    const key = identityKey(identity);
    const record = this.getOrCreateSession(key);
    record.inflight += 1;

    try {
      const client = await this.ensureClient();
      const openRoute = async (): Promise<{ channel: number; entry: RouteEntry }> => {
        // closeSession/shutdown marks and deletes the record synchronously. A request
        // that was waiting for connect must not open a route for a session that no
        // longer exists.
        if (!this.isCurrentSession(key, record)) {
          throw new RouteTornDownError("subc session closed");
        }

        try {
          const opened = await this.routeChannel(client, identity, record);
          // routeOpen may have awaited. Do not send a request after a close, even
          // if a stale route entry just resolved.
          if (!this.isCurrentSession(key, record)) {
            throw new RouteTornDownError("subc session closed");
          }
          return opened;
        } catch (err) {
          // A teardown that raced the open is not a transport fault — surface it
          // without dropping the client or charging the failure budget.
          if (err instanceof RouteTornDownError) throw err;
          // routeOpen itself failed. Classify the error like other request failures
          // for transient connection death, but only while this request's session
          // and client are still current. A close-induced failure must not drop a
          // healthy client shared by other sessions.
          if (
            isConsumerReconnectTransient(err) &&
            this.isCurrentSession(key, record) &&
            this.client === client
          ) {
            this.dropClient(client);
          }
          throw err;
        }
      };

      const clearRouteEntry = (entry: RouteEntry): void => {
        if (record.routeEntry === entry) {
          entry.closed = true;
          record.routeEntry = null;
        }
      };

      const handleRequestFailure = (err: unknown, entry: RouteEntry): void => {
        // The raw `request()` path does not turn failures into SubcCallError; it
        // rejects with a base SubcError for route-level failures or a raw socket
        // error for connection failures. Use `isConsumerReconnectTransient`, not
        // `instanceof SubcCallError`, to distinguish a dead connection from a dead
        // route.
        //
        // A failed request makes its own route suspect, so drop it and let the next
        // call re-open. Check only the current entry; a stale failure must not
        // delete a successor route in the same still-open session.
        clearRouteEntry(entry);
        // Only a failure from the current session on the current client can charge
        // or drop that client. A failure caused by closeSession sees
        // !isCurrentSession because close marked/deleted the record before awaiting
        // transport cleanup.
        if (this.isCurrentSession(key, record) && this.client === client) {
          if (isConsumerReconnectTransient(err)) {
            this.transportFailures = 0;
            this.dropClient(client);
          } else if (
            !isUnknownChannelError(err) &&
            ++this.transportFailures >= MAX_CONSECUTIVE_TRANSPORT_FAILURES
          ) {
            // A run of non-transient throws (timeouts / route GOODBYEs) with no
            // success between them is a half-open socket: local writes succeed but
            // no response ever returns, so isConsumerReconnectTransient never fires
            // and the client would otherwise be kept forever. Force reconnect.
            this.transportFailures = 0;
            this.dropClient(client);
          }
        }
      };

      const requestOnRoute = async (channel: number): Promise<unknown> => {
        // Forward the caller's progress callback to the subc client. Current
        // foreground calls return one final reply, so production does not rely on
        // live progress events here.
        const reply = await client.request(channel, body, { timeoutMs, onProgress });
        // The half-open failure budget belongs to the current live session on the
        // current client. A late success after close or client replacement must
        // not mutate the current client's counter.
        if (this.isCurrentSession(key, record) && this.client === client) {
          this.transportFailures = 0;
        }
        // bg_events is a session resource, not a route-entry resource. A
        // successful request on an older route should still ensure the subscription
        // while the session remains open, and a late success after close must not
        // resurrect one.
        this.ensureBgSubscription(identity, record);
        return reply;
      };

      let { channel, entry } = await openRoute();
      try {
        return await requestOnRoute(channel);
      } catch (err) {
        if (
          isUnknownChannelError(err) &&
          this.isCurrentSession(key, record) &&
          this.client === client
        ) {
          // The daemon says this channel does not exist, so the request was not
          // delivered to the module. Reopen the route and resend once; all other
          // route/request failures keep the no-auto-retry mutation-safety rule.
          clearRouteEntry(entry);
          ({ channel, entry } = await openRoute());
          try {
            return await requestOnRoute(channel);
          } catch (retryErr) {
            handleRequestFailure(retryErr, entry);
            throw retryErr;
          }
        }
        handleRequestFailure(err, entry);
        throw err;
      }
    } finally {
      record.inflight -= 1;
      this.deleteSessionIfEmpty(key, record);
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
        this.connecting = null;
        // shutdown() may have fired while this connect was in flight. Don't
        // install a live client after teardown — close it and fail the call
        // (B-#5: otherwise a socket leaks open past shutdown()).
        if (this.shuttingDown) {
          try {
            client.close();
          } catch {
            // best-effort
          }
          throw new SubcCallError("terminal", "subc transport is shutting down");
        }
        this.client = client;
        // Fresh client generation starts with a clean failure budget (R2-T2).
        this.transportFailures = 0;
        return client;
      })
      .catch((err) => {
        this.connecting = null;
        throw err;
      });
    return this.connecting;
  }

  /**
   * Resolve the route channel for `identity`, returning BOTH the channel and the
   * `RouteEntry` token that owns it. The route-entry token is the route-cache
   * ownership guard: stale request failures can clear only the route they used,
   * never a successor route in the same still-open session.
   */
  private async routeChannel(
    client: SubcClientLike,
    identity: BindIdentity,
    record: SessionRecord,
  ): Promise<{ channel: number; entry: RouteEntry }> {
    const key = identityKey(identity);
    const existing = record.routeEntry;
    if (existing?.channel != null) return { channel: existing.channel, entry: existing };
    if (existing?.opening) return { channel: await existing.opening, entry: existing };

    // Singleflight: install the entry BEFORE awaiting so a concurrent caller for
    // the same identity awaits this same open instead of minting a second channel.
    const entry: RouteEntry = { client, opening: null, channel: null, closed: false };
    const opening = client
      .routeOpen({ kind: "tool_provider", module_id: AFT_MODULE_ID }, identity)
      .then((channel) => {
        // A close/shutdown may have raced this open, or this entry may have been
        // replaced by a transient drop/reopen. In both cases, release the freshly
        // minted channel and clear only this entry if it still owns the cache slot.
        if (
          !this.isCurrentSession(key, record) ||
          record.routeEntry !== entry ||
          entry.closed ||
          this.client !== client
        ) {
          safeCloseRoute(client, channel);
          if (record.routeEntry === entry) record.routeEntry = null;
          throw new RouteTornDownError("subc route opened after teardown");
        }
        entry.channel = channel;
        entry.opening = null;
        return channel;
      })
      .catch((err) => {
        const current = this.isCurrentSession(key, record);
        // Failed open: drop this entry so the next call retries cleanly, but never
        // delete a successor entry that replaced it while this open was in flight.
        if (record.routeEntry === entry) {
          entry.closed = true;
          record.routeEntry = null;
        }
        if (!current && !(err instanceof RouteTornDownError)) {
          throw new RouteTornDownError("subc route opened after session closed");
        }
        throw err;
      });
    entry.opening = opening;
    record.routeEntry = entry;
    return { channel: await opening, entry };
  }

  /**
   * Open the dedicated bg_events subscription for `identity` once. Idempotent —
   * a second call for the same identity is a no-op (one sub per session, the
   * duplicate-sub guard from Oracle #2). No-op when no nudge handler is wired,
   * during shutdown, or after the session has been closed.
   */
  private ensureBgSubscription(identity: BindIdentity, record: SessionRecord): void {
    if (this.shuttingDown || !this.onBgEventsNudge) return;
    const key = identityKey(identity);
    if (!this.isCurrentSession(key, record)) return;
    if (record.bgSub) return;
    const onNudge = (): void => this.onBgEventsNudge?.(identity.project_root, identity.session);
    const sub = new BgSubscription(
      identity,
      () => this.ensureClient(),
      (client) => this.dropClient(client),
      onNudge,
      this.bgBackoffSleep,
    );
    record.bgSub = sub;
  }

  /** Drop a dead client so the next call reconnects; preserves session records. */
  private dropClient(client: SubcClientLike): void {
    if (this.client === client) {
      this.client = null;
      for (const [key, record] of this.sessions) {
        const entry = record.routeEntry;
        if (entry?.client === client) {
          entry.closed = true;
          record.routeEntry = null;
          this.deleteSessionIfEmpty(key, record);
        }
      }
      // The half-open failure counter is per-client-generation: a dropped client
      // resets it so the NEXT client starts fresh (R2-T2). Without this, failures
      // accrued on a client dropped via another path (e.g. a bg-subscription
      // transient drop) would carry over and trip the backstop on a healthy new
      // client after a single failure.
      this.transportFailures = 0;
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
    const subs: BgSubscription[] = [];
    const entries: RouteEntry[] = [];
    for (const record of this.sessions.values()) {
      record.closed = true;
      const sub = record.bgSub;
      record.bgSub = null;
      if (sub) subs.push(sub);
      const entry = record.routeEntry;
      record.routeEntry = null;
      if (entry) {
        entry.closed = true;
        entries.push(entry);
      }
    }
    this.sessions.clear();
    const client = this.client;
    this.client = null;
    this.transports.clear();

    await Promise.allSettled(subs.map((sub) => sub.stop()));
    await Promise.allSettled(
      entries.map(async (entry) => {
        if (entry.channel == null) return;
        try {
          await entry.client.closeRouteChannel(entry.channel);
        } catch {
          // best-effort; a dropped connection releases the route on the other side
        }
      }),
    );
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
    const record = this.sessions.get(key);
    if (!record) return;

    // All lifecycle mutation happens before the first await. In-flight requests
    // resumed after this point observe !isCurrentSession and skip route opens,
    // bg-sub resurrection, and client failure-budget mutation.
    record.closed = true;
    this.sessions.delete(key);
    const sub = record.bgSub;
    record.bgSub = null;
    const entry = record.routeEntry;
    record.routeEntry = null;
    if (entry) entry.closed = true;

    if (sub) await sub.stop();
    if (entry?.channel != null) {
      try {
        await entry.client.closeRouteChannel(entry.channel);
      } catch {
        // best-effort; a dropped connection releases the route on the other side
      }
    }
  }
}
