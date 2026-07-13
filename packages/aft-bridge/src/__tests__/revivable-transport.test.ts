/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import type {
  BindIdentity,
  RequestOptions,
  RouteHandle,
  RouteTarget,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import { RevivableTransportPool } from "../revivable-transport.js";
import {
  type SubcClientLike,
  type SubcSubscriptionLike,
  SubcTransportPool,
} from "../subc-transport.js";

class FakeClient implements SubcClientLike {
  readonly routeOpens: BindIdentity[] = [];
  closed = 0;
  private nextChannel = 1;

  async routeOpen(_target: RouteTarget, identity: BindIdentity): Promise<RouteHandle> {
    this.routeOpens.push(identity);
    const channel = this.nextChannel++;
    return { channel, epoch: channel } as RouteHandle;
  }

  async request(_route: RouteHandle, _body: unknown, _options?: RequestOptions): Promise<unknown> {
    return {
      structuredContent: {
        success: true,
        text: "revived",
      },
    };
  }

  subscribe(
    _route: RouteHandle,
    _body: unknown,
    _onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike {
    return { unsubscribe: () => undefined };
  }

  async closeRouteChannel(_route: RouteHandle): Promise<void> {}

  close(): void {
    this.closed += 1;
  }
}

function makeSubcPool(client: FakeClient): SubcTransportPool {
  return new SubcTransportPool({
    connectionFile: "/tmp/fake-subc-connection.json",
    harness: "opencode",
    connect: async () => client,
  });
}

describe("RevivableTransportPool", () => {
  let previousLogger: Logger | undefined;

  beforeEach(() => {
    previousLogger = getActiveLogger();
  });

  afterEach(() => {
    const slot = globalThis as Record<symbol, unknown>;
    const key = Symbol.for("aft-bridge-active-logger");
    if (previousLogger) setActiveLogger(previousLogger);
    else delete slot[key];
  });

  test("revives a shut-down pool with fresh routes and repeats after the replacement shuts down", async () => {
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    const clients = [initialClient, revivedClient];
    let created = 0;
    const initialPool = makeSubcPool(initialClient);
    const owner = new RevivableTransportPool(initialPool, async () => {
      const client = clients[++created];
      if (!client) throw new Error("unexpected extra pool creation");
      return makeSubcPool(client);
    });
    const transport = owner.getBridge("/work/project");

    await transport.toolCall("before-shutdown", "read", {});
    await owner.shutdown();
    expect(owner.isShutdown()).toBe(true);
    expect(initialClient.closed).toBe(1);

    const revived = await transport.toolCall("after-shutdown", "read", {});
    expect(revived.text).toBe("revived");
    expect(created).toBe(1);
    expect(initialClient.routeOpens.map((identity) => identity.session)).toEqual([
      "before-shutdown",
    ]);
    expect(revivedClient.routeOpens.map((identity) => identity.session)).toEqual([
      "after-shutdown",
    ]);

    await owner.shutdown();
    expect(owner.isShutdown()).toBe(true);
    expect(revivedClient.closed).toBe(1);
  });

  test("concurrent demand during revival shares one replacement instance", async () => {
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    let created = 0;
    let releaseRevival!: () => void;
    const revivalGate = new Promise<void>((resolve) => {
      releaseRevival = resolve;
    });
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), async () => {
      created += 1;
      await revivalGate;
      return makeSubcPool(revivedClient);
    });
    const transport = owner.getBridge("/work/project");

    await owner.shutdown();
    const first = transport.toolCall("session-a", "read", {});
    const second = transport.toolCall("session-b", "read", {});
    await Promise.resolve();
    expect(created).toBe(1);
    releaseRevival();

    await Promise.all([first, second]);
    expect(created).toBe(1);
    expect(revivedClient.routeOpens.map((identity) => identity.session)).toEqual([
      "session-a",
      "session-b",
    ]);
  });

  test("logs the host-quit revival diagnosis", async () => {
    const messages: string[] = [];
    setActiveLogger({
      log: () => undefined,
      warn: (message) => messages.push(message),
      error: () => undefined,
    });
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), async () =>
      makeSubcPool(revivedClient),
    );

    await owner.shutdown();
    await owner.getBridge("/work/project").toolCall("session", "read", {});

    expect(messages).toContain(
      "transport was shut down but new demand arrived — reviving (host quit hook fired without process exit?)",
    );
  });
});
