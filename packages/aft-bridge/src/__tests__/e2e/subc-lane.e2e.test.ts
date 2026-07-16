/// <reference path="../../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { AftProjectTransport, AftTransportPool, ToolCallResult } from "../../transport.js";
import { createAftTransportPool } from "../../transport-factory.js";
import { type PreparedSubcLane, prepareSubcLane, type SubcRig, startSubcRig } from "./subc-rig.js";

const initialPrepared = await prepareSubcLane();
const maybeDescribe = initialPrepared.skipReason ? describe.skip : describe;
const describeName = initialPrepared.skipReason
  ? `subc transport e2e lane (skipped: ${initialPrepared.skipReason})`
  : "subc transport e2e lane";

maybeDescribe(describeName, () => {
  let prepared: PreparedSubcLane = initialPrepared;
  let rig: SubcRig;
  const pools: AftTransportPool[] = [];
  const nudges: Array<{ root: string; session: string; at: number }> = [];

  beforeAll(async () => {
    prepared = await prepareSubcLane();
    rig = await startSubcRig(prepared);
  }, 30_000);

  afterAll(async () => {
    await Promise.all(pools.splice(0).map((pool) => pool.shutdown().catch(() => undefined)));
    await rig?.cleanup();
  });

  async function bridge(): Promise<AftProjectTransport> {
    const pool = await createAftTransportPool({
      harness: "opencode",
      binaryPath: prepared.aftBinaryPath ?? "",
      poolOptions: { timeoutMs: 15_000 },
      configOverrides: {},
      subcConnectionFile: rig.connectionFile,
      subcConsumerIdentity: null,
      onBgEventsNudge: (root, session) => nudges.push({ root, session, at: Date.now() }),
    });
    pools.push(pool);
    return pool.getBridge(rig.projectDir);
  }

  test("preview edit is dry-run, apply mutates, and stale re-apply returns success:false", async () => {
    const transport = await bridge();
    const file = join(rig.projectDir, "preview-edit.txt");
    await writeFile(file, "alpha\nbeta\n", "utf8");

    const preview = await transport.toolCall(
      "preview-edit",
      "edit",
      { filePath: "preview-edit.txt", oldString: "beta", newString: "gamma" },
      { preview: true },
    );
    expect(preview.success).toBe(true);
    expect(await readFile(file, "utf8")).toBe("alpha\nbeta\n");

    const apply = await transport.toolCall("preview-edit", "edit", {
      filePath: "preview-edit.txt",
      oldString: "beta",
      newString: "gamma",
    });
    expect(apply.success).toBe(true);
    expect(await readFile(file, "utf8")).toBe("alpha\ngamma\n");

    const stale = await transport.toolCall("preview-edit", "edit", {
      filePath: "preview-edit.txt",
      oldString: "beta",
      newString: "gamma",
    });
    expect(stale.success).toBe(false);
  }, 20_000);

  test("preview write and preview apply_patch leave disk unchanged", async () => {
    const transport = await bridge();
    const writeTarget = join(rig.projectDir, "preview-write.txt");

    const previewWrite = await transport.toolCall(
      "preview-write",
      "write",
      { filePath: "preview-write.txt", content: "created\n" },
      { preview: true },
    );
    expect(previewWrite.success).toBe(true);
    expect(existsSync(writeTarget)).toBe(false);

    const patchTarget = join(rig.projectDir, "preview-patch.txt");
    await writeFile(patchTarget, "before\n", "utf8");
    const patchText = `*** Begin Patch
*** Update File: preview-patch.txt
@@
-before
+after
*** End Patch`;

    const previewPatch = await transport.toolCall(
      "preview-patch",
      "apply_patch",
      { patchText },
      { preview: true },
    );
    expect(previewPatch.success).toBe(true);
    expect(await readFile(patchTarget, "utf8")).toBe("before\n");

    const applyPatch = await transport.toolCall("preview-patch", "apply_patch", { patchText });
    expect(applyPatch.success).toBe(true);
    expect(await readFile(patchTarget, "utf8")).toBe("after\n");
  }, 20_000);

  test("long wait:true bash honors transportTimeoutMs beyond the subc default deadline", async () => {
    const transport = await bridge();
    const started = Date.now();
    const result = await transport.toolCall(
      "long-bash",
      "bash",
      { command: "sleep 34; echo long-bash-ok", wait: true, timeout: 120_000 },
      { transportTimeoutMs: 130_000 },
    );
    const elapsed = Date.now() - started;

    expect(result.success).toBe(true);
    expect(JSON.stringify(result)).toContain("long-bash-ok");
    expect(elapsed).toBeGreaterThanOrEqual(33_000);
  }, 60_000);

  test("bash permission_required loop can be regranted with permissions_granted", async () => {
    const transport = await bridge();
    const first = await transport.toolCall("permission-loop", "bash", {
      command: "echo permission-ok",
      permissions_requested: true,
    });
    expect(first.success).toBe(false);
    expect(first.code).toBe("permission_required");
    const grants = permissionPatterns(first);
    expect(grants.length).toBeGreaterThan(0);

    const second = await transport.toolCall("permission-loop", "bash", {
      command: "echo permission-ok",
      permissions_requested: true,
      permissions_granted: grants,
    });
    expect(second.success).toBe(true);
    expect(JSON.stringify(second)).toContain("permission-ok");
  }, 20_000);

  test("background completions nudge, drain, ack, then stay quiet", async () => {
    const transport = await bridge();
    const session = `bg-${Date.now()}`;
    const before = nudges.length;
    const spawned = await transport.toolCall(session, "bash", {
      command: "sleep 1; echo bg-complete-ok",
      background: true,
    });
    expect(spawned.success).toBe(true);
    const taskId = String((spawned as { task_id?: unknown }).task_id ?? "");
    expect(taskId.length).toBeGreaterThan(0);

    await waitFor(() => nudges.length > before, 12_000, "bg_events nudge");
    const drain = await waitForCompletion(transport, session, taskId, 8_000);
    await transport.send("bash_ack_completions", { session_id: session, task_ids: [taskId] });
    expect(JSON.stringify(drain)).toContain("bg-complete-ok");

    const quietStart = nudges.length;
    await sleep(3_000);
    const postAckNudges = nudges.slice(quietStart).filter((nudge) => nudge.session === session);
    expect(postAckNudges.length).toBeLessThanOrEqual(1);
    const afterAckDrain = await transport.send("bash_drain_completions", { session_id: session });
    expect(completionTaskIds(afterAckDrain)).not.toContain(taskId);
    // Internal waits (nudge 12s + drain 8s + quiet 3s) exceed bun's 5s default
    // test timeout under CI load; budget for the worst case.
  }, 30_000);

  test("two sessions on one root do not leak background completions across drains", async () => {
    const transport = await bridge();
    const sessionA = `session-a-${Date.now()}`;
    const sessionB = `session-b-${Date.now()}`;
    const spawned = await transport.toolCall(sessionB, "bash", {
      command: "sleep 1; echo only-session-b",
      background: true,
    });
    const taskId = String((spawned as { task_id?: unknown }).task_id ?? "");
    expect(taskId.length).toBeGreaterThan(0);
    await sleep(1_750);

    const drainA = await transport.send("bash_drain_completions", { session_id: sessionA });
    expect(completionTaskIds(drainA)).not.toContain(taskId);

    const drainB = await waitForCompletion(transport, sessionB, taskId, 8_000);
    expect(completionTaskIds(drainB)).toContain(taskId);
    await transport.send("bash_ack_completions", { session_id: sessionB, task_ids: [taskId] });
  }, 20_000);
});

function permissionPatterns(result: ToolCallResult): string[] {
  const asks = Array.isArray(result.asks) ? result.asks : [];
  return asks.flatMap((ask) => {
    if (!ask || typeof ask !== "object") return [];
    const patterns = (ask as { patterns?: unknown }).patterns;
    return Array.isArray(patterns)
      ? patterns.filter((p): p is string => typeof p === "string")
      : [];
  });
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

function sleep(ms: number): Promise<void> {
  return new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
}
