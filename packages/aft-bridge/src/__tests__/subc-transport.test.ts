import { describe, expect, test } from "bun:test";

import {
  type BindIdentity,
  type RouteTarget,
  SocketClosedError,
  SubcCallError,
  SubcError,
} from "@cortexkit/subc-client";

import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";

/** Records every routeOpen/request so a test can assert caching + bodies. */
class FakeClient implements SubcClientLike {
  routeOpens: BindIdentity[] = [];
  requests: { channel: number; body: unknown }[] = [];
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

  close(): void {
    this.closed += 1;
  }
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
