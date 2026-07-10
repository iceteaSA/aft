import { warn } from "./active-logger.js";
import { canonicalizeProjectRoot } from "./project-identity.js";
import type {
  AftProjectTransport,
  AftTransportPool,
  ToolCallArguments,
  ToolCallOptions,
  ToolCallResult,
} from "./transport.js";

interface StatusSubscribableBridge {
  subscribeStatus(listener: (snapshot: Record<string, unknown>) => void): () => void;
}

type StatusListener = (snapshot: Record<string, unknown>) => void;

type PoolFactory = () => Promise<AftTransportPool>;

/**
 * Owns one terminal transport instance and replaces it when new demand arrives
 * after the host has shut it down. The replaced instance is never reused: its
 * routes, sessions, and sockets remain owned by the dead instance.
 */
export class RevivableTransportPool implements AftTransportPool {
  private activePool: AftTransportPool;
  private revival: Promise<AftTransportPool> | null = null;
  private readonly transports = new Map<string, RevivableProjectTransport>();
  private readonly configureOverrides = new Map<string, unknown>();

  constructor(
    initialPool: AftTransportPool,
    private readonly createPool: PoolFactory,
    private readonly onBinaryReplaced?: (path: string) => void,
  ) {
    this.activePool = initialPool;
  }

  getBridge(projectRoot: string): RevivableProjectTransport {
    const key = canonicalizeProjectRoot(projectRoot);
    let transport = this.transports.get(key);
    if (!transport) {
      transport = new RevivableProjectTransport(this, key);
      this.transports.set(key, transport);
    }
    return transport;
  }

  getActiveBridgeForRoot(projectRoot: string): RevivableProjectTransport | null {
    const key = canonicalizeProjectRoot(projectRoot);
    const bridge = this.currentBridge(key);
    if (!bridge) return null;
    const transport = this.getBridge(key);
    transport.refreshStatusSubscription(bridge);
    return transport;
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

  setConfigureOverride(key: string, value: unknown): void {
    if (value === undefined) this.configureOverrides.delete(key);
    else this.configureOverrides.set(key, value);
    this.activePool.setConfigureOverride(key, value);
  }

  async reconfigure(projectRoot: string, overrides: Record<string, unknown>): Promise<void> {
    for (const [key, value] of Object.entries(overrides)) {
      if (value === undefined) this.configureOverrides.delete(key);
      else this.configureOverrides.set(key, value);
    }
    const pool = await this.ensureActivePool();
    await pool.reconfigure(projectRoot, overrides);
  }

  async replaceBinary(path: string): Promise<string> {
    const replaced = await this.activePool.replaceBinary(path);
    this.onBinaryReplaced?.(replaced);
    return replaced;
  }

  closeSession(projectRoot: string, session: string): Promise<void> {
    return this.activePool.closeSession(projectRoot, session);
  }

  async shutdown(): Promise<void> {
    const revival = this.revival;
    if (revival) {
      await Promise.allSettled([revival]);
    }
    await this.activePool.shutdown();
    for (const transport of this.transports.values()) {
      transport.refreshStatusSubscription(null);
    }
  }

  isShutdown(): boolean {
    return this.activePool.isShutdown();
  }

  async ensureActivePool(): Promise<AftTransportPool> {
    if (!this.activePool.isShutdown()) return this.activePool;
    if (this.revival) return this.revival;

    warn(
      "transport was shut down but new demand arrived — reviving (host quit hook fired without process exit?)",
    );
    const revival = this.createPool().then((pool) => {
      for (const [key, value] of this.configureOverrides) {
        pool.setConfigureOverride(key, value);
      }
      this.activePool = pool;
      for (const [root, transport] of this.transports) {
        transport.refreshStatusSubscription(pool.getActiveBridgeForRoot(root));
      }
      return pool;
    });
    this.revival = revival;
    revival.then(
      () => {
        if (this.revival === revival) this.revival = null;
      },
      () => {
        if (this.revival === revival) this.revival = null;
      },
    );
    return revival;
  }

  currentBridge(projectRoot: string): AftProjectTransport | null {
    return this.activePool.getActiveBridgeForRoot(projectRoot);
  }

  async send(
    projectRoot: string,
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    const pool = await this.ensureActivePool();
    const bridge = pool.getBridge(projectRoot);
    this.getBridge(projectRoot).refreshStatusSubscription(bridge);
    return bridge.send(command, params, options);
  }

  async toolCallOnProject(
    projectRoot: string,
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    const pool = await this.ensureActivePool();
    const bridge = pool.getBridge(projectRoot);
    this.getBridge(projectRoot).refreshStatusSubscription(bridge);
    return bridge.toolCall(sessionId, name, rawArgs, options);
  }
}

/**
 * Stable project facade returned to callers so a facade acquired before a host
 * quit hook still routes the next call through the replacement pool.
 */
class RevivableProjectTransport implements AftProjectTransport {
  private readonly statusListeners = new Map<StatusListener, () => void>();
  private readonly statusBridges = new Map<StatusListener, AftProjectTransport | null>();

  constructor(
    private readonly owner: RevivableTransportPool,
    private readonly projectRoot: string,
  ) {}

  getCwd(): string {
    return this.projectRoot;
  }

  getStatusBar() {
    return this.owner.currentBridge(this.projectRoot)?.getStatusBar();
  }

  getCachedStatus() {
    return this.owner.currentBridge(this.projectRoot)?.getCachedStatus() ?? null;
  }

  cacheStatusSnapshot(snapshot: Parameters<AftProjectTransport["cacheStatusSnapshot"]>[0]): void {
    this.owner.currentBridge(this.projectRoot)?.cacheStatusSnapshot(snapshot);
  }

  send(
    command: string,
    params?: Record<string, unknown>,
    options?: Parameters<AftProjectTransport["send"]>[2],
  ): Promise<Record<string, unknown>> {
    return this.owner.send(this.projectRoot, command, params, options);
  }

  toolCall(
    sessionId: string | undefined,
    name: string,
    rawArgs?: ToolCallArguments,
    options?: ToolCallOptions,
  ): Promise<ToolCallResult> {
    return this.owner.toolCallOnProject(this.projectRoot, sessionId, name, rawArgs, options);
  }

  subscribeStatus(listener: StatusListener): () => void {
    if (this.statusListeners.has(listener)) return () => this.removeStatusListener(listener);
    this.statusListeners.set(listener, () => {});
    this.bindStatusListener(listener, this.owner.currentBridge(this.projectRoot));
    return () => this.removeStatusListener(listener);
  }

  refreshStatusSubscription(bridge: AftProjectTransport | null): void {
    for (const listener of this.statusListeners.keys()) {
      this.bindStatusListener(listener, bridge);
    }
  }

  private bindStatusListener(listener: StatusListener, bridge: AftProjectTransport | null): void {
    if (this.statusBridges.get(listener) === bridge) return;
    const previousUnsubscribe = this.statusListeners.get(listener);
    previousUnsubscribe?.();
    const maybe = bridge as (AftProjectTransport & Partial<StatusSubscribableBridge>) | null;
    const unsubscribe =
      maybe && typeof maybe.subscribeStatus === "function"
        ? maybe.subscribeStatus(listener)
        : () => {};
    this.statusListeners.set(listener, unsubscribe);
    this.statusBridges.set(listener, bridge);
  }

  private removeStatusListener(listener: StatusListener): void {
    const unsubscribe = this.statusListeners.get(listener);
    this.statusListeners.delete(listener);
    this.statusBridges.delete(listener);
    unsubscribe?.();
  }
}
