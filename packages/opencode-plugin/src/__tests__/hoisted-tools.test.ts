/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { aftPrefixedTools, hoistedTools } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

const PROJECT_CWD = resolve(import.meta.dir, "../../../..");
let sdkCtx = createMockSdkContext(PROJECT_CWD);
let tmpDir: string | null = null;

/**
 * read/write/edit/apply_patch now return `{ output, title, metadata }` so UI
 * metadata (title + diff) rides on the result instead of a side-channel store.
 * Most assertions only care about the agent-visible text — unwrap it here
 * (tolerating the legacy bare-string shape).
 */
function text(r: unknown): string {
  return typeof r === "string" ? r : ((r as { output?: string })?.output ?? "");
}

async function makeTempDir(): Promise<string> {
  return await realpath(await mkdtemp(resolve(tmpdir(), "aft-hoisted-")));
}

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };

/** Creates a mock client that returns no connected LSP servers. */
function createMockClient(): any {
  return {
    lsp: {
      status: async () => ({ data: [] }),
    },
    find: {
      symbols: async () => ({ data: [] }),
    },
  };
}

/** Helper to create a PluginContext with a pool and a mock client. */
function createPluginContext(
  pool: BridgePool,
  config: PluginContext["config"] = {} as PluginContext["config"],
): PluginContext {
  return { pool, client: createMockClient(), config, storageDir: "/tmp/aft-test" };
}

/** Mock SDK ToolContext for test execute calls. */
function createMockSdkContext(directory: string): ToolContext {
  return {
    sessionID: "test",
    messageID: "test",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

function recordingAsk(calls: Array<Record<string, unknown>>): ToolContext["ask"] {
  return (async (input: Record<string, unknown>) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}

function previewResponse(
  diff = "Index: preview.ts\n--- preview.ts\n+++ preview.ts\n",
): BridgeResponse {
  return { success: true, preview: true, preview_diff: diff };
}

function createMockHoistedHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
  config: PluginContext["config"] = {} as PluginContext["config"],
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push({ command, params });
      return await sendImpl(command, params);
    },
  };

  const pool = {
    getBridge: () => bridge,
  } as unknown as BridgePool;

  return {
    calls,
    tools: hoistedTools(createPluginContext(pool, config)),
  };
}

afterEach(async () => {
  if (tmpDir) {
    await rm(tmpDir, { recursive: true, force: true });
    tmpDir = null;
  }
  sdkCtx = createMockSdkContext(PROJECT_CWD);
});

describe("Hoisted tool execute handlers", () => {
  test("read throws the Rust error response instead of accessing missing content", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return { success: false, message: "File not found: missing.ts" };
    });

    await expect(tools.read.execute({ filePath: "missing.ts" }, sdkCtx)).rejects.toThrow(
      "File not found: missing.ts",
    );
  });

  test("write throws the Rust error response for invalid writes", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("write");
      return { success: false, message: "Refusing to write outside project root" };
    });

    await expect(
      tools.write.execute({ filePath: "../outside.ts", content: "export const x = 1;\n" }, sdkCtx),
    ).rejects.toThrow("Refusing to write outside project root");
  });

  test("write defaults diagnostics off and omits LSP payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({ success: true }));

    const result = text(
      await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx),
    );

    expect(result).toBe("File updated.");
    expect(result).not.toContain("lsp_diagnostics");
    expect(result).not.toContain("LSP errors detected");
    expect(calls).toHaveLength(1);
    expect(calls[0]).toEqual({
      command: "write",
      params: {
        file: resolve(tmpDir, "src/app.ts"),
        content: "export {};\n",
        create_dirs: true,
        diagnostics: false,
        include_diff_content: true,
        session_id: "test",
      },
    });
  });

  test("write follows lsp.diagnostics_on_edit (config-driven; no per-call param)", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({ success: true }), {
      lsp: { diagnostics_on_edit: true },
    } as PluginContext["config"]);

    await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx);
    expect(calls[0].params.diagnostics).toBe(true);

    // The per-call `diagnostics` param was removed (agents never used it; the
    // status bar + aft_inspect are the agent-facing diagnostics paths). A
    // stray param must NOT override the configured default.
    await tools.write.execute(
      { filePath: "src/app.ts", content: "export {};\n", diagnostics: false },
      sdkCtx,
    );
    expect(calls[1].params.diagnostics).toBe(true);
  });

  test("write surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(
      async () => ({
        success: true,
        lsp_diagnostics: [{ severity: "error", line: 7, message: "Bad type" }],
      }),
      { lsp: { diagnostics_on_edit: true } } as PluginContext["config"],
    );

    const result = text(
      await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx),
    );

    expect(calls[0].params.diagnostics).toBe(true);
    expect(result).toContain("LSP errors detected");
    expect(result).toContain("Line 7: Bad type");
  });

  test("edit defaults diagnostics off and omits LSP payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      replacements: 1,
    }));

    const result = text(
      await tools.edit.execute(
        { filePath: "src/app.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    );

    // Agent-facing result is the compact summary sentence, not raw JSON.
    expect(result).toBe("Edited (+0/-0).");
    expect(result).not.toContain("lsp_diagnostics");
    expect(result).not.toContain("LSP errors detected");
    expect(result).not.toContain("backup_id");
    const previewCall = calls.find((call) => call.params.preview === true);
    const realCall = calls.find((call) => call.params.preview !== true);
    expect(previewCall?.command).toBe("edit_match");
    expect(previewCall?.params.diagnostics).toBeUndefined();
    expect(realCall?.command).toBe("edit_match");
    expect(realCall?.params.diagnostics).toBe(false);
  });

  test("edit surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const diagnostics = [{ severity: "error", line: 3, message: "Missing import" }];
    const { calls, tools } = createMockHoistedHarness(
      async () => ({
        success: true,
        replacements: 1,
        lsp_diagnostics: diagnostics,
      }),
      { lsp: { diagnostics_on_edit: true } } as PluginContext["config"],
    );

    const result = text(
      await tools.edit.execute(
        { filePath: "src/app.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    );

    // Headline is the compact summary; LSP errors are appended below it.
    expect(result.split("\n\n")[0]).toBe("Edited (+0/-0).");
    const realCall = calls.find((call) => call.params.preview !== true);
    expect(realCall?.params.diagnostics).toBe(true);
    expect(result).toContain("Line 3: Missing import");
  });

  test("apply_patch defaults diagnostics off and omits LSP payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    await writeFile(resolve(tmpDir, "file.ts"), "old\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: file.ts",
      "@@",
      "-old",
      "+new",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: true };
      throw new Error(`Unexpected command: ${command}`);
    });

    const result = text(await tools.apply_patch.execute({ patchText }, sdkCtx));

    const writeCall = calls.find((call) => call.command === "write");
    expect(writeCall?.params.diagnostics).toBe(false);
    expect(result).toContain("Updated file.ts");
    expect(result).not.toContain("lsp_diagnostics");
    expect(result).not.toContain("LSP errors detected");
  });

  test("apply_patch surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);
    await writeFile(resolve(tmpDir, "file.ts"), "old\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: file.ts",
      "@@",
      "-old",
      "+new",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(
      async (command) => {
        if (command === "apply_patch") return previewResponse();
        if (command === "checkpoint") return { success: true };
        if (command === "write") {
          return {
            success: true,
            lsp_diagnostics: [{ severity: "error", line: 9, message: "Patch type error" }],
          };
        }
        throw new Error(`Unexpected command: ${command}`);
      },
      { lsp: { diagnostics_on_edit: true } } as PluginContext["config"],
    );

    const result = text(await tools.apply_patch.execute({ patchText }, sdkCtx));

    const writeCall = calls.find((call) => call.command === "write");
    expect(writeCall?.params.diagnostics).toBe(true);
    expect(result).toContain("LSP errors detected in file.ts");
    expect(result).toContain("Line 9: Patch type error");
  });

  test("mutation tool schemas expose no per-call diagnostics param", () => {
    const { tools } = createMockHoistedHarness(async () => ({ success: true }));
    const pool = {
      getBridge: () => ({ send: async () => ({ success: true }) }),
    } as unknown as BridgePool;
    const prefixedTools = aftPrefixedTools(createPluginContext(pool));

    // Removed deliberately: agents never used it; diagnostics are the status
    // bar (passive) + aft_inspect (pull) + the lsp.diagnostics_on_edit config.
    for (const toolDef of [
      tools.write,
      tools.edit,
      tools.apply_patch,
      prefixedTools.aft_write,
      prefixedTools.aft_edit,
      prefixedTools.aft_apply_patch,
    ]) {
      expect(toolDef.args.diagnostics).toBeUndefined();
      expect(toolDef.args.preview).toBeUndefined();
      expect(toolDef.args.dryRun).toBeUndefined();
    }
  });

  test("write approval ask includes unified diff metadata for existing changes", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    await writeFile(resolve(tmpDir, "src.ts"), "export const value = 1;\n");

    const { tools } = createMockHoistedHarness(async () => ({ success: true, created: false }));

    await tools.write.execute({ filePath: "src.ts", content: "export const value = 2;\n" }, sdkCtx);

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "src.ts"));
    expect(editAsk?.metadata?.diff).toBeTypeOf("string");
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toContain("--- ");
    expect(diff).toContain("-export const value = 1;");
    expect(diff).toContain("+export const value = 2;");
  });

  test("write approval ask includes all-additions diff metadata for a new file", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const { tools } = createMockHoistedHarness(async () => ({ success: true, created: true }));

    await tools.write.execute(
      { filePath: "new.ts", content: "export const fresh = true;\n" },
      sdkCtx,
    );

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "new.ts"));
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toBeTypeOf("string");
    expect(diff).toContain("+export const fresh = true;");
    expect(diff).not.toContain("-export const fresh = true;");
  });

  test("edit oldString approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const { tools } = createMockHoistedHarness(async (_command, params) => {
      if (params.preview === true) {
        return {
          success: true,
          diff: { before: "const value = 1;\n", after: "const value = 2;\n" },
        };
      }
      return { success: true, replacements: 1 };
    });

    await tools.edit.execute({ filePath: "src.ts", oldString: "1", newString: "2" }, sdkCtx);

    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toContain("-const value = 1;");
    expect(diff).toContain("+const value = 2;");
  });

  test("edit symbol approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const previewDiff =
      "Index: symbol.ts\n--- symbol.ts\n+++ symbol.ts\n@@ -1,1 +1,1 @@\n-export function oldName() {}\n+export function newName() {}\n";

    const { calls, tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("edit_symbol");
      if (params.preview === true) return previewResponse(previewDiff);
      return {
        success: true,
        symbol: "oldName",
        operation: "replace",
        diff: { additions: 1, deletions: 1 },
      };
    });

    await tools.edit.execute(
      { filePath: "symbol.ts", symbol: "oldName", content: "export function newName() {}\n" },
      sdkCtx,
    );

    expect(calls[0]?.params.preview).toBe(true);
    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("edit batch approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const previewDiff =
      "Index: batch.ts\n--- batch.ts\n+++ batch.ts\n@@ -1,2 +1,2 @@\n-before\n+after\n";

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("batch");
      if (params.preview === true) return previewResponse(previewDiff);
      return { success: true, edits_applied: 1 };
    });

    await tools.edit.execute(
      { filePath: "batch.ts", edits: [{ oldString: "before", newString: "after" }] },
      sdkCtx,
    );

    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("edit preview errors surface before asking for approval", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const { calls, tools } = createMockHoistedHarness(async (_command, params) => {
      if (params.preview === true) return { success: false, message: "match_not_found" };
      throw new Error("real edit should not run after preview failure");
    });

    await expect(
      tools.edit.execute(
        { filePath: "missing.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    ).rejects.toThrow("match_not_found");
    expect(askCalls).toHaveLength(0);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.params.preview).toBe(true);
  });

  test("apply_patch approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    await writeFile(resolve(tmpDir, "file.ts"), "old\n");
    const previewDiff = "Index: file.ts\n--- file.ts\n+++ file.ts\n@@ -1,1 +1,1 @@\n-old\n+new\n";
    const patchText = [
      "*** Begin Patch",
      "*** Update File: file.ts",
      "@@",
      "-old",
      "+new",
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch") return previewResponse(previewDiff);
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: true };
      throw new Error(`Unexpected command: ${command}`);
    });

    await tools.apply_patch.execute({ patchText }, sdkCtx);

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "file.ts"));
    expect(editAsk?.metadata?.diff).toBe(previewDiff);
  });

  test("apply_patch preview errors surface before asking for approval", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    await writeFile(resolve(tmpDir, "file.ts"), "actual\n");
    const patchText = [
      "*** Begin Patch",
      "*** Update File: file.ts",
      "@@",
      "-expected",
      "+new",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch")
        return { success: false, message: "Failed to find expected lines" };
      throw new Error("real patch should not run after preview failure");
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Failed to find expected lines",
    );
    expect(askCalls).toHaveLength(0);
    expect(calls).toHaveLength(1);
    expect(calls[0]?.command).toBe("apply_patch");
    expect(calls[0]?.params.preview).toBe(true);
  });

  test("edit throws the Rust error response for failed replacements", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("edit_match");
      return { success: false, message: "Match not found in file" };
    });

    await expect(
      tools.edit.execute(
        { filePath: "target.ts", oldString: "before", newString: "after" },
        sdkCtx,
      ),
    ).rejects.toThrow("Match not found in file");
  });

  // Regression: Rust reverts a write that fails syntax validation and returns
  // success:true with rolled_back:true. Reporting "File updated." then would be
  // a lie — the file is unchanged. The agent must be told it rolled back.
  test("write reports a rolled-back write honestly, not as success", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("write");
      return { success: true, rolled_back: true, created: false };
    });

    const result = text(
      await tools.write.execute({ filePath: "target.ts", content: "const = ;\n" }, sdkCtx),
    );
    expect(result.toLowerCase()).toContain("rolled back");
    expect(result).not.toContain("File updated");
  });

  // Regression: hoisted apply_patch wraps each hunk's bridge call in try/catch,
  // but callBridge returns `success: false` responses WITHOUT throwing — so the
  // catch never ran, the hunk was falsely recorded as Created, and the error
  // was silently lost. The add/delete/update(+move) branches now convert a
  // `success: false` response into a throw so it reaches the failure path. For
  // a move hunk this is critical: a failed destination write must NOT proceed
  // to delete the source.
  test("apply_patch throws the Rust error response when a patch write fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const patchText = [
      "*** Begin Patch",
      "*** Add File: broken.ts",
      "+export const broken = true;",
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: false, message: "Disk full while writing patch" };
      throw new Error(`Unexpected command: ${command}`);
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Disk full while writing patch",
    );
  });

  // BLOCKER regression: a move hunk whose destination write fails must NOT
  // delete the source. Before the fix, the destination `write` returning
  // `{ success: false }` (data, not a throw) was treated as success and the
  // code proceeded to delete the source — losing the file entirely.
  test("apply_patch move hunk does not delete source when destination write fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const sourceFile = resolve(tmpDir, "from.ts");
    await writeFile(sourceFile, "export const moved = 1;\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: from.ts",
      "*** Move to: to.ts",
      "@@",
      "-export const moved = 1;",
      "+export const moved = 2;",
      "*** End Patch",
    ].join("\n");

    let deleteFileCalled = false;
    const { tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: false, message: "Disk full writing destination" };
      if (command === "delete_file") {
        deleteFileCalled = true;
        return { success: true };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Disk full writing destination",
    );
    // Source must be untouched: never deleted, content intact.
    expect(deleteFileCalled).toBe(false);
    expect(existsSync(sourceFile)).toBe(true);
    expect(await readFile(sourceFile, "utf-8")).toBe("export const moved = 1;\n");
  });

  test("delete throws when every file in the batch fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("delete_file");
      const files = (params.files as string[]) ?? [];
      return {
        success: true,
        complete: false,
        deleted: [],
        skipped_files: files.map((file) => ({ file, reason: "Cannot delete protected file" })),
      };
    });

    await expect(
      tools.aft_delete.execute({ files: ["locked.ts", "also-locked.ts"] }, sdkCtx),
    ).rejects.toThrow("Cannot delete protected file");
  });

  test("delete throws the Rust error response before synthesizing success", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("delete_file");
      return { success: false, message: "bridge delete refused" };
    });

    await expect(tools.aft_delete.execute({ files: ["doomed.ts"] }, sdkCtx)).rejects.toThrow(
      "bridge delete refused",
    );
  });

  test("delete returns partial-success payload when some files fail", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("delete_file");
      const files = (params.files as string[]) ?? [];
      const deleted: Array<{ file: string; backup_id: string | null }> = [];
      const skipped: Array<{ file: string; reason: string }> = [];
      for (const file of files) {
        if (file.includes("blocked.ts")) {
          skipped.push({ file, reason: "permission denied" });
        } else {
          deleted.push({ file, backup_id: null });
        }
      }
      return {
        success: true,
        complete: skipped.length === 0,
        deleted,
        skipped_files: skipped,
      };
    });

    const raw = await tools.aft_delete.execute({ files: ["a.ts", "blocked.ts", "c.ts"] }, sdkCtx);
    const parsed = JSON.parse(raw);
    expect(parsed.success).toBe(true);
    expect(parsed.complete).toBe(false);
    expect(parsed.deleted).toHaveLength(2);
    expect(parsed.deleted[0]).toContain("a.ts");
    expect(parsed.deleted[1]).toContain("c.ts");
    expect(parsed.skipped_files).toEqual([
      expect.objectContaining({ reason: "permission denied" }),
    ]);
    expect(parsed.skipped_files[0].file).toContain("blocked.ts");
  });

  test("delete reports complete=true when every file succeeds", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("delete_file");
      const files = (params.files as string[]) ?? [];
      return {
        success: true,
        complete: true,
        deleted: files.map((file) => ({ file, backup_id: null })),
        skipped_files: [],
      };
    });

    const raw = await tools.aft_delete.execute({ files: ["a.ts", "b.ts"] }, sdkCtx);
    const parsed = JSON.parse(raw);
    expect(parsed).toEqual({
      success: true,
      complete: true,
      deleted: expect.any(Array),
      skipped_files: [],
    });
    expect(parsed.deleted).toHaveLength(2);
  });

  test("move throws the Rust error response when rename fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("move_file");
      return { success: false, message: "Destination already exists" };
    });

    await expect(
      tools.aft_move.execute({ filePath: "source.ts", destination: "dest.ts" }, sdkCtx),
    ).rejects.toThrow("Destination already exists");
  });

  test("edit batch mode translates oldString/newString fields for the Rust bridge", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      edits_applied: 2,
    }));

    const result = text(
      await tools.edit.execute(
        {
          filePath: "batch.ts",
          edits: [
            { oldString: "before", newString: "after" },
            { startLine: 4, endLine: 6, content: "replacement" },
          ],
        },
        sdkCtx,
      ),
    );

    // 2 edits applied, no diff in the mock -> counts default to 0.
    expect(result).toBe("Edited (+0/-0, 2 edits).");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({
      command: "batch",
      params: {
        file: resolve(tmpDir, "batch.ts"),
        preview: true,
        include_diff_content: true,
        session_id: "test",
      },
    });
    expect(calls[1]).toEqual({
      command: "batch",
      params: {
        file: resolve(tmpDir, "batch.ts"),
        edits: [
          { match: "before", replacement: "after" },
          { line_start: 4, line_end: 6, content: "replacement" },
        ],
        diagnostics: false,
        include_diff_content: true,
        session_id: "test",
      },
    });
  });

  test('legacy aft_edit mode:"write" throws the Rust error response', async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const pool = {
      getBridge: () => ({
        send: async (command: string, params: Record<string, unknown>) => {
          expect(command).toBe("write");
          expect(params.diagnostics).toBe(false);
          return { success: false, message: "legacy write refused" };
        },
      }),
    } as unknown as BridgePool;
    const tools = aftPrefixedTools(createPluginContext(pool));

    await expect(
      tools.aft_edit.execute(
        { mode: "write", file: "legacy.ts", content: "export const x = 1;\n" },
        sdkCtx,
      ),
    ).rejects.toThrow("legacy write refused");
  });

  test("edit forwards replaceAll to Rust for multiple occurrences", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      replacements: 3,
    }));

    const result = text(
      await tools.edit.execute(
        {
          filePath: "repeated.ts",
          oldString: "oldName",
          newString: "newName",
          replaceAll: true,
        },
        sdkCtx,
      ),
    );

    // replaceAll with 3 replacements -> count surfaced; no diff -> +0/-0.
    expect(result).toBe("Edited (+0/-0, 3 replacements).");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({
      command: "edit_match",
      params: {
        file: resolve(tmpDir, "repeated.ts"),
        match: "oldName",
        replacement: "newName",
        replace_all: true,
        preview: true,
        include_diff_content: true,
        session_id: "test",
      },
    });
    expect(calls[1]).toEqual({
      command: "edit_match",
      params: {
        file: resolve(tmpDir, "repeated.ts"),
        match: "oldName",
        replacement: "newName",
        replace_all: true,
        diagnostics: false,
        include_diff_content: true,
        session_id: "test",
      },
    });
  });

  /// Diff-payload contract: the plugin requests full before/after from Rust
  /// (include_diff_content) for UI metadata, but the AGENT-facing result must
  /// strip the file content down to counts only. Echoing before/after into the
  /// model context makes the payload scale with file size, not edit size.
  test("edit agent result strips diff before/after to counts-only", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const bigBefore = `${"x".repeat(50_000)}\n`;
    const bigAfter = `${"y".repeat(50_000)}\n`;
    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      replacements: 1,
      diff: { before: bigBefore, after: bigAfter, additions: 1, deletions: 1 },
    }));

    const result = text(
      await tools.edit.execute({ filePath: "big.ts", oldString: "x", newString: "y" }, sdkCtx),
    );

    // Agent result must NOT contain the 50KB file content from either side.
    expect(result).not.toContain(bigBefore);
    expect(result).not.toContain(bigAfter);
    expect(result.length).toBeLessThan(2_000);

    // Counts survive for the agent's verification signal, in the compact
    // summary sentence (no raw JSON, no before/after content).
    expect(result).toBe("Edited (+1/-1).");
  });

  /// BUG-6a (per-file commit): when a 2-hunk patch has 1 success and 1
  /// failure, the successful hunk MUST stay applied. Pre-fix, AFT rolled
  /// the whole patch back via checkpoint restore + newly-created cleanup,
  /// throwing away the agent's correct work and forcing them to manually
  /// split patches. New behavior: each hunk commits independently.
  test("apply_patch keeps successful hunks when a later hunk fails (per-file commit)", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const createdFile = resolve(tmpDir, "created.ts");
    const failedFile = resolve(tmpDir, "broken.ts");
    const patchText = [
      "*** Begin Patch",
      "*** Add File: created.ts",
      "+export const created = true;",
      "*** Add File: broken.ts",
      "+export const broken = true;",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };

      if (command === "write") {
        const file = params.file as string;
        if (file === createdFile) {
          await writeFile(file, params.content as string);
          return { success: true };
        }

        if (file === failedFile) {
          throw new Error("Simulated patch failure");
        }
      }

      if (command === "delete_file") {
        // Cleanup of the failed-add partial. We don't expect any other
        // delete_file calls — successful hunks must NOT be deleted.
        await rm(params.file as string, { force: true });
        return { success: true };
      }

      throw new Error(`Unexpected command: ${command}`);
    });

    const result = text(await tools.apply_patch.execute({ patchText }, sdkCtx));

    expect(result).toContain("Created created.ts");
    expect(result).toContain("Failed to create broken.ts: Simulated patch failure");
    // New: explicit partial-success summary.
    expect(result).toContain("Patch partially applied");
    expect(result).toContain("1 of 2 hunk(s) succeeded");
    expect(result).toContain("Failed: broken.ts");
    expect(result).toContain("aft_safety");

    // No "rolled back" wording — we keep successful changes.
    expect(result).not.toContain("removed 1 newly-created file(s)");
    expect(result).not.toContain("restored pre-existing files");

    // The successful add MUST still be on disk.
    expect(existsSync(createdFile)).toBe(true);

    // No checkpoint call because both paths were newly-created
    // (checkpointPaths empty). The failed-add file is best-effort cleaned
    // up via delete_file in the catch block — but only because the
    // simulated write threw AFTER the file was supposedly created. Our
    // mock's write throws before fs.write happens so the file never
    // exists; assert it was attempted but tolerate either outcome.
    expect(calls.some((c) => c.command === "write" && c.params.file === createdFile)).toBe(true);
    expect(calls.some((c) => c.command === "write" && c.params.file === failedFile)).toBe(true);
    // Crucially: NO restore_checkpoint and NO delete on createdFile.
    expect(calls.some((c) => c.command === "restore_checkpoint")).toBe(false);
    expect(calls.some((c) => c.command === "delete_file" && c.params.file === createdFile)).toBe(
      false,
    );
  });

  test("apply_patch restores checkpoint when move source delete fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const earlierFile = resolve(tmpDir, "src/earlier.ts");
    const sourceFile = resolve(tmpDir, "src/original.ts");
    const destFile = resolve(tmpDir, "src/renamed.ts");
    await writeFile(sourceFile, "export const x = 1;\n", { flag: "wx" }).catch(async () => {
      const { mkdir } = await import("node:fs/promises");
      await mkdir(resolve(tmpDir as string, "src"), { recursive: true });
      await writeFile(sourceFile, "export const x = 1;\n");
    });

    const patchText = [
      "*** Begin Patch",
      "*** Add File: src/earlier.ts",
      "+export const earlier = true;",
      "*** Update File: src/original.ts",
      "*** Move to: src/renamed.ts",
      "@@",
      "-export const x = 1;",
      "+export const x = 2;",
      "*** End Patch",
    ].join("\n");

    let destWritten = false;
    const { calls, tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") {
        const file = params.file as string;
        if (file === earlierFile) {
          await writeFile(file, params.content as string);
          return { success: true };
        }
        if (file === destFile) {
          await writeFile(file, params.content as string);
          destWritten = true;
          return { success: true };
        }
      }
      if (command === "delete_file") {
        const file = params.file as string;
        if (file === sourceFile) {
          // Simulate the source delete failing mid-patch.
          throw new Error("Simulated delete_file failure");
        }
        if (file === destFile) {
          await rm(destFile, { force: true });
          return { success: true };
        }
      }
      if (command === "restore_checkpoint") return { success: true };
      throw new Error(`Unexpected command: ${command}`);
    });

    const result = text(await tools.apply_patch.execute({ patchText }, sdkCtx));

    expect(destWritten).toBe(true);
    expect(existsSync(earlierFile)).toBe(true);
    expect(existsSync(destFile)).toBe(false);
    expect(result).toContain("Failed to update src/original.ts");
    expect(result).toContain("restored pre-patch checkpoint");
    expect(result).toContain("Patch partially applied");
    expect(calls.some((c) => c.command === "restore_checkpoint")).toBe(true);
    expect(calls.some((c) => c.command === "delete_file" && c.params.file === destFile)).toBe(true);
  });

  test("apply_patch restores pre-existing move destination when source delete fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const sourceFile = resolve(tmpDir, "src/original.ts");
    const destFile = resolve(tmpDir, "src/renamed.ts");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(resolve(tmpDir, "src"), { recursive: true });
    await writeFile(sourceFile, "export const x = 1;\n");
    await writeFile(destFile, "ORIGINAL\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: src/original.ts",
      "*** Move to: src/renamed.ts",
      "@@",
      "-export const x = 1;",
      "+export const x = 2;",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") {
        await writeFile(params.file as string, params.content as string);
        return { success: true };
      }
      if (command === "delete_file") {
        if (params.file === sourceFile) throw new Error("source locked");
        throw new Error(`unexpected delete_file for ${String(params.file)}`);
      }
      if (command === "restore_checkpoint") {
        await writeFile(destFile, "ORIGINAL\n");
        return { success: true };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    // Total-failure (single hunk) now throws so OpenCode classifies the call
    // as errored. Inspect the thrown error for the rollback messaging.
    let caught: unknown;
    try {
      await tools.apply_patch.execute({ patchText }, sdkCtx);
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeInstanceOf(Error);
    expect((caught as Error).message).toContain("restored pre-patch checkpoint");
    expect(await readFile(destFile, "utf-8")).toBe("ORIGINAL\n");
    expect(calls.some((c) => c.command === "restore_checkpoint")).toBe(true);
    expect(calls.some((c) => c.command === "delete_file" && c.params.file === destFile)).toBe(
      false,
    );
  });

  test("apply_patch reports both copies when move rollback delete also fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const sourceFile = resolve(tmpDir, "src/original.ts");
    const destFile = resolve(tmpDir, "src/renamed.ts");
    const { mkdir } = await import("node:fs/promises");
    await mkdir(resolve(tmpDir, "src"), { recursive: true });
    await writeFile(sourceFile, "export const x = 1;\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: src/original.ts",
      "*** Move to: src/renamed.ts",
      "@@",
      "-export const x = 1;",
      "+export const x = 2;",
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") {
        await writeFile(params.file as string, params.content as string);
        return { success: true };
      }
      if (command === "delete_file") {
        const file = params.file as string;
        if (file === sourceFile) throw new Error("source locked");
      }
      if (command === "restore_checkpoint") throw new Error("restore locked");
      throw new Error(`Unexpected command: ${command}`);
    });

    // Total-failure (single hunk) now throws so OpenCode classifies the call
    // as errored. Inspect the thrown error for the move-rollback messaging.
    let caught: unknown;
    try {
      await tools.apply_patch.execute({ patchText }, sdkCtx);
    } catch (e) {
      caught = e;
    }
    expect(caught).toBeInstanceOf(Error);
    const message = (caught as Error).message;

    expect(existsSync(sourceFile)).toBe(true);
    expect(existsSync(destFile)).toBe(true);
    expect(message).toContain("move_partial_failure");
    expect(message).toContain(sourceFile);
    expect(message).toContain(destFile);
    expect(message).toContain("Both copies may exist or destination content may be changed");
  });

  /// BUG-6a dogfooding repro: the user's exact 3-file complaint. A multi-
  /// file patch where 2 files patch cleanly and the 3rd hits a fuzzy-match
  /// drift used to roll back the 2 successes. Now the 2 successes commit
  /// and only the failed file is reported as failing.
  test("apply_patch keeps successful files when ONE of three updates fails (user repro)", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const okFile1 = resolve(tmpDir, "cli-program.ts");
    const okFile2 = resolve(tmpDir, "cli-installer.ts");
    const driftFile = resolve(tmpDir, "athena-council-guard.ts");

    // Seed all three files with realistic pre-patch content.
    await writeFile(okFile1, "old line 1\n");
    await writeFile(okFile2, "old line 2\n");
    await writeFile(driftFile, "drifted content that won't match\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: cli-program.ts",
      "@@",
      "-old line 1",
      "+new line 1",
      "*** Update File: cli-installer.ts",
      "@@",
      "-old line 2",
      "+new line 2",
      "*** Update File: athena-council-guard.ts",
      "@@",
      "-expected line that doesn't exist in file",
      "+something else",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") {
        const file = params.file as string;
        await writeFile(file, params.content as string);
        return { success: true };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const result = text(await tools.apply_patch.execute({ patchText }, sdkCtx));

    expect(result).toContain("Updated cli-program.ts");
    expect(result).toContain("Updated cli-installer.ts");
    expect(result).toContain("Failed to update athena-council-guard.ts");
    expect(result).toContain("Patch partially applied");
    expect(result).toContain("2 of 3 hunk(s) succeeded");
    expect(result).toContain("aft_safety");

    // The two successful files must reflect the new content on disk.
    expect((await import("node:fs/promises")).readFile(okFile1, "utf-8")).resolves.toBe(
      "new line 1\n",
    );
    expect((await import("node:fs/promises")).readFile(okFile2, "utf-8")).resolves.toBe(
      "new line 2\n",
    );
    // The drifted file is unchanged (applyUpdateChunks throws BEFORE write).
    expect((await import("node:fs/promises")).readFile(driftFile, "utf-8")).resolves.toBe(
      "drifted content that won't match\n",
    );

    // No restore_checkpoint anywhere — that's the whole fix.
    expect(calls.some((c) => c.command === "restore_checkpoint")).toBe(false);
    // No delete_file on the successful files — we keep them.
    expect(calls.some((c) => c.command === "delete_file" && c.params.file === okFile1)).toBe(false);
    expect(calls.some((c) => c.command === "delete_file" && c.params.file === okFile2)).toBe(false);
  });

  // Regression test for the dogfooded report where a single-file patch hit
  // a fuzzy-match drift, our code wrote the failure summary to `output`,
  // and OpenCode's UI rendered the call as `state.status: "completed"` —
  // green check next to "Patch failed — none of the 1 hunk(s) applied".
  // Total-failure cases must throw so OpenCode classifies them as errored
  // (matching native apply_patch which uses Effect.fail for all errors).
  test("apply_patch throws when ALL hunks fail (so OpenCode marks it errored)", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const driftFile = resolve(tmpDir, "src/hooks/index.ts");
    await mkdir(resolve(tmpDir, "src/hooks"), { recursive: true });
    await writeFile(driftFile, "actual content that the patch does not expect\n");

    const patchText = [
      "*** Begin Patch",
      "*** Update File: src/hooks/index.ts",
      "@@",
      '-export { createDelegateTaskRetryHook } from "./delegate-task-retry";',
      '-export { createJsonErrorRecoveryHook } from "./json-error-recovery";',
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: true };
      throw new Error(`Unexpected command: ${command}`);
    });

    let caught: unknown;
    try {
      await tools.apply_patch.execute({ patchText }, sdkCtx);
    } catch (e) {
      caught = e;
    }

    expect(caught).toBeInstanceOf(Error);
    const message = (caught as Error).message;
    expect(message).toContain("Failed to update src/hooks/index.ts");
    expect(message).toContain("Patch failed");
    expect(message).toContain("none of the 1 hunk(s) applied");
  });

  test("read returns binary-file messages without trying to split missing content", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      binary: true,
      message: "Binary file (512 bytes)",
    }));

    const result = text(await tools.read.execute({ filePath: "artifact.bin" }, sdkCtx));

    expect(result).toBe("Binary file (512 bytes)");
    expect(calls[0]).toEqual({
      command: "read",
      params: {
        file: resolve(tmpDir, "artifact.bin"),
        session_id: "test",
      },
    });
  });

  test("read handles directory listings and truncated content responses", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    let callIndex = 0;
    const { tools } = createMockHoistedHarness(async () => {
      callIndex += 1;
      if (callIndex === 1) {
        return { success: true, entries: ["a.ts", "src/"] };
      }

      return {
        success: true,
        content: "1: one\n2: two",
        truncated: true,
        start_line: 1,
        end_line: 2,
        total_lines: 10,
      };
    });

    const directoryResult = text(await tools.read.execute({ filePath: "." }, sdkCtx));
    const truncatedResult = text(await tools.read.execute({ filePath: "big.ts" }, sdkCtx));

    expect(directoryResult).toBe("a.ts\nsrc/");
    // Case B: agent did NOT specify a range, response was clamped → hint footer
    // is useful, tells the agent more exists and how to get it.
    expect(truncatedResult).toBe(
      "1: one\n2: two\n(Showing lines 1-2 of 10. Use startLine/endLine to read other sections.)",
    );
  });

  test("read does not append a footer when the file fits in default limit (case A)", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      content: "1: one\n2: two\n3: three",
      // truncated:false means the response IS the whole file — no footer needed.
      truncated: false,
      start_line: 1,
      end_line: 3,
      total_lines: 3,
    }));

    const result = text(await tools.read.execute({ filePath: "small.ts" }, sdkCtx));

    expect(result).toBe("1: one\n2: two\n3: three");
  });

  test("read drops the navigation hint when the agent supplied startLine/endLine (case B)", async () => {
    // Repro for the dogfooding bug: agent calls read({startLine: 130, endLine: 190})
    // on a 191-line file and gets back lines 130-190 EXACTLY. Telling them
    // "use startLine/endLine to read other sections" right after they used
    // those exact params is patronizing. They have the math.
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      content: "130: ...\n190: ...",
      // Rust still reports truncated:true because the response is a slice
      // of the file (end_line < total_lines). The plugin must NOT key the
      // hint off this flag alone — it needs to know the agent picked the slice.
      truncated: true,
      start_line: 130,
      end_line: 190,
      total_lines: 191,
    }));

    const result = text(
      await tools.read.execute({ filePath: "registry.ts", startLine: 130, endLine: 190 }, sdkCtx),
    );

    // The user's exact complaint: when end_line matches total_lines (or is
    // close to it after a deliberate range), no footer should be emitted at
    // all. Agent gets back only the content.
    expect(result).toBe("130: ...\n190: ...");
    expect(result).not.toContain("Use startLine/endLine");
  });

  test("read drops the footer entirely when the agent's range happens not to cover the full file (case B)", async () => {
    // Subtle case: agent asked 100-150 of a 200-line file. They got back
    // exactly what they asked for. The earlier "compact when clamped"
    // branch would have spuriously emitted `(Lines 100-150 of 200)` here,
    // which is the SAME shape of patronizing footer as the original bug —
    // re-teaching an agent that they got less than the whole file when
    // THEY chose to. Agent has the math: they sent the request and they
    // can see the content length.
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      content: "100: ...\n150: ...",
      truncated: true,
      start_line: 100,
      end_line: 150,
      total_lines: 200,
    }));

    const result = text(
      await tools.read.execute({ filePath: "mid.ts", startLine: 100, endLine: 150 }, sdkCtx),
    );

    expect(result).toBe("100: ...\n150: ...");
    expect(result).not.toContain("Use startLine/endLine");
    expect(result).not.toContain("(Lines 100-150");
  });

  test("read drops the navigation hint when the agent supplied offset/limit (case B)", async () => {
    // Same as the startLine/endLine case but for the OpenCode-built-in-
    // compatible offset/limit param shape. Agent that picked the slice
    // should not be re-taught how to pick a slice.
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async () => ({
      success: true,
      content: "10: ...\n29: ...",
      truncated: true,
      start_line: 10,
      end_line: 29,
      total_lines: 30,
    }));

    const result = text(
      await tools.read.execute({ filePath: "small.ts", offset: 10, limit: 20 }, sdkCtx),
    );

    // No footer at all — agent picked the range, has the math.
    expect(result).toBe("10: ...\n29: ...");
    expect(result).not.toContain("Use startLine/endLine");
    expect(result).not.toContain("(Lines");
  });

  test("write distinguishes new files from updates", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    let writeCount = 0;
    const { calls, tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("write");
      writeCount += 1;
      return writeCount === 1
        ? { success: true, created: true, formatted: false }
        : { success: true, created: false, formatted: true };
    });

    const createdResult = text(
      await tools.write.execute(
        { filePath: "created.ts", content: "export const created = true;\n" },
        sdkCtx,
      ),
    );
    const updatedResult = text(
      await tools.write.execute(
        { filePath: "created.ts", content: "export const created = false;\n" },
        sdkCtx,
      ),
    );

    expect(createdResult).toBe("Created new file.");
    expect(updatedResult).toBe("File updated. Auto-formatted.");
    expect(calls).toHaveLength(2);
    expect(calls[0]?.params.file).toBe(resolve(tmpDir, "created.ts"));
    expect(calls[1]?.params.file).toBe(resolve(tmpDir, "created.ts"));
  });

  /// Regression: v0.15.3 — apply_patch metadata.files entries must include
  /// `patch`, `additions`, and `deletions` for OpenCode's UI to render diffs.
  ///
  /// OpenCode's UI patchFile() at packages/ui/src/components/apply-patch-file.ts
  /// drops any file metadata entry that lacks all of `patch`, `before`, `after`.
  /// Pre-fix, AFT only sent `{ filePath, relativePath, type }`, so EVERY file
  /// was silently dropped and the TUI/desktop showed no diffs at all.
  test("apply_patch returns per-file diff metadata for the OpenCode renderer", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const updatedFile = resolve(tmpDir, "updated.ts");
    const deletedFile = resolve(tmpDir, "deleted.ts");

    // Seed source files for the update + delete hunks (apply_patch reads
    // them via fs.readFile to compute per-file diffs).
    await writeFile(updatedFile, "old line\n");
    await writeFile(deletedFile, "to be deleted\n");

    const patchText = [
      "*** Begin Patch",
      "*** Add File: new.ts",
      "+export const created = 1;",
      "*** Update File: updated.ts",
      "@@",
      "-old line",
      "+new line",
      "*** Delete File: deleted.ts",
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command) => {
      if (command === "apply_patch") return previewResponse();
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: true };
      if (command === "delete_file") return { success: true };
      throw new Error(`Unexpected command: ${command}`);
    });

    const stored = (await tools.apply_patch.execute({ patchText }, sdkCtx)) as {
      title?: string;
      metadata?: Record<string, unknown>;
    };
    expect(stored).toBeDefined();
    expect(stored?.title).toContain("Success. Updated the following files:");
    expect(stored?.title).toContain("A new.ts");
    expect(stored?.title).toContain("M updated.ts");
    expect(stored?.title).toContain("D deleted.ts");

    const meta = stored?.metadata as {
      diff: string;
      files: Array<{
        filePath: string;
        relativePath: string;
        type: string;
        patch: string;
        additions: number;
        deletions: number;
        movePath?: string;
      }>;
    };

    expect(meta.diff).toBeTypeOf("string");
    expect(meta.files).toHaveLength(3);

    // Each file MUST carry patch + additions + deletions or the OpenCode UI
    // will silently drop it (the v0.15.3 regression). This assertion
    // catches any future change that strips these fields.
    for (const file of meta.files) {
      expect(file.filePath).toBeTypeOf("string");
      expect(file.relativePath).toBeTypeOf("string");
      expect(["add", "update", "delete", "move"]).toContain(file.type);
      expect(file.patch).toBeTypeOf("string");
      expect(file.patch.length).toBeGreaterThan(0);
      expect(file.additions).toBeTypeOf("number");
      expect(file.deletions).toBeTypeOf("number");
    }

    // Sanity-check shape of each per-file entry. We don't assert exact
    // additions/deletions counts because buildUnifiedDiff treats absent
    // content as an empty line ("") which shows up in the diff — the
    // important contract is that `patch` and the counters are present
    // and non-degenerate, which the per-entry loop above already checks.
    const addEntry = meta.files.find((f) => f.type === "add");
    expect(addEntry?.additions).toBeGreaterThan(0);

    const updateEntry = meta.files.find((f) => f.type === "update");
    expect(updateEntry?.additions).toBeGreaterThan(0);
    expect(updateEntry?.deletions).toBeGreaterThan(0);

    const deleteEntry = meta.files.find((f) => f.type === "delete");
    expect(deleteEntry?.deletions).toBeGreaterThan(0);
  });
});

/**
 * Verify the bash hoisting gate. Hoisted bash replaces OpenCode's built-in
 * bash, so registering it without any `experimental.bash.*` flag set means
 * users with default config silently get our (untested-against-them) bash
 * code path with zero experimental benefit. We register `bash`,
 * `bash_status`, and `bash_kill` together when at least one
 * `experimental.bash.*` flag is enabled.
 *
 * `bash_status` and `bash_kill` ride alongside `bash` regardless of which
 * experimental flag enabled it: foreground bash auto-promotes long-running
 * tasks to background after a short wait-window, so the agent always needs
 * tools to inspect or kill those promoted tasks. Earlier versions gated
 * them on `experimental.bash.background` specifically, which left
 * compress/rewrite-only users with promotion messages referencing
 * non-existent tools. (See council audit
 * `.alfonso/athena/council-aft-bash-timeout-audit-057818e1583d3883/`.)
 */
describe("Hoisted bash gating (post v0.27.2 graduation)", () => {
  function toolsWithConfig(
    cfg: Partial<PluginContext["config"]>,
    prefixed = false,
  ): Record<string, unknown> {
    const pool = { getBridge: () => ({ send: async () => ({}) }) } as unknown as BridgePool;
    const ctx: PluginContext = {
      pool,
      client: createMockClient(),
      config: cfg as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    return prefixed ? aftPrefixedTools(ctx) : hoistedTools(ctx);
  }

  // ---- Surface defaults (new graduation behavior) ----------------------

  test("no bash config + tool_surface=recommended → bash registered (graduated on by default)", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended" });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
    expect(tools.read).toBeDefined();
    expect(tools.edit).toBeDefined();
  });

  test("no bash config + tool_surface=all → bash registered", () => {
    const tools = toolsWithConfig({ tool_surface: "all" });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("no bash config + tool_surface=minimal → bash NOT registered", () => {
    // Minimal surface opts out of everything not strictly core, including
    // bash. Users on minimal need to opt back in with explicit `bash: true`.
    const tools = toolsWithConfig({ tool_surface: "minimal" });
    expect(tools.bash).toBeUndefined();
    expect(tools.bash_status).toBeUndefined();
    expect(tools.bash_kill).toBeUndefined();
  });

  // ---- Top-level bash shape (new surface) ------------------------------

  test("bash: true → all three bash tools registered, all sub-features on", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: true });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("bash: false → no bash-family tools registered (hard opt-out)", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: false });
    expect(tools.bash).toBeUndefined();
    expect(tools.bash_status).toBeUndefined();
    expect(tools.bash_kill).toBeUndefined();
  });

  test("bash: { rewrite: false } → partial override; bash still registered (object form enables hoist)", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: { rewrite: false } });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  // ---- Legacy experimental.bash shape (backward compat) ----------------

  test("legacy experimental.bash.rewrite=true → all three bash tools registered", () => {
    // Foreground bash can auto-promote even without the background flag,
    // so status/kill must be available for the promoted task.
    const tools = toolsWithConfig({ experimental: { bash: { rewrite: true } } });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("legacy experimental.bash.compress=true → all three bash tools registered", () => {
    const tools = toolsWithConfig({ experimental: { bash: { compress: true } } });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("legacy experimental.bash.background=true → all three bash tools registered", () => {
    const tools = toolsWithConfig({ experimental: { bash: { background: true } } });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("legacy experimental.bash with all three flags true → full bash surface", () => {
    const tools = toolsWithConfig({
      experimental: { bash: { rewrite: true, compress: true, background: true } },
    });
    expect(tools.bash).toBeDefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("legacy experimental.bash empty + tool_surface=minimal → NOT registered", () => {
    // Empty legacy block + minimal surface = no opt-in anywhere, no bash.
    const tools = toolsWithConfig({ tool_surface: "minimal", experimental: { bash: {} } });
    expect(tools.bash).toBeUndefined();
    expect(tools.bash_status).toBeUndefined();
    expect(tools.bash_kill).toBeUndefined();
  });

  // ---- Hoist-off mode --------------------------------------------------

  test("hoist-off + legacy experimental.bash.rewrite=true → aft_bash plus status/kill registered", () => {
    const tools = toolsWithConfig(
      { hoist_builtin_tools: false, experimental: { bash: { rewrite: true } } },
      true,
    );
    expect(tools.aft_bash).toBeDefined();
    expect(tools.bash).toBeUndefined();
    expect(tools.bash_status).toBeDefined();
    expect(tools.bash_kill).toBeDefined();
  });

  test("hoist-off + bash: false → no bash-family tools registered", () => {
    const tools = toolsWithConfig({ hoist_builtin_tools: false, bash: false }, true);
    expect(tools.aft_bash).toBeUndefined();
    expect(tools.bash).toBeUndefined();
    expect(tools.bash_status).toBeUndefined();
    expect(tools.bash_kill).toBeUndefined();
  });
});
