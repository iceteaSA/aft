/// <reference path="../bun-test.d.ts" />

// Verified-directory redirect: the TUI process can run in a different
// directory (e.g. $HOME, attached to a serve/Desktop host) than the process
// hosting the session's warm bridge. Port discovery is hash(directory)-keyed,
// so without the redirect the TUI client only ever reaches its own bridgeless
// instance and renders the lazy placeholder forever. The placeholder carries
// `verified_directory`; the client follows it once.

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  __resetRpcRedirectsForTest,
  AftRpcClient,
  subscribeRpcRedirects,
} from "../shared/rpc-client.js";
import { __resetRpcNotificationsForTest } from "../shared/rpc-notifications.js";
import { AftRpcServer } from "../shared/rpc-server.js";

const tempRoots = new Set<string>();
const servers = new Set<AftRpcServer>();

function makeRoot(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-rpc-redirect-"));
  tempRoots.add(root);
  return root;
}

async function startServer(
  storageDir: string,
  directory: string,
  status: Record<string, unknown>,
): Promise<AftRpcServer> {
  const server = new AftRpcServer(storageDir, directory);
  server.handle("status", async () => status);
  await server.start();
  servers.add(server);
  return server;
}

afterEach(() => {
  for (const server of servers) {
    try {
      server.stop();
    } catch {
      // best-effort
    }
  }
  servers.clear();
  __resetRpcNotificationsForTest();
  __resetRpcRedirectsForTest();
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("verified-directory redirect", () => {
  test("placeholder with verified_directory redirects to the warm instance", async () => {
    const root = makeRoot();
    const storageDir = join(root, "storage");
    const tuiDir = join(root, "home");
    const projectDir = join(root, "project");

    await startServer(storageDir, tuiDir, {
      success: true,
      status: "not_initialized",
      verified_directory: projectDir,
      message: "placeholder",
    });
    await startServer(storageDir, projectDir, {
      success: true,
      project_root: projectDir,
      served_directory: projectDir,
      cache_role: "main",
    });

    const learned: Array<[string, string]> = [];
    subscribeRpcRedirects((from, to) => learned.push([from, to]));

    const client = new AftRpcClient(storageDir, tuiDir);
    const result = await client.call<Record<string, unknown>>("status", {});

    expect(result.cache_role).toBe("main");
    expect(result.project_root).toBe(projectDir);
    expect(learned).toEqual([[tuiDir, projectDir]]);

    // resolveEndpoint follows the learned redirect: the socket must subscribe
    // to the instance that owns the bridge, not the placeholder instance.
    const endpoint = await client.resolveEndpoint();
    const direct = await new AftRpcClient(storageDir, projectDir).resolveEndpoint();
    expect(endpoint?.port).toBe(direct?.port);
  });

  test("redirect is one hop: a placeholder chain does not recurse and stays unlearned", async () => {
    const root = makeRoot();
    const storageDir = join(root, "storage");
    const tuiDir = join(root, "home");
    const middleDir = join(root, "middle");
    const farDir = join(root, "far");

    await startServer(storageDir, tuiDir, {
      success: true,
      status: "not_initialized",
      verified_directory: middleDir,
    });
    // The target is itself bridgeless and points elsewhere — must NOT chase.
    await startServer(storageDir, middleDir, {
      success: true,
      status: "not_initialized",
      verified_directory: farDir,
    });
    await startServer(storageDir, farDir, {
      success: true,
      project_root: farDir,
      cache_role: "main",
    });

    const learned: Array<[string, string]> = [];
    subscribeRpcRedirects((from, to) => learned.push([from, to]));

    const client = new AftRpcClient(storageDir, tuiDir);
    const result = await client.call<Record<string, unknown>>("status", {});

    // Kept the ORIGINAL placeholder (the middle placeholder is no improvement),
    // and the dead redirect was never learned — the socket must not re-home
    // onto another bridgeless instance.
    expect(result.status).toBe("not_initialized");
    expect(result.verified_directory).toBe(middleDir);
    expect(learned).toEqual([]);
  });

  test("self-referencing verified_directory is ignored", async () => {
    const root = makeRoot();
    const storageDir = join(root, "storage");
    const tuiDir = join(root, "home");

    await startServer(storageDir, tuiDir, {
      success: true,
      status: "not_initialized",
      verified_directory: tuiDir,
    });

    const client = new AftRpcClient(storageDir, tuiDir);
    const result = await client.call<Record<string, unknown>>("status", {});
    expect(result.status).toBe("not_initialized");
  });
});
