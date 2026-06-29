import { describe, expect, test } from "bun:test";

import {
  type BindIdentity,
  type RouteTarget,
  SocketClosedError,
  SubcCallError,
  SubcError,
} from "@cortexkit/subc-client";

import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";

/** A controllable held-open subscription handle. */
class FakeSubscription {
  unsubscribed = 0;
  private resolveClosed!: () => void;
  private rejectClosed!: (err: Error) => void;
  readonly closed: Promise<void>;

  constructor(
    readonly channel: number,
    readonly onEvent: (event: Uint8Array) => void,
  ) {
    this.closed = new Promise<void>((resolve, reject) => {
      this.resolveClosed = resolve;
      this.rejectClosed = reject;
    });
    // Swallow the rejection if nobody is awaiting yet (the loop attaches its own).
    this.closed.catch(() => undefined);
  }

  /** Fire a bg_events nudge to the subscriber. */
  emit(): void {
    this.onEvent(new TextEncoder().encode(JSON.stringify({ op: "bg_events" })));
  }

  /** Simulate a socket drop / route GOODBYE — the loop should resubscribe. */
  drop(): void {
    this.rejectClosed(new Error("subscription dropped"));
  }

  /** Simulate an intentional StreamEnd — the loop should NOT resubscribe. */
  end(): void {
    this.resolveClosed();
  }

  unsubscribe(): void {
    this.unsubscribed += 1;
    this.resolveClosed();
  }
}

/** Records every routeOpen/request/subscribe so a test can assert caching + bodies. */
class FakeClient implements SubcClientLike {
  routeOpens: BindIdentity[] = [];
  requests: { channel: number; body: unknown }[] = [];
  subscriptions: FakeSubscription[] = [];
  closedRoutes: number[] = [];
  closed = 0;
  private nextChannel = 1;

  constructor(private readonly onRequest: (channel: number, body: unknown) => Promise<unknown>) {}

  async routeOpen(_target: RouteTarget, identity: BindIdentity): Promise<number> {
    this.routeOpens.push(identity);
    return this.nextChannel++;
  }

  async request(channel: number, body: unknown): Promise<unknown> {
    this.requests.push({ channel, body });
    return this.onRequest(channel, body);
  }

  subscribe(
    channel: number,
    _body: unknown,
    onEvent: (event: Uint8Array) => void,
  ): FakeSubscription {
    const sub = new FakeSubscription(channel, onEvent);
    this.subscriptions.push(sub);
    return sub;
  }

  async closeRouteChannel(channel: number): Promise<void> {
    this.closedRoutes.push(channel);
  }

  close(): void {
    this.closed += 1;
  }
}

/** Yield to the microtask/timer queue so the bg loop can advance. */
async function tick(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

function poolWith(
  client: FakeClient,
  harness = "opencode",
): { pool: SubcTransportPool; connects: number } {
  const state = { connects: 0 };
  const pool = new SubcTransportPool({
    connectionFile: "/tmp/fake-subc-connection.json",
    harness,
    connect: async () => {
      state.connects += 1;
      return client;
    },
  });
  return {
    pool,
    get connects() {
      return state.connects;
    },
  } as { pool: SubcTransportPool; connects: number };
}

// The Rust module wraps the flat response under structuredContent (S1 envelope).
function envelope(flat: Record<string, unknown>): Record<string, unknown> {
  return {
    content: [{ type: "text", text: flat.text }],
    isError: flat.success === false,
    structuredContent: flat,
  };
}

describe("SubcTransport.toolCall", () => {
  test("sends {name, arguments} and re-lifts structuredContent to the flat result", async () => {
    const client = new FakeClient(async () =>
      envelope({
        id: "req-1",
        success: true,
        text: "rendered output",
        status_bar: { errors: 0, warnings: 1 },
        bg_completions: [{ task_id: "bash-1" }],
      }),
    );
    const { pool } = poolWith(client);

    const result = await pool
      .getBridge("/work/proj")
      .toolCall("sess-1", "read", { filePath: "a.ts" });

    // Body is the tool-route shape, NOT {method, params}.
    expect(client.requests[0]?.body).toEqual({
      name: "read",
      arguments: { filePath: "a.ts" },
    });
    // structuredContent re-lifted: sidecars survive as flat top-level fields.
    expect(result.success).toBe(true);
    expect(result.text).toBe("rendered output");
    expect(result.status_bar).toEqual({ errors: 0, warnings: 1 });
    expect(result.bg_completions).toEqual([{ task_id: "bash-1" }]);
    // getStatusBar captured + normalized the counts from the response (full shape).
    expect(pool.getBridge("/work/proj").getStatusBar()).toEqual({
      errors: 0,
      warnings: 1,
      dead_code: 0,
      unused_exports: 0,
      duplicates: 0,
      todos: 0,
      tier2_stale: false,
    });
  });

  test("preview:true is placed at the top level of the request body", async () => {
    const client = new FakeClient(async () =>
      envelope({ id: "r", success: true, text: "preview" }),
    );
    const { pool } = poolWith(client);

    await pool.getBridge("/work/proj").toolCall("s", "edit", { oldString: "a" }, { preview: true });

    expect(client.requests[0]?.body).toEqual({
      name: "edit",
      arguments: { oldString: "a" },
      preview: true,
    });
  });

  test("caches the route per (root, harness, session) and reuses it", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);
    const t = pool.getBridge("/work/proj");

    await t.toolCall("sess-A", "read", {});
    await t.toolCall("sess-A", "grep", {}); // same identity -> same channel, no new routeOpen
    await t.toolCall("sess-B", "read", {}); // different session -> new route

    expect(client.routeOpens.length).toBe(2);
    expect(client.routeOpens[0]?.session).toBe("sess-A");
    expect(client.routeOpens[1]?.session).toBe("sess-B");
    // First two calls rode the same channel.
    expect(client.requests[0]?.channel).toBe(client.requests[1]?.channel);
    expect(client.requests[2]?.channel).not.toBe(client.requests[0]?.channel);
  });

  test("session-less call falls back to the __default__ session", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);

    await pool.getBridge("/work/proj").toolCall(undefined, "read", {});

    expect(client.routeOpens[0]?.session).toBe("__default__");
  });

  test("a tool-level success:false reply is returned, not thrown", async () => {
    const client = new FakeClient(async () =>
      envelope({ id: "r", success: false, code: "path_not_found", text: "no such file" }),
    );
    const { pool } = poolWith(client);

    const result = await pool.getBridge("/work/proj").toolCall("s", "read", {});
    expect(result.success).toBe(false);
    expect(result.code).toBe("path_not_found");
  });
});

describe("SubcTransport Rd reconnect", () => {
  // The raw request() path rejects with REAL error types — base SubcError
  // (timeout / route GOODBYE / daemon Error frame) or a socket error (closed /
  // reset / pre-send write failure) — and NEVER a managed SubcCallError. These
  // tests use those real types so the `isConsumerReconnectTransient` classifier
  // is exercised exactly as it will be in production (a prior version faked
  // SubcCallError, which the classifier treats as transient and so masked the
  // wrong-instanceof bug).

  test("a dead-socket error (transient) drops the channel AND client; next call reconnects", async () => {
    let calls = 0;
    let madeClients = 0;
    const onRequest = async (): Promise<unknown> => {
      calls += 1;
      if (calls === 1) throw new SocketClosedError("subc socket closed");
      return envelope({ id: "r", success: true, text: "recovered" });
    };
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return new FakeClient(onRequest);
      },
    });
    const t = pool.getBridge("/work/proj");

    // First call surfaces the transport error (Rd never auto-retries).
    await expect(t.toolCall("s", "read", {})).rejects.toBeInstanceOf(SocketClosedError);

    // Second call reconnects (a NEW client from the factory) and recovers.
    const result = await t.toolCall("s", "read", {});
    expect(result.text).toBe("recovered");
    expect(madeClients).toBe(2); // the dead client was dropped, a fresh one connected
  });

  test("a not-queued write failure (transient, not_sent-equivalent) drops the client", async () => {
    // SubcWriteNotQueuedError is the raw-path analog of `not_sent`: bytes never
    // left the local socket. isConsumerReconnectTransient classifies it transient.
    let calls = 0;
    let madeClients = 0;
    const notQueued = Object.assign(new Error("write not queued"), { code: "EPIPE" });
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return new FakeClient(async () => {
          calls += 1;
          if (calls === 1) throw notQueued; // EPIPE -> transient
          return envelope({ id: "r", success: true, text: "ok" });
        });
      },
    });
    const t = pool.getBridge("/work/proj");
    await expect(t.toolCall("s", "read", {})).rejects.toBe(notQueued);
    await t.toolCall("s", "read", {});
    expect(madeClients).toBe(2);
  });

  test("a plain timeout (non-transient SubcError) KEEPS the client, drops only the route", async () => {
    // Q1: a lost/late response does NOT prove the connection is dead. Keep the
    // client (no reconnect); the route is re-opened on the next call. Mutation-
    // safe: the error is surfaced, never auto-retried.
    let calls = 0;
    let madeClients = 0;
    const client = new FakeClient(async () => {
      calls += 1;
      if (calls === 1) throw new SubcError("request on channel 1 timed out after 30000ms");
      return envelope({ id: "r", success: true, text: "second" });
    });
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => {
        madeClients += 1;
        return client;
      },
    });
    const t = pool.getBridge("/work/proj");

    await expect(t.toolCall("s", "edit", {})).rejects.toBeInstanceOf(SubcError);
    expect(client.closed).toBe(0); // client kept alive
    // The route was dropped, so the next call re-opens it on the SAME client.
    const result = await t.toolCall("s", "edit", {});
    expect(result.text).toBe("second");
    expect(madeClients).toBe(1); // never reconnected
    expect(client.routeOpens.length).toBe(2); // route re-opened
    expect(calls).toBe(2); // exactly two underlying requests — no auto-retry
  });
});

describe("SubcTransport.send", () => {
  test("configure is satisfied locally and never hits the wire", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);

    const res = await pool
      .getBridge("/work/proj")
      .send("configure", { project_root: "/work/proj" });
    expect(res.success).toBe(true);
    expect(res.subc_local).toBe(true);
    expect(client.requests.length).toBe(0); // no route request issued
  });

  test("a native command rides the route as {name, arguments} scoped to its session", async () => {
    const client = new FakeClient(async () =>
      envelope({ id: "r", success: true, text: "", bg_completions: [] }),
    );
    const { pool } = poolWith(client);

    await pool.getBridge("/work/proj").send("bash_drain_completions", { session_id: "sess-Z" });

    expect(client.routeOpens[0]?.session).toBe("sess-Z");
    expect(client.requests[0]?.body).toEqual({
      name: "bash_drain_completions",
      arguments: { session_id: "sess-Z" },
    });
  });
});

describe("SubcTransport bg_events subscription (S3)", () => {
  function bgPool(client: FakeClient): {
    pool: SubcTransportPool;
    nudges: { root: string; session: string }[];
  } {
    const nudges: { root: string; session: string }[] = [];
    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake",
      harness: "opencode",
      connect: async () => client,
      onBgEventsNudge: (root, session) => nudges.push({ root, session }),
      bgBackoffSleep: async () => undefined, // no real delay in tests
    });
    return { pool, nudges };
  }

  test("opens a dedicated bg subscription on a DISTINCT channel after the first tool call", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge("/work/proj").toolCall("sess-1", "read", {});
    await tick();

    // Two route.opens: the tool route + the dedicated bg_events route.
    expect(client.routeOpens.length).toBe(2);
    expect(client.subscriptions.length).toBe(1);
    // The bg subscription rides a DIFFERENT channel from the tool request.
    const toolChannel = client.requests[0]?.channel;
    expect(client.subscriptions[0]?.channel).not.toBe(toolChannel);
  });

  test("a nudge AND the initial (re)subscribe both fire onBgEventsNudge (forced-drain trigger)", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool, nudges } = bgPool(client);

    await pool.getBridge("/work/proj").toolCall("sess-1", "read", {});
    await tick();
    // Immediate replay nudge on subscribe.
    expect(nudges.length).toBe(1);

    // A wake nudge from the module drives another drain.
    client.subscriptions[0]?.emit();
    expect(nudges.length).toBe(2);
    expect(nudges[1]).toEqual({ root: pool.getBridge("/work/proj").getCwd(), session: "sess-1" });
  });

  test("is idempotent — one subscription per session even across many tool calls", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);
    const t = pool.getBridge("/work/proj");

    await t.toolCall("sess-1", "read", {});
    await tick();
    await t.toolCall("sess-1", "grep", {});
    await t.toolCall("sess-1", "edit", {});
    await tick();

    expect(client.subscriptions.length).toBe(1); // never re-subscribed for the same session
  });

  test("INDEPENDENT reconnect: a dropped subscription resubscribes + re-drains with NO tool call (idle-stranding fix)", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool, nudges } = bgPool(client);

    await pool.getBridge("/work/proj").toolCall("sess-1", "read", {});
    await tick();
    expect(nudges.length).toBe(1); // initial subscribe replay
    const firstSub = client.subscriptions[0];

    // Socket drop — NO tool call follows (idle agent). The loop must resubscribe.
    firstSub?.drop();
    await tick();
    await tick();

    expect(client.subscriptions.length).toBe(2); // resubscribed independently
    // The resubscribe fired another forced-drain replay (recovers a completion
    // that landed while disconnected).
    expect(nudges.length).toBe(2);
  });

  test("StreamEnd (intentional close) does NOT resubscribe", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge("/work/proj").toolCall("sess-1", "read", {});
    await tick();
    client.subscriptions[0]?.end(); // StreamEnd
    await tick();
    await tick();

    expect(client.subscriptions.length).toBe(1); // no resubscribe on a clean end
  });

  test("closeSession stops the subscription and closes both routes", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = bgPool(client);

    await pool.getBridge("/work/proj").toolCall("sess-1", "read", {});
    await tick();
    await pool.closeSession("/work/proj", "sess-1");

    expect(client.subscriptions[0]?.unsubscribed).toBe(1);
    // Both the bg route and the tool route were closed.
    expect(client.closedRoutes.length).toBe(2);
  });

  test("no bg subscription is opened when onBgEventsNudge is not configured", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client); // no onBgEventsNudge

    await pool.getBridge("/work/proj").toolCall("sess-1", "read", {});
    await tick();

    expect(client.subscriptions.length).toBe(0);
    expect(client.routeOpens.length).toBe(1); // tool route only
  });
});

describe("SubcTransportPool lifecycle", () => {
  test("getActiveBridgeForRoot returns null before connect, a transport after", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);

    expect(pool.getActiveBridgeForRoot("/work/proj")).toBeNull();
    await pool.getBridge("/work/proj").toolCall("s", "read", {});
    expect(pool.getActiveBridgeForRoot("/work/proj")).not.toBeNull();
  });

  test("shutdown closes the client and rejects further calls", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);
    await pool.getBridge("/work/proj").toolCall("s", "read", {});

    await pool.shutdown();
    expect(client.closed).toBe(1);
    await expect(pool.getBridge("/work/proj").toolCall("s", "read", {})).rejects.toBeInstanceOf(
      SubcCallError,
    );
  });

  test("setConfigureOverride and replaceBinary are no-ops over subc", async () => {
    const client = new FakeClient(async () => envelope({ id: "r", success: true, text: "" }));
    const { pool } = poolWith(client);
    expect(() => pool.setConfigureOverride("k", "v")).not.toThrow();
    await expect(pool.replaceBinary("/new/path")).resolves.toBe("/new/path");
  });
});
