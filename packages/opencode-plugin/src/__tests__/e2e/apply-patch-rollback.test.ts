/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { type AftTransportPool, BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { noopAsk, toolResultText } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type HarnessFactory,
  harnessPool,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

type AskInput = Record<string, unknown>;

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: AftTransportPool, storageDir: string): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir,
  };
}

function createSdkContext(directory: string, ask: ToolContext["ask"] = noopAsk): ToolContext {
  return {
    sessionID: `apply-patch-cutover-e2e-${Math.random()}`,
    messageID: "apply-patch-message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function recordingAsk(
  calls: AskInput[],
  onAsk?: (input: AskInput) => void | Promise<void>,
): ToolContext["ask"] {
  return (async (input: AskInput) => {
    calls.push(input);
    await onAsk?.(input);
  }) as unknown as ToolContext["ask"];
}

export function runApplyPatchRollbackSuite(
  options: { harnessFactory?: HarnessFactory; name?: string } = {},
): void {
  maybeDescribe(options.name ?? "e2e apply_patch server-side cutover", () => {
    let preparedBinary: PreparedBinary = initialBinary;
    const harnesses: E2EHarness[] = [];
    const pools: AftTransportPool[] = [];

    beforeAll(async () => {
      preparedBinary = await prepareBinary();
    });

    afterEach(async () => {
      await Promise.allSettled(pools.splice(0, pools.length).map((pool) => pool.shutdown()));
      await cleanupHarnesses(harnesses);
    });

    async function toolHarness(ask?: ToolContext["ask"]): Promise<{
      h: E2EHarness;
      tools: ReturnType<typeof hoistedTools>;
      sdkCtx: ToolContext;
    }> {
      const h = await (options.harnessFactory ?? createHarness)(preparedBinary, {
        fixtureNames: [],
        timeoutMs: 20_000,
      });
      harnesses.push(h);
      const actualStorageDir = join(h.tempDir, ".storage");
      const pool =
        h.transport === "subc"
          ? harnessPool(h)
          : new BridgePool(
              h.binaryPath,
              { timeoutMs: 20_000 },
              { storage_dir: actualStorageDir, harness: "opencode" },
            );
      if (h.transport !== "subc") pools.push(pool);
      const ctx = createPluginContext(pool, actualStorageDir);
      return {
        h,
        tools: hoistedTools(ctx),
        sdkCtx: createSdkContext(h.tempDir, ask),
      };
    }

    test("successful add+update patch changes disk and returns server summary", async () => {
      const { h, tools, sdkCtx } = await toolHarness();
      await writeFile(h.path("existing.txt"), "before\n", "utf8");

      const output = await tools.apply_patch.execute(
        {
          patchText: `*** Begin Patch
*** Add File: created.txt
+created
*** Update File: existing.txt
@@
-before
+after
*** End Patch`,
        },
        sdkCtx,
      );

      expect(toolResultText(output)).toContain("Created created.txt");
      expect(toolResultText(output)).toContain("Updated existing.txt");
      expect(await readTextFile(h.path("created.txt"))).toBe("created\n");
      expect(await readTextFile(h.path("existing.txt"))).toBe("after\n");
    });

    test("preview then denied edit permission leaves disk unchanged", async () => {
      const ask = (async (input: AskInput) => {
        if (input.permission === "edit") throw new Error("Denied by test");
      }) as unknown as ToolContext["ask"];
      const { h, tools, sdkCtx } = await toolHarness(ask);
      await writeFile(h.path("existing.txt"), "before\n", "utf8");

      const output = await tools.apply_patch.execute(
        {
          patchText: `*** Begin Patch
*** Add File: created.txt
+created
*** Update File: existing.txt
@@
-before
+after
*** End Patch`,
        },
        sdkCtx,
      );

      expect(toolResultText(output)).toContain("permission_denied");
      expect(await readTextFile(h.path("existing.txt"))).toBe("before\n");
      expect(existsSync(h.path("created.txt"))).toBe(false);
    });

    test("total failure after preview throws the server error", async () => {
      let mutatedAfterPreview = false;
      let targetPath = "";
      const ask = recordingAsk([], async (input) => {
        if (input.permission === "edit" && !mutatedAfterPreview) {
          mutatedAfterPreview = true;
          await writeFile(targetPath, "drift\n", "utf8");
        }
      });
      const { h, tools, sdkCtx } = await toolHarness(ask);
      targetPath = h.path("target.txt");
      await writeFile(h.path("target.txt"), "before\n", "utf8");

      await expect(
        tools.apply_patch.execute(
          {
            patchText: `*** Begin Patch
*** Update File: target.txt
@@
-before
+after
*** End Patch`,
          },
          sdkCtx,
        ),
      ).rejects.toThrow("Patch failed");
      expect(await readTextFile(h.path("target.txt"))).toBe("drift\n");
    });

    test("partial failure returns summary and keeps successful hunks", async () => {
      let projectRoot = "";
      const ask = recordingAsk([], async (input) => {
        if (input.permission === "edit") {
          await writeFile(join(projectRoot, "second.txt"), "race\n");
        }
      });
      const { h, tools, sdkCtx } = await toolHarness(ask);
      projectRoot = h.tempDir;

      const output = await tools.apply_patch.execute(
        {
          patchText: `*** Begin Patch
*** Add File: first.txt
+first
*** Add File: second.txt
+second
*** End Patch`,
        },
        sdkCtx,
      );

      const text = toolResultText(output);
      expect(text).toContain("Created first.txt");
      expect(text).toContain("Failed to create second.txt");
      expect(text).toContain("Patch partially applied");
      expect(await readTextFile(h.path("first.txt"))).toBe("first\n");
      expect(await readTextFile(h.path("second.txt"))).toBe("race\n");
    });

    test("external-directory and edit permissions are both requested", async () => {
      const asks: AskInput[] = [];
      const ask = recordingAsk(asks);
      const { h, tools, sdkCtx } = await toolHarness(ask);
      const externalDir = join(dirname(h.tempDir), `aft-apply-patch-external-${Date.now()}`);
      const externalFile = join(externalDir, "outside.txt");
      await mkdir(externalDir, { recursive: true });

      try {
        const output = await tools.apply_patch.execute(
          {
            patchText: `*** Begin Patch
*** Add File: ${externalFile}
+outside
*** End Patch`,
          },
          sdkCtx,
        );

        expect(toolResultText(output)).toContain(`Created ${externalFile}`);
        expect(await readFile(externalFile, "utf8")).toBe("outside\n");
        expect(asks.map((call) => call.permission)).toEqual(["external_directory", "edit"]);
      } finally {
        await rm(externalDir, { recursive: true, force: true });
      }
    });
  });
}

if (process.env.AFT_OPENCODE_E2E_IMPORT_ONLY !== "1") {
  runApplyPatchRollbackSuite();
}
