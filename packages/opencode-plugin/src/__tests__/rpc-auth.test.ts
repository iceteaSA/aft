/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, spyOn, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { AftRpcClient } from "../shared/rpc-client.js";
import { AftRpcServer } from "../shared/rpc-server.js";
import {
  isPidAlive,
  parseRpcPortRecord,
  rpcPortFileDir,
  rpcPortFilePath,
} from "../shared/rpc-utils.js";

/** Resolve the (single) per-instance port file written by an AftRpcServer. */
function resolveInstancePortFile(storageDir: string, directory: string): string {
  const portsDir = rpcPortFileDir(storageDir, directory);
  const entries = readdirSync(portsDir).filter((entry) => entry.endsWith(".json"));
  if (entries.length !== 1) {
    throw new Error(
      `expected exactly one port file in ${portsDir}, found ${entries.length}: ${entries.join(", ")}`,
    );
  }
  return join(portsDir, entries[0] as string);
}

const tempRoots = new Set<string>();

function makeFixture() {
  const root = mkdtempSync(join(tmpdir(), "aft-rpc-auth-"));
  tempRoots.add(root);
  return { storageDir: join(root, "storage"), directory: join(root, "project") };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("AFT RPC auth", () => {
  test("writes token to port file and requires it for requests", async () => {
    const fixture = makeFixture();
    const server = new AftRpcServer(fixture.storageDir, fixture.directory);
    server.handle("echo", async (params) => ({ ok: true, params }));

    try {
      const port = await server.start();
      const instancePortFile = resolveInstancePortFile(fixture.storageDir, fixture.directory);
      const portFile = JSON.parse(readFileSync(instancePortFile, "utf-8")) as {
        port: number;
        token: string;
      };
      expect(portFile.port).toBe(port);
      expect(portFile.token).toMatch(/^[0-9a-f]{64}$/);
      if (process.platform !== "win32") {
        expect(statSync(instancePortFile).mode & 0o777).toBe(0o600);
      }

      const forbidden = await fetch(`http://127.0.0.1:${port}/rpc/echo`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ value: 1 }),
      });
      expect(forbidden.status).toBe(403);

      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      const result = await client.call<{ ok: boolean; params: Record<string, unknown> }>("echo", {
        value: 1,
      });
      expect(result).toEqual({ ok: true, params: { value: 1 } });
    } finally {
      server.stop();
    }
  });

  test("client parses legacy integer port files without throwing", async () => {
    // Backward-compat at the parser level: old aft versions wrote a plain integer
    // port file. The new client must still parse those files (without exceptions
    // about JSON parsing or missing token field) and reach the network layer.
    //
    // We use a plain http.Server that requires a token (matches new aft server
    // behavior). The client reading the legacy integer file gets `token: null`,
    // sends the request, server returns 403. The 403 proves the legacy parser
    // path works end-to-end — only the server's token check rejects, not the client.
    const fixture = makeFixture();
    const { createServer } = await import("node:http");
    const tokenRequiredServer = createServer((req, res) => {
      if (req.url === "/health") {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ ok: true }));
        return;
      }
      let body = "";
      req.on("data", (chunk) => {
        body += chunk;
      });
      req.on("end", () => {
        const params = JSON.parse(body) as { token?: string | null };
        if (params.token == null) {
          res.writeHead(403, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ error: "Forbidden" }));
          return;
        }
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ ok: true }));
      });
    });
    await new Promise<void>((resolve) => tokenRequiredServer.listen(0, "127.0.0.1", resolve));
    const address = tokenRequiredServer.address();
    const port = typeof address === "object" && address ? address.port : 0;

    try {
      const legacyPortPath = rpcPortFilePath(fixture.storageDir, fixture.directory);
      mkdirSync(rpcPortFileDir(fixture.storageDir, fixture.directory), { recursive: true });
      // Write legacy integer-only format AND keep the per-instance ports dir
      // empty so the client falls back to the legacy file.
      writeFileSync(legacyPortPath, String(port), "utf-8");

      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      // The client must parse the integer file without throwing JSON errors,
      // and the request must reach the server (where it's rejected with 403
      // because no token was supplied).
      await expect(client.call("echo", {})).rejects.toThrow("403");
    } finally {
      await new Promise<void>((resolve) => tokenRequiredServer.close(() => resolve()));
    }
  });

  test("client appends legacy port after stale per-instance entries and cleans stale JSON after two failures", async () => {
    const fixture = makeFixture();
    const { createServer } = await import("node:http");

    let rpcCalls = 0;
    const legacyServer = createServer((req, res) => {
      if (req.url === "/health") {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ ok: true }));
        return;
      }

      let body = "";
      req.on("data", (chunk) => {
        body += chunk;
      });
      req.on("end", () => {
        if (req.url?.startsWith("/rpc/")) {
          rpcCalls++;
          const params = JSON.parse(body) as Record<string, unknown>;
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ ok: true, rpcCalls, echoed: params }));
          return;
        }
        res.writeHead(404);
        res.end();
      });
    });

    await new Promise<void>((resolve) => legacyServer.listen(0, "127.0.0.1", resolve));
    const address = legacyServer.address();
    const port = typeof address === "object" && address ? address.port : 0;

    const stalePortProbe = createServer((_req, res) => {
      res.writeHead(500);
      res.end();
    });
    await new Promise<void>((resolve) => stalePortProbe.listen(0, "127.0.0.1", resolve));
    const staleAddress = stalePortProbe.address();
    const stalePort = typeof staleAddress === "object" && staleAddress ? staleAddress.port : 0;
    await new Promise<void>((resolve) => stalePortProbe.close(() => resolve()));

    try {
      const portsDir = rpcPortFileDir(fixture.storageDir, fixture.directory);
      mkdirSync(portsDir, { recursive: true });
      const stalePath = join(portsDir, "stale.json");
      writeFileSync(stalePath, JSON.stringify({ port: stalePort, token: "stale-token" }), "utf-8");

      const legacyPortPath = rpcPortFilePath(fixture.storageDir, fixture.directory);
      mkdirSync(dirname(legacyPortPath), { recursive: true });
      writeFileSync(legacyPortPath, String(port), "utf-8");

      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      const first = await client.call<{
        ok: boolean;
        rpcCalls: number;
        echoed: Record<string, unknown>;
      }>("echo", { value: "first" });
      expect(first.ok).toBe(true);
      expect(first.echoed.value).toBe("first");
      expect(existsSync(stalePath)).toBe(true);

      const second = await client.call<{
        ok: boolean;
        rpcCalls: number;
        echoed: Record<string, unknown>;
      }>("echo", { value: "second" });
      expect(second.ok).toBe(true);
      expect(second.rpcCalls).toBe(2);
      expect(second.echoed.value).toBe("second");
      expect(existsSync(stalePath)).toBe(false);
    } finally {
      await new Promise<void>((resolve) => legacyServer.close(() => resolve()));
    }
  });

  test("client call can be aborted while an RPC request is in flight", async () => {
    const fixture = makeFixture();
    const server = new AftRpcServer(fixture.storageDir, fixture.directory);
    let markHandlerStarted!: () => void;
    const handlerStarted = new Promise<void>((resolve) => {
      markHandlerStarted = resolve;
    });
    let releaseHandler!: () => void;
    const keepHandlerOpen = new Promise<void>((resolve) => {
      releaseHandler = resolve;
    });
    server.handle("slow", async () => {
      markHandlerStarted();
      await keepHandlerOpen;
      return { ok: true };
    });

    try {
      await server.start();
      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      const controller = new AbortController();
      const pending = client.call("slow", {}, { signal: controller.signal });

      await handlerStarted;
      controller.abort();
      releaseHandler();
      await expect(pending).rejects.toThrow();
    } finally {
      releaseHandler();
      server.stop();
    }
  });

  test("client interoperates with a tokenless legacy server", async () => {
    // True backward-compat test: simulate an OLD aft server that never enforced
    // tokens (pre-#23 behavior). New client must still talk to it successfully.
    // We mock this with a plain http.Server that ignores any token field.
    const fixture = makeFixture();
    const { createServer } = await import("node:http");
    const legacyServer = createServer((req, res) => {
      // Legacy aft servers responded to /health unauthenticated and /rpc/* without
      // checking any token. We mirror that here so the new client's resolvePortInfo
      // path (which health-checks before sending RPC) accepts the legacy server.
      if (req.url === "/health") {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ ok: true }));
        return;
      }
      let body = "";
      req.on("data", (chunk) => {
        body += chunk;
      });
      req.on("end", () => {
        if (req.url?.startsWith("/rpc/")) {
          // Old server: accept without checking token field.
          const params = JSON.parse(body) as Record<string, unknown>;
          res.writeHead(200, { "Content-Type": "application/json" });
          res.end(JSON.stringify({ ok: true, echoed: params }));
          return;
        }
        res.writeHead(404);
        res.end();
      });
    });

    await new Promise<void>((resolve) => legacyServer.listen(0, "127.0.0.1", resolve));
    const address = legacyServer.address();
    const port = typeof address === "object" && address ? address.port : 0;

    try {
      // Write legacy integer-only port file (matches what old plugins wrote).
      const portPath = rpcPortFilePath(fixture.storageDir, fixture.directory);
      mkdirSync(dirname(portPath), { recursive: true });
      writeFileSync(portPath, String(port), "utf-8");

      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      const result = await client.call<{ ok: boolean; echoed: Record<string, unknown> }>("echo", {
        value: 42,
      });
      expect(result.ok).toBe(true);
      // params include the token field (null), which the legacy server should ignore.
      expect(result.echoed.value).toBe(42);
    } finally {
      await new Promise<void>((resolve) => legacyServer.close(() => resolve()));
    }
  });

  test("client refreshes cached port and retries once after stale port failure", async () => {
    const fixture = makeFixture();
    const staleServer = new AftRpcServer(fixture.storageDir, fixture.directory);
    staleServer.handle("echo", async () => ({ stale: true }));

    const freshServer = new AftRpcServer(fixture.storageDir, fixture.directory);
    freshServer.handle("echo", async (params) => ({ fresh: true, params }));

    try {
      await staleServer.start();
      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      await expect(client.call("echo", { value: "first" })).resolves.toMatchObject({
        stale: true,
      });

      staleServer.stop();
      await freshServer.start();

      const result = await client.call<{ fresh: boolean; params: Record<string, unknown> }>(
        "echo",
        { value: "second" },
      );

      expect(result.fresh).toBe(true);
      expect(result.params.value).toBe("second");
    } finally {
      staleServer.stop();
      freshServer.stop();
    }
  });

  test("server records pid and started_at in the port file", async () => {
    const fixture = makeFixture();
    const server = new AftRpcServer(fixture.storageDir, fixture.directory);
    server.handle("echo", async () => ({ ok: true }));
    try {
      const before = Date.now();
      await server.start();
      const file = JSON.parse(
        readFileSync(resolveInstancePortFile(fixture.storageDir, fixture.directory), "utf-8"),
      ) as { pid: number; started_at: number };
      expect(file.pid).toBe(process.pid);
      expect(file.started_at).toBeGreaterThanOrEqual(before);
      expect(file.started_at).toBeLessThanOrEqual(Date.now());
    } finally {
      server.stop();
    }
  });

  test("client skips and reclaims a port file whose pid is dead, without health-checking it", async () => {
    const fixture = makeFixture();
    const portsDir = rpcPortFileDir(fixture.storageDir, fixture.directory);
    mkdirSync(portsDir, { recursive: true });

    // A dead-pid file pointing at a port nobody is listening on. The client must
    // delete it on read (not wait out a health-check). PID 1 is init/launchd —
    // always alive — so use a very high pid that is almost certainly not a live
    // process to represent a crashed plugin.
    const deadPid = 2_000_000_000;
    const deadFile = join(portsDir, "dead.json");
    writeFileSync(
      deadFile,
      JSON.stringify({ port: 9, token: "dead-token", pid: deadPid, started_at: 1 }),
      "utf-8",
    );

    // A live server for this project (records its own live pid).
    const server = new AftRpcServer(fixture.storageDir, fixture.directory);
    server.handle("echo", async (params) => ({ ok: true, params }));
    try {
      await server.start();
      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      const result = await client.call<{ ok: boolean; params: Record<string, unknown> }>("echo", {
        value: "live",
      });
      expect(result.ok).toBe(true);
      expect(result.params.value).toBe("live");
      // The dead-pid file is reclaimed on read.
      expect(existsSync(deadFile)).toBe(false);
    } finally {
      server.stop();
    }
  });

  test("client prefers the newest live server when two are running", async () => {
    const fixture = makeFixture();
    const older = new AftRpcServer(fixture.storageDir, fixture.directory);
    older.handle("echo", async () => ({ which: "older" }));
    const newer = new AftRpcServer(fixture.storageDir, fixture.directory);
    newer.handle("echo", async () => ({ which: "newer" }));
    let now = 1_000;
    const dateNow = spyOn(Date, "now").mockImplementation(() => now);
    try {
      await older.start();
      // The client orders candidates by the recorded started_at field; set it
      // deterministically instead of sleeping and hoping the clock advances.
      now = 2_000;
      await newer.start();

      const client = new AftRpcClient(fixture.storageDir, fixture.directory);
      const result = await client.call<{ which: string }>("echo", {});
      // Both are live (same pid, this test process); newest-by-started_at wins.
      expect(result.which).toBe("newer");
    } finally {
      dateNow.mockRestore();
      older.stop();
      newer.stop();
    }
  });
});

describe("rpc-utils pid/record parsing", () => {
  test("parseRpcPortRecord reads pid + started_at and rejects junk", () => {
    expect(
      parseRpcPortRecord(JSON.stringify({ port: 5, token: "t", pid: 42, started_at: 99 })),
    ).toEqual({ port: 5, token: "t", pid: 42, started_at: 99 });
    // Legacy JSON without pid.
    expect(parseRpcPortRecord(JSON.stringify({ port: 5, token: "t" }))).toEqual({
      port: 5,
      token: "t",
      pid: undefined,
      started_at: undefined,
    });
    // Legacy bare-integer (unauthenticated).
    expect(parseRpcPortRecord("5432")).toEqual({ port: 5432, token: null });
    expect(parseRpcPortRecord("")).toBeNull();
    expect(parseRpcPortRecord("{not json")).toBeNull();
    expect(parseRpcPortRecord(JSON.stringify({ port: 0 }))).toBeNull();
    expect(parseRpcPortRecord(JSON.stringify({ port: 70000 }))).toBeNull();
  });

  test("isPidAlive: current process alive, bogus pids dead", () => {
    expect(isPidAlive(process.pid)).toBe(true);
    expect(isPidAlive(undefined)).toBe(false);
    expect(isPidAlive(0)).toBe(false);
    expect(isPidAlive(-1)).toBe(false);
    expect(isPidAlive(2_000_000_000)).toBe(false);
  });
});
