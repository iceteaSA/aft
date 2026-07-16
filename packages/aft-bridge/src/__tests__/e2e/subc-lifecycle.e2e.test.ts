/// <reference path="../../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { AftProjectTransport, AftTransportPool, ToolCallResult } from "../../transport.js";
import { createAftTransportPool } from "../../transport-factory.js";
import {
  type AftModuleRuntime,
  type PreparedSubcLane,
  prepareSubcLane,
  type SubcRig,
  startSubcRig,
} from "./subc-rig.js";

const initialPrepared = await prepareSubcLane();
const maybeDescribe = initialPrepared.skipReason ? describe.skip : describe;
const describeName = initialPrepared.skipReason
  ? `subc transport lifecycle e2e lane (skipped: ${initialPrepared.skipReason})`
  : "subc transport lifecycle e2e lane";
let prepared: PreparedSubcLane = initialPrepared;

maybeDescribe(describeName, () => {
  test("module respawn mid-session reopens the route and recovers bg_events", async () => {
    await withRig(async ({ rig, pools, nudges }) => {
      const pool = await createPool(prepared, rig, pools, nudges);
      const transport = pool.getBridge(rig.projectDir);
      const session = `respawn-${Date.now()}`;

      const warm = await transport.toolCall(session, "read", { filePath: "seed.txt" });
      assertToolSuccess(warm, "warm read");
      const before = await rig.waitForAftModuleRuntime(10_000);

      const recoveryStarted = Date.now();
      process.kill(before.pid, "SIGTERM");
      const after = await rig.waitForAftModuleRestart(before, 30_000);
      if (before.restartCount !== null && after.restartCount !== null) {
        expect(after.restartCount).toBeGreaterThan(before.restartCount);
      }
      expect(after.pid).not.toBe(before.pid);

      const postRespawn = await transport.toolCall(session, "read", { filePath: "seed.txt" });
      const recoveryElapsed = Date.now() - recoveryStarted;
      assertToolSuccess(postRespawn, "post-respawn read");
      expect(JSON.stringify(postRespawn)).toContain("seed");
      expect(JSON.stringify(postRespawn)).not.toContain("executor actor is not registered");
      expect(recoveryElapsed).toBeLessThanOrEqual(30_000);

      const nudgeStart = nudges.length;
      const spawned = await transport.toolCall(session, "bash", {
        command: "sleep 1; echo respawn-bg-ok",
        background: true,
      });
      assertToolSuccess(spawned, "post-respawn background bash spawn");
      const taskId = taskIdFrom(spawned);
      await waitFor(() => nudges.length > nudgeStart, 12_000, "post-respawn bg_events nudge");
      const drain = await waitForCompletion(transport, session, taskId, 8_000);
      expect(JSON.stringify(drain)).toContain("respawn-bg-ok");
      await transport.send("bash_ack_completions", { session_id: session, task_ids: [taskId] });

      console.info(
        `[subc-lifecycle] respawn kill_pid=${before.pid} restart_count=${formatRestartCount(
          before,
        )}->${formatRestartCount(after)} new_pid=${after.pid} post_success=${postRespawn.success} recovery_ms=${recoveryElapsed}`,
      );
    });
  }, 90_000);

  test("multi-root sessions on one daemon stay isolated", async () => {
    await withRig(async ({ rig, pools, nudges }) => {
      const rootA = rig.projectDir;
      const rootB = await rig.createProject("project-b");
      const pool = await createPool(prepared, rig, pools, nudges);
      const bridgeA = pool.getBridge(rootA);
      const bridgeB = pool.getBridge(rootB);

      const writeA = await bridgeA.toolCall("multi-a", "write", {
        filePath: "only-a.txt",
        content: "root-a-only\n",
      });
      assertToolSuccess(writeA, "root A write");
      expect(await readFile(join(rootA, "only-a.txt"), "utf8")).toBe("root-a-only\n");
      expect(existsSync(join(rootB, "only-a.txt"))).toBe(false);

      const grepA = await bridgeA.toolCall("multi-a", "grep", { pattern: "root-a-only" });
      assertToolSuccess(grepA, "root A grep");
      expect(JSON.stringify(grepA)).toContain("only-a.txt");
      const grepB = await bridgeB.toolCall("multi-b", "grep", { pattern: "root-a-only" });
      assertToolSuccess(grepB, "root B grep");
      expect(JSON.stringify(grepB)).not.toContain("only-a.txt");

      await pool.closeSession(rootA, "multi-a");
      const readB = await bridgeB.toolCall("multi-b", "read", { filePath: "seed.txt" });
      assertToolSuccess(readB, "root B read after root A closeSession");
      expect(JSON.stringify(readB)).toContain("seed");
    });
  }, 60_000);

  test("first binds survive cold-configure contention", async () => {
    await withRig(async ({ rig, pools, nudges }) => {
      const roots = await Promise.all(
        ["cold-a", "cold-b", "cold-c"].map((name) =>
          rig.createProject(name, { fileCount: 700, nestedDirs: 35 }),
        ),
      );
      const pool = await createPool(prepared, rig, pools, nudges);
      const started = Date.now();
      const firstCalls = await Promise.all(
        roots.map(async (root, index) => {
          const result = await pool
            .getBridge(root)
            .toolCall(`cold-${index}`, "read", { filePath: "seed.txt" });
          return { root, result };
        }),
      );
      const elapsed = Date.now() - started;

      for (const { result } of firstCalls) {
        assertToolSuccess(result, "cold first read");
        expect(JSON.stringify(result)).toContain("seed");
        expect(JSON.stringify(result)).not.toContain("route.bind");
        expect(JSON.stringify(result)).not.toContain("module_timeout");
      }
      expect(elapsed).toBeLessThanOrEqual(45_000);
      console.info(
        `[subc-lifecycle] cold-configure roots=${roots.length} first_calls_ms=${elapsed}`,
      );
    });
  }, 60_000);

  test("daemon restart on the same connection file recovers the existing pool", async () => {
    await withRig(async ({ rig, pools, nudges }) => {
      const pool = await createPool(prepared, rig, pools, nudges);
      const transport = pool.getBridge(rig.projectDir);
      const session = `restart-${Date.now()}`;
      const warm = await transport.toolCall(session, "read", { filePath: "seed.txt" });
      assertToolSuccess(warm, "warm pre-restart read");
      const nudgeStart = nudges.length;
      const oldPid = rig.daemonPid;

      await rig.restartDaemon();
      expect(rig.daemonPid).toBeDefined();
      expect(rig.daemonPid).not.toBe(oldPid);
      await rig.waitForAftCatalog(20_000);
      await waitFor(() => nudges.length > nudgeStart, 12_000, "bg subscription reconnect nudge");

      const postRestart = await transport.toolCall(session, "read", { filePath: "seed.txt" });
      assertToolSuccess(postRestart, "post-daemon-restart read");
      expect(JSON.stringify(postRestart)).toContain("seed");
      console.info(
        `[subc-lifecycle] daemon-restart old_pid=${oldPid ?? "n/a"} new_pid=${
          rig.daemonPid ?? "n/a"
        } classification=bg_subscription_socket_drop_transient_reconnected_auth_reread post_success=${postRestart.success}`,
      );
    });
  }, 60_000);
});

interface RigRun {
  rig: SubcRig;
  pools: AftTransportPool[];
  nudges: Array<{ root: string; session: string; at: number }>;
}

async function withRig(run: (ctx: RigRun) => Promise<void>): Promise<void> {
  prepared = await prepareSubcLane();
  const rig = await startSubcRig(prepared);
  const pools: AftTransportPool[] = [];
  const nudges: Array<{ root: string; session: string; at: number }> = [];
  try {
    await run({ rig, pools, nudges });
  } finally {
    await Promise.all(pools.splice(0).map((pool) => pool.shutdown().catch(() => undefined)));
    await rig.cleanup();
  }
}

async function createPool(
  preparedLane: PreparedSubcLane,
  rig: SubcRig,
  pools: AftTransportPool[],
  nudges: Array<{ root: string; session: string; at: number }>,
): Promise<AftTransportPool> {
  const pool = await createAftTransportPool({
    harness: "opencode",
    binaryPath: preparedLane.aftBinaryPath ?? "",
    poolOptions: { timeoutMs: 15_000 },
    configOverrides: {},
    subcConnectionFile: rig.connectionFile,
    subcConsumerIdentity: null,
    onBgEventsNudge: (root, session) => nudges.push({ root, session, at: Date.now() }),
  });
  pools.push(pool);
  return pool;
}

function assertToolSuccess(result: ToolCallResult, label: string): void {
  expect(result.success, `${label}: ${JSON.stringify(result)}`).toBe(true);
}

function taskIdFrom(result: ToolCallResult): string {
  const taskId = String((result as { task_id?: unknown }).task_id ?? "");
  expect(taskId.length).toBeGreaterThan(0);
  return taskId;
}

async function waitForCompletion(
  transport: AftProjectTransport,
  session: string,
  taskId: string,
  timeoutMs: number,
): Promise<Record<string, unknown>> {
  let lastDrain: Record<string, unknown> = {};
  await waitFor(
    async () => {
      lastDrain = await transport.send("bash_drain_completions", { session_id: session });
      return completionTaskIds(lastDrain).includes(taskId);
    },
    timeoutMs,
    `completion ${taskId}`,
  );
  return lastDrain;
}

function completionTaskIds(response: Record<string, unknown>): string[] {
  const completions = Array.isArray(response.bg_completions) ? response.bg_completions : [];
  return completions
    .map((completion) =>
      completion && typeof completion === "object"
        ? String((completion as { task_id?: unknown }).task_id ?? "")
        : "",
    )
    .filter((taskId) => taskId.length > 0);
}

async function waitFor(
  predicate: () => boolean | Promise<boolean>,
  timeoutMs: number,
  label: string,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${label}`);
}

function formatRestartCount(runtime: AftModuleRuntime): string {
  return runtime.restartCount === null ? "n/a" : String(runtime.restartCount);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
}
