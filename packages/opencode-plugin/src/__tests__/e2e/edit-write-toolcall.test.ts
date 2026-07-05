/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { readFile, writeFile } from "node:fs/promises";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolResult } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { mockAskDeny, noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

type MutationToolResult = {
  output: string;
  title?: string;
  metadata?: {
    filediff?: {
      file: string;
      before: string;
      after: string;
      additions: number;
      deletions: number;
    };
  };
};

function createMockClient(): PluginContext["client"] {
  return { lsp: {}, find: {} } as PluginContext["client"];
}

function createToolContext(h: E2EHarness, ask: ToolContext["ask"] = noopAsk): ToolContext {
  return {
    messageID: "edit-write-toolcall-e2e",
    agent: "test",
    directory: h.tempDir,
    worktree: h.tempDir,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  } as ToolContext;
}

function createTools(h: E2EHarness): ReturnType<typeof hoistedTools> {
  const pool = { getBridge: () => h.bridge } as unknown as BridgePool;
  const ctx: PluginContext = {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir: h.path(".aft-test-storage"),
  };
  return hoistedTools(ctx);
}

function asMutationResult(result: ToolResult): MutationToolResult {
  if (typeof result === "string") {
    throw new Error(`expected object ToolResult, got string: ${result}`);
  }
  return result as MutationToolResult;
}

async function executeEdit(
  h: E2EHarness,
  args: Record<string, unknown>,
  ask?: ToolContext["ask"],
): Promise<MutationToolResult> {
  return asMutationResult(await createTools(h).edit.execute(args, createToolContext(h, ask)));
}

async function executeWrite(
  h: E2EHarness,
  args: Record<string, unknown>,
  ask?: ToolContext["ask"],
): Promise<MutationToolResult> {
  return asMutationResult(await createTools(h).write.execute(args, createToolContext(h, ask)));
}

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e hoisted edit/write tool_call cutover", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary);
    harnesses.push(created);
    return created;
  }

  test("edit append mutates the file and returns server text", async () => {
    const h = await harness();
    await writeFile(h.path("append.txt"), "alpha\n");

    const result = await executeEdit(h, { filePath: "append.txt", appendContent: "beta\n" });

    expect(await readTextFile(h.path("append.txt"))).toBe("alpha\nbeta\n");
    expect(result.output).toBe("Edited (+1/-0).");
  });

  test("edit oldString/newString mutates the file, returns text, and preserves UI filediff", async () => {
    const h = await harness();
    await writeFile(h.path("replace.ts"), "export const value = 1;\n");

    const result = await executeEdit(h, {
      filePath: "replace.ts",
      oldString: "value = 1",
      newString: "value = 2",
    });

    expect(await readTextFile(h.path("replace.ts"))).toBe("export const value = 2;\n");
    expect(result.output).toBe("Edited (+1/-1).");
    expect(result.metadata?.filediff?.file.endsWith("/replace.ts")).toBe(true);
    expect(result.metadata?.filediff).toMatchObject({
      before: "export const value = 1;\n",
      after: "export const value = 2;\n",
      additions: 1,
      deletions: 1,
    });
  });

  test("edit on a >512KB file falls back to the preview hunk diff (no blank filediff)", async () => {
    const h = await harness();
    // Over Rust's 512KB diff-content cap: the response diff is counts-only
    // (`truncated: true`, no before/after). Fabricating empty contents used to
    // render a blank diff in the UI.
    const big = `${"a".repeat(600_000)}\nbeta\n`;
    await writeFile(h.path("big.txt"), big);

    const result = await executeEdit(h, {
      filePath: "big.txt",
      oldString: "beta",
      newString: "BETA",
    });

    expect(await readTextFile(h.path("big.txt"))).toContain("BETA");
    expect(result.output).toBe("Edited (+1/-1).");
    // No fabricated empty-content filediff...
    expect(result.metadata?.filediff).toBeUndefined();
    // ...and the rendered diff carries the real hunk from the preview.
    const diffText = result.metadata?.diff as string | undefined;
    expect(diffText).toContain("-beta");
    expect(diffText).toContain("+BETA");
  });

  test("edit symbol+content mutates the symbol and returns server text", async () => {
    const h = await harness();
    await writeFile(
      h.path("symbol.ts"),
      "export function greet(name: string): string {\n  return `Hi, ${name}`;\n}\n",
    );

    const result = await executeEdit(h, {
      filePath: "symbol.ts",
      symbol: "greet",
      content: "export function greet(name: string): string {\n  return `Hello, ${name}`;\n}\n",
    });

    expect(await readTextFile(h.path("symbol.ts"))).toContain("Hello");
    expect(result.output).toBe("Edited (+2/-1).");
  });

  test("edit edits[] batch mutates all edits and returns server text", async () => {
    const h = await harness();
    await writeFile(h.path("batch.txt"), "one\ntwo\nthree\n");

    const result = await executeEdit(h, {
      filePath: "batch.txt",
      edits: [
        { oldString: "one", newString: "ONE" },
        { startLine: 3, endLine: 3, content: "THREE" },
      ],
    });

    expect(await readTextFile(h.path("batch.txt"))).toBe("ONE\ntwo\nTHREE\n");
    expect(result.output).toBe("Edited (+2/-2, 2 edits).");
  });

  test("write creates and overwrites files through tool_call", async () => {
    const h = await harness();

    const create = await executeWrite(h, {
      filePath: "created.ts",
      content: "export const created = true;\n",
    });
    expect(await readTextFile(h.path("created.ts"))).toBe("export const created = true;\n");
    expect(create.output).toBe("Created new file.");

    await writeFile(h.path("overwrite.ts"), "export const value = 1;\n");
    const overwrite = await executeWrite(h, {
      filePath: "overwrite.ts",
      content: "export const value = 2;\n",
    });
    expect(await readTextFile(h.path("overwrite.ts"))).toBe("export const value = 2;\n");
    expect(overwrite.output).toBe("File updated.");
  });

  test("denied preview approval returns permission_denied and leaves the file unchanged", async () => {
    const h = await harness();
    await writeFile(h.path("denied.ts"), "export const value = 1;\n");

    const result = await createTools(h).edit.execute(
      { filePath: "denied.ts", oldString: "1", newString: "2" },
      createToolContext(h, mockAskDeny("Denied by test.")),
    );

    expect(JSON.parse(result as string)).toMatchObject({
      success: false,
      code: "permission_denied",
    });
    expect(await readTextFile(h.path("denied.ts"))).toBe("export const value = 1;\n");
  });

  test("edit oldString not found throws before mutating", async () => {
    const h = await harness();
    await writeFile(h.path("missing-match.ts"), "export const value = 1;\n");

    await expect(
      executeEdit(h, {
        filePath: "missing-match.ts",
        oldString: "does not exist",
        newString: "replacement",
      }),
    ).rejects.toThrow(/not found|match/i);
    expect(await readFile(h.path("missing-match.ts"), "utf8")).toBe("export const value = 1;\n");
  });
});
