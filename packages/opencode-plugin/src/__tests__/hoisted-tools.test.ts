/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import type { BridgePool, ToolCallOptions } from "@cortexkit/aft-bridge";
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
type SendCall = {
  command: string;
  params: Record<string, unknown>;
  options?: ToolCallOptions;
};

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
  return { success: true, preview: true, preview_diff: diff, text: "Preview ready." };
}

function isPreviewCall(call: SendCall | undefined): boolean {
  return call?.options?.preview === true;
}

function recordedToolCallOptions(options?: ToolCallOptions): ToolCallOptions | undefined {
  return options?.preview === true ? { preview: true } : undefined;
}

function createMockHoistedHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
    options?: ToolCallOptions,
  ) => Promise<BridgeResponse> | BridgeResponse,
  config: PluginContext["config"] = {} as PluginContext["config"],
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push({ command, params });
      return await sendImpl(command, params);
    },
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
      options?: ToolCallOptions,
    ) => {
      const recordedOptions = recordedToolCallOptions(options);
      calls.push({
        command: name,
        params: rawArgs,
        ...(recordedOptions ? { options: recordedOptions } : {}),
      });
      return await sendImpl(name, rawArgs, options);
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

  test("read maps image attachments to OpenCode data URLs and metadata", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return {
        success: true,
        text: "Read image attachment (image/png, 32×16, 1 KB).",
        attachments: [
          {
            kind: "image",
            mime: "image/png",
            data: "aW1hZ2U=",
            bytes: 1024,
            base64_bytes: 8,
            width: 32,
            height: 16,
            resized: false,
            animation: "none",
            orientation_applied: false,
          },
        ],
      };
    });

    const result = (await tools.read.execute({ filePath: "image.png" }, sdkCtx)) as {
      output: string;
      attachments?: Array<{ type: string; mime: string; url: string }>;
      metadata?: { isImage?: boolean; isPdf?: boolean };
    };

    expect(result.output).toContain("Read image");
    expect(result.attachments).toEqual([
      { type: "file", mime: "image/png", url: "data:image/png;base64,aW1hZ2U=" },
    ]);
    expect(result.metadata?.isImage).toBe(true);
    expect(result.metadata?.isPdf).toBe(false);
  });

  test("read maps PDF attachments to OpenCode data URLs and metadata", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("read");
      return {
        success: true,
        text: "Read PDF attachment (128 bytes).",
        attachments: [
          {
            kind: "pdf",
            mime: "application/pdf",
            data: "JVBERi0=",
            bytes: 128,
            base64_bytes: 8,
          },
        ],
      };
    });

    const result = (await tools.read.execute({ filePath: "doc.pdf" }, sdkCtx)) as {
      attachments?: Array<{ type: string; mime: string; url: string }>;
      metadata?: { isImage?: boolean; isPdf?: boolean };
    };

    expect(result.attachments).toEqual([
      { type: "file", mime: "application/pdf", url: "data:application/pdf;base64,JVBERi0=" },
    ]);
    expect(result.metadata?.isImage).toBe(false);
    expect(result.metadata?.isPdf).toBe(true);
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

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview ? previewResponse() : { success: true, text: "File updated." },
    );

    const result = text(
      await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx),
    );

    expect(result).toBe("File updated.");
    expect(result).not.toContain("lsp_diagnostics");
    expect(result).not.toContain("LSP errors detected");
    expect(calls).toHaveLength(2);
    expect(calls[0]).toMatchObject({
      command: "write",
      params: {
        filePath: "src/app.ts",
        content: "export {};\n",
      },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({
      command: "write",
      params: { filePath: "src/app.ts", content: "export {};\n" },
    });
  });

  test("write leaves diagnostics server-owned and out of agent arguments", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview ? previewResponse() : { success: true, text: "File updated." },
    );

    await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx);

    await tools.write.execute(
      { filePath: "src/app.ts", content: "export {};\n", diagnostics: false },
      sdkCtx,
    );
    expect(calls).toHaveLength(4);
    for (const call of calls) {
      expect(call.params.diagnostics).toBeUndefined();
      expect(call.params.include_diff_content).toBeUndefined();
      expect(call.params.preview).toBeUndefined();
    }
  });

  test("write surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : {
            success: true,
            text: "File updated.\n\nLSP errors detected, please fix:\n  Line 7: Bad type\n",
          },
    );

    const result = text(
      await tools.write.execute({ filePath: "src/app.ts", content: "export {};\n" }, sdkCtx),
    );

    expect(calls.every((call) => call.params.diagnostics === undefined)).toBe(true);
    expect(result).toContain("LSP errors detected");
    expect(result).toContain("Line 7: Bad type");
  });

  test("edit defaults diagnostics off and omits LSP payload", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 1, text: "Edited (+0/-0)." },
    );

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
    const previewCall = calls.find(isPreviewCall);
    const realCall = calls.find((call) => !isPreviewCall(call));
    expect(previewCall?.command).toBe("edit");
    expect(previewCall?.params.diagnostics).toBeUndefined();
    expect(previewCall?.params.include_diff_content).toBeUndefined();
    expect(realCall?.command).toBe("edit");
    expect(realCall?.params.diagnostics).toBeUndefined();
    expect(realCall?.params.include_diff_content).toBeUndefined();
  });

  test("edit surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const diagnostics = [{ severity: "error", line: 3, message: "Missing import" }];
    const { calls, tools } = createMockHoistedHarness(
      async (_command, _params, options) =>
        options?.preview
          ? previewResponse()
          : {
              success: true,
              replacements: 1,
              lsp_diagnostics: diagnostics,
              text: "Edited (+0/-0).\n\nLSP errors detected, please fix:\n  Line 3: Missing import",
            },
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
    const realCall = calls.find((call) => !isPreviewCall(call));
    expect(realCall?.params.diagnostics).toBeUndefined();
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

    const previewDiff =
      "Index: src.ts\n--- src.ts\n+++ src.ts\n@@ -1,1 +1,1 @@\n-export const value = 1;\n+export const value = 2;\n";
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse(previewDiff)
        : { success: true, created: false, text: "File updated." },
    );

    await tools.write.execute({ filePath: "src.ts", content: "export const value = 2;\n" }, sdkCtx);

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "src.ts"));
    expect(editAsk?.metadata?.diff).toBeTypeOf("string");
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("write approval ask includes all-additions diff metadata for a new file", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const previewDiff =
      "Index: new.ts\n--- new.ts\n+++ new.ts\n@@ -0,0 +1,1 @@\n+export const fresh = true;\n";
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse(previewDiff)
        : { success: true, created: true, text: "Created new file." },
    );

    await tools.write.execute(
      { filePath: "new.ts", content: "export const fresh = true;\n" },
      sdkCtx,
    );

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "new.ts"));
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toBeTypeOf("string");
    expect(diff).toBe(previewDiff);
  });

  test("edit oldString approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const previewDiff =
      "Index: src.ts\n--- src.ts\n+++ src.ts\n@@ -1,1 +1,1 @@\n-const value = 1;\n+const value = 2;\n";
    const { tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) {
        return {
          success: true,
          preview_diff: previewDiff,
          text: "Preview ready.",
        };
      }
      return { success: true, replacements: 1, text: "Edited (+1/-1)." };
    });

    await tools.edit.execute({ filePath: "src.ts", oldString: "1", newString: "2" }, sdkCtx);

    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("edit symbol approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const previewDiff =
      "Index: symbol.ts\n--- symbol.ts\n+++ symbol.ts\n@@ -1,1 +1,1 @@\n-export function oldName() {}\n+export function newName() {}\n";

    const { calls, tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("edit");
      if (options?.preview) return previewResponse(previewDiff);
      return {
        success: true,
        symbol: "oldName",
        operation: "replace",
        diff: { additions: 1, deletions: 1 },
        text: "Edited (+1/-1).",
      };
    });

    await tools.edit.execute(
      { filePath: "symbol.ts", symbol: "oldName", content: "export function newName() {}\n" },
      sdkCtx,
    );

    expect(calls[0]?.options?.preview).toBe(true);
    const diff = askCalls.find((call) => call.permission === "edit")?.metadata?.diff as string;
    expect(diff).toBe(previewDiff);
  });

  test("edit batch approval ask uses the Rust preview diff", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    const previewDiff =
      "Index: batch.ts\n--- batch.ts\n+++ batch.ts\n@@ -1,2 +1,2 @@\n-before\n+after\n";

    const { tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("edit");
      if (options?.preview) return previewResponse(previewDiff);
      return { success: true, edits_applied: 1, text: "Edited (+1/-1)." };
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

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) => {
      if (options?.preview) return { success: false, message: "match_not_found", text: "" };
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
    expect(calls[0]?.options?.preview).toBe(true);
  });

  test("apply_patch approval ask uses TS preview diff for add/update/delete/move", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;
    await writeFile(resolve(tmpDir, "updated.ts"), "old update\n");
    await writeFile(resolve(tmpDir, "deleted.ts"), "delete me\n");
    await writeFile(resolve(tmpDir, "from.ts"), "move source\n");

    const patchText = [
      "*** Begin Patch",
      "*** Add File: new.ts",
      "+added line",
      "*** Update File: updated.ts",
      "@@",
      "-old update",
      "+new update",
      "*** Delete File: deleted.ts",
      "*** Update File: from.ts",
      "*** Move to: to.ts",
      "@@",
      "-move source",
      "+move dest",
      "*** End Patch",
    ].join("\n");

    const { calls, tools } = createMockHoistedHarness(async (command) => {
      if (command === "checkpoint") return { success: true };
      if (command === "write") return { success: true };
      if (command === "delete_file") return { success: true };
      throw new Error(`Unexpected command: ${command}`);
    });

    await tools.apply_patch.execute({ patchText }, sdkCtx);

    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(resolve(tmpDir, "new.ts"));
    const diff = editAsk?.metadata?.diff as string;
    expect(diff).toBeTypeOf("string");
    expect(diff).toContain(`Index: ${resolve(tmpDir, "new.ts")}`);
    expect(diff).toContain("+added line");
    expect(diff).toContain(`Index: ${resolve(tmpDir, "updated.ts")}`);
    expect(diff).toContain("-old update");
    expect(diff).toContain("+new update");
    expect(diff).toContain(`Index: ${resolve(tmpDir, "deleted.ts")}`);
    expect(diff).toContain("-delete me");
    expect(diff).toContain(`Index: ${resolve(tmpDir, "to.ts")}`);
    expect(diff).toContain("-move source");
    expect(diff).toContain("+move dest");
    expect(calls.some((call) => call.command === "apply_patch")).toBe(false);
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

    const { calls, tools } = createMockHoistedHarness(async (_command) => {
      throw new Error(`Unexpected command after preview failure: ${_command}`);
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Failed to update file.ts",
    );
    expect(askCalls).toHaveLength(0);
    expect(calls).toHaveLength(0);
  });

  test("edit throws the Rust error response for failed replacements", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("edit");
      if (options?.preview) return { success: false, message: "Match not found in file", text: "" };
      throw new Error("real edit should not run after preview failure");
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

    const { tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("write");
      return options?.preview
        ? previewResponse()
        : {
            success: true,
            rolled_back: true,
            created: false,
            text: "Write rolled back: the content produced invalid syntax, so the file was left unchanged.",
          };
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
      expect(command).toBe("aft_delete");
      const files = (params.files as string[]) ?? [];
      return {
        success: true,
        text: `Deleted 0/${files.length} file(s)`,
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
      expect(command).toBe("aft_delete");
      return { success: false, message: "bridge delete refused" };
    });

    await expect(tools.aft_delete.execute({ files: ["doomed.ts"] }, sdkCtx)).rejects.toThrow(
      "bridge delete refused",
    );
  });

  test("delete returns readable partial-success text when some files fail", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("aft_delete");
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
        text: `Deleted ${deleted.length}/${files.length} file(s)`,
        complete: skipped.length === 0,
        deleted,
        skipped_files: skipped,
      };
    });

    const result = await tools.aft_delete.execute(
      { files: ["a.ts", "blocked.ts", "c.ts"] },
      sdkCtx,
    );
    expect(result).toBe("Deleted 2/3 file(s)");
    expect(result.trim().startsWith("{")).toBe(false);
  });

  test("delete returns readable text when every file succeeds", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command, params) => {
      expect(command).toBe("aft_delete");
      const files = (params.files as string[]) ?? [];
      return {
        success: true,
        text: `Deleted ${files.length}/${files.length} file(s)`,
        complete: true,
        deleted: files.map((file) => ({ file, backup_id: null })),
        skipped_files: [],
      };
    });

    const result = await tools.aft_delete.execute({ files: ["a.ts", "b.ts"] }, sdkCtx);
    expect(result).toBe("Deleted 2/2 file(s)");
    expect(result.trim().startsWith("{")).toBe(false);
  });

  test("move throws the Rust error response when rename fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { tools } = createMockHoistedHarness(async (command) => {
      expect(command).toBe("aft_move");
      return { success: false, message: "Destination already exists" };
    });

    await expect(
      tools.aft_move.execute({ filePath: "source.ts", destination: "dest.ts" }, sdkCtx),
    ).rejects.toThrow("Destination already exists");
  });

  test("edit batch mode forwards raw camelCase fields through tool_call", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, edits_applied: 2, text: "Edited (+0/-0, 2 edits)." },
    );

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
      command: "edit",
      params: {
        filePath: "batch.ts",
        edits: [
          { oldString: "before", newString: "after" },
          { startLine: 4, endLine: 6, content: "replacement" },
        ],
      },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({
      command: "edit",
      params: {
        filePath: "batch.ts",
        edits: [
          { oldString: "before", newString: "after" },
          { startLine: 4, endLine: 6, content: "replacement" },
        ],
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

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 3, text: "Edited (+0/-0, 3 replacements)." },
    );

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
      command: "edit",
      params: {
        filePath: "repeated.ts",
        oldString: "oldName",
        newString: "newName",
        replaceAll: true,
      },
      options: { preview: true },
    });
    expect(calls[1]).toEqual({
      command: "edit",
      params: {
        filePath: "repeated.ts",
        oldString: "oldName",
        newString: "newName",
        replaceAll: true,
      },
    });
  });

  test('edit forwards string replaceAll "true" to Rust replace_all', async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 1, text: "Edited (+0/-0)." },
    );

    await tools.edit.execute(
      {
        filePath: "repeated.ts",
        oldString: "oldName",
        newString: "newName",
        replaceAll: "true" as unknown as boolean,
      },
      sdkCtx,
    );

    const applyCall = calls.find((c) => c.command === "edit" && !isPreviewCall(c));
    expect(applyCall?.params.replaceAll).toBe(true);
  });

  test('edit coerces string occurrence "0" and keeps the first occurrence selectable', async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : { success: true, replacements: 1, text: "Edited (+0/-0)." },
    );

    await tools.edit.execute(
      {
        filePath: "repeated.ts",
        oldString: "oldName",
        newString: "newName",
        occurrence: "0" as unknown as number,
      },
      sdkCtx,
    );

    const applyCall = calls.find((c) => c.command === "edit" && !isPreviewCall(c));
    expect(applyCall?.params.occurrence).toBe(0);
  });

  /// Diff-payload contract: the server returns full before/after for UI
  /// metadata, but the agent-facing result must stay as compact rendered text.
  /// Echoing before/after into the model context makes the payload scale with
  /// file size, not edit size.
  test("edit agent result strips diff before/after to counts-only", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const bigBefore = `${"x".repeat(50_000)}\n`;
    const bigAfter = `${"y".repeat(50_000)}\n`;
    const { tools } = createMockHoistedHarness(async (_command, _params, options) =>
      options?.preview
        ? previewResponse()
        : {
            success: true,
            replacements: 1,
            text: "Edited (+1/-1).",
            diff: { before: bigBefore, after: bigAfter, additions: 1, deletions: 1 },
          },
    );

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

  test("apply_patch metadata collapses delete+add on the same path into one net diff", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const targetFile = resolve(tmpDir, "replace.ts");
    await writeFile(targetFile, "export const value = 1;\n");

    const patchText = [
      "*** Begin Patch",
      "*** Delete File: replace.ts",
      "*** Add File: replace.ts",
      "+export const value = 2;",
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "checkpoint") return { success: true };
      if (command === "delete_file") {
        await rm(params.file as string, { force: true });
        return { success: true };
      }
      if (command === "write") {
        await writeFile(params.file as string, params.content as string);
        return { success: true };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const result = (await tools.apply_patch.execute({ patchText }, sdkCtx)) as {
      output: string;
      title: string;
      metadata: {
        diff: string;
        files: Array<{
          relativePath: string;
          type: string;
          additions: number;
          deletions: number;
          patch: string;
        }>;
      };
    };

    expect(result.output).toContain("Deleted replace.ts");
    expect(result.output).toContain("Created replace.ts");
    expect(result.metadata.files).toHaveLength(1);
    const file = result.metadata.files[0];
    expect(file.relativePath).toBe("replace.ts");
    expect(file.type).toBe("update");
    expect(file.additions).toBe(1);
    expect(file.deletions).toBe(1);
    expect(file.patch).toContain("-export const value = 1;");
    expect(file.patch).toContain("+export const value = 2;");
    expect(result.metadata.diff).toBe(file.patch);
    expect(result.title).toContain("M replace.ts");
    expect(result.title).not.toContain("D replace.ts");
    expect(result.title).not.toContain("A replace.ts");
  });

  test("apply_patch metadata keeps an earlier same-path delete when a later add fails", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const targetFile = resolve(tmpDir, "replace.ts");
    await writeFile(targetFile, "export const value = 1;\n");

    const patchText = [
      "*** Begin Patch",
      "*** Delete File: replace.ts",
      "*** Add File: replace.ts",
      "+export const value = 2;",
      "*** End Patch",
    ].join("\n");

    const { tools } = createMockHoistedHarness(async (command, params) => {
      if (command === "checkpoint") return { success: true };
      if (command === "delete_file") {
        await rm(params.file as string, { force: true });
        return { success: true };
      }
      if (command === "write") {
        return { success: false, message: "simulated write failure" };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const result = (await tools.apply_patch.execute({ patchText }, sdkCtx)) as {
      output: string;
      title: string;
      metadata: {
        diff: string;
        files: Array<{
          relativePath: string;
          type: string;
          additions: number;
          deletions: number;
          patch: string;
        }>;
      };
    };

    expect(result.output).toContain("Deleted replace.ts");
    expect(result.output).toContain("Failed to create replace.ts: simulated write failure");
    expect(result.output).toContain("Patch partially applied");
    expect(result.output).toContain("1 of 2 hunk(s) succeeded");
    expect(existsSync(targetFile)).toBe(false);

    expect(result.metadata.files).toHaveLength(1);
    const file = result.metadata.files[0];
    expect(file.relativePath).toBe("replace.ts");
    expect(file.type).toBe("delete");
    expect(file.additions).toBe(0);
    expect(file.deletions).toBe(1);
    expect(file.patch).toContain("-export const value = 1;");
    expect(file.patch).not.toContain("+export const value = 2;");
    expect(result.metadata.diff).toBe(file.patch);
    expect(result.title).toContain("Partially applied (1 of 2)");
    expect(result.title).toContain("D replace.ts");
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

  test("apply_patch preview stops before approval when ONE of three updates cannot match", async () => {
    tmpDir = await makeTempDir();
    const askCalls: Array<Record<string, unknown>> = [];
    sdkCtx = { ...createMockSdkContext(tmpDir), ask: recordingAsk(askCalls) } as ToolContext;

    const okFile1 = resolve(tmpDir, "cli-program.ts");
    const okFile2 = resolve(tmpDir, "cli-installer.ts");
    const driftFile = resolve(tmpDir, "athena-council-guard.ts");

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

    const { calls, tools } = createMockHoistedHarness(async (command) => {
      throw new Error(`Unexpected bridge command after preview failure: ${command}`);
    });

    await expect(tools.apply_patch.execute({ patchText }, sdkCtx)).rejects.toThrow(
      "Failed to update athena-council-guard.ts",
    );

    expect(askCalls).toHaveLength(0);
    expect(calls).toHaveLength(0);
    expect(await readFile(okFile1, "utf-8")).toBe("old line 1\n");
    expect(await readFile(okFile2, "utf-8")).toBe("old line 2\n");
    expect(await readFile(driftFile, "utf-8")).toBe("drifted content that won't match\n");
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
    expect(message).toContain("Failed to find expected lines");
  });

  test("read returns binary-file messages without trying to split missing content", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "Binary file (512 bytes)",
      binary: true,
      message: "Binary file (512 bytes)",
    }));

    const result = text(await tools.read.execute({ filePath: "artifact.bin" }, sdkCtx));

    expect(result).toBe("Binary file (512 bytes)");
    expect(calls[0]).toEqual({
      command: "read",
      params: {
        filePath: "artifact.bin",
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
        return { success: true, text: "a.ts\nsrc/", entries: ["a.ts", "src/"] };
      }

      return {
        success: true,
        text: "1: one\n2: two\n(Showing lines 1-2 of 10. Use startLine/endLine to read other sections.)",
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
      text: "1: one\n2: two\n3: three",
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
      text: "130: ...\n190: ...",
      content: "130: ...\n190: ...",
      // Rust sets truncated:true for any file slice (end_line < total_lines).
      // The server must not base its truncation hint only on that flag; it must
      // also check whether the agent explicitly chose the slice via startLine/endLine.
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
      text: "100: ...\n150: ...",
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

    const { calls, tools } = createMockHoistedHarness(async () => ({
      success: true,
      text: "10: ...\n29: ...",
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
    expect(calls[0]).toEqual({
      command: "read",
      params: { filePath: "small.ts", startLine: 10, endLine: 29 },
    });
  });

  test("write distinguishes new files from updates", async () => {
    tmpDir = await makeTempDir();
    sdkCtx = createMockSdkContext(tmpDir);

    let writeCount = 0;
    const { calls, tools } = createMockHoistedHarness(async (command, _params, options) => {
      expect(command).toBe("write");
      if (options?.preview) return previewResponse();
      writeCount += 1;
      return writeCount === 1
        ? { success: true, created: true, formatted: false, text: "Created new file." }
        : { success: true, created: false, formatted: true, text: "File updated. Auto-formatted." };
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
    expect(calls).toHaveLength(4);
    expect(calls[0]?.params.filePath).toBe("created.ts");
    expect(calls[1]?.params.filePath).toBe("created.ts");
    expect(calls[2]?.params.filePath).toBe("created.ts");
    expect(calls[3]?.params.filePath).toBe("created.ts");
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
 * bash when the resolved bash config enables it. The primary `bash` tool can
 * be present without the background control surface: `bash.background: false`
 * means foreground commands block to completion and no `bash_status` /
 * `bash_kill` / `bash_write` / `bash_watch` tools are registered.
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

  function expectBackgroundControls(
    tools: Record<string, unknown>,
    expected: "present" | "absent",
  ): void {
    for (const name of ["bash_status", "bash_write", "bash_watch", "bash_kill"]) {
      if (expected === "present") {
        expect(tools[name]).toBeDefined();
      } else {
        expect(tools[name]).toBeUndefined();
      }
    }
  }

  // ---- Surface defaults (graduated behavior) ---------------------------

  test("no bash config + tool_surface=recommended → full bash surface registered", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended" });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "present");
    expect(tools.read).toBeDefined();
    expect(tools.edit).toBeDefined();
  });

  test("no bash config + tool_surface=all → full bash surface registered", () => {
    const tools = toolsWithConfig({ tool_surface: "all" });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "present");
  });

  test("no bash config + tool_surface=minimal → bash NOT registered", () => {
    // Minimal surface opts out of everything not strictly core, including
    // bash. Users on minimal need to opt back in with explicit `bash: true`.
    const tools = toolsWithConfig({ tool_surface: "minimal" });
    expect(tools.bash).toBeUndefined();
    expectBackgroundControls(tools, "absent");
  });

  // ---- Top-level bash shape --------------------------------------------

  test("bash: true → full bash surface registered", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: true });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "present");
  });

  test("bash: false → no bash-family tools registered (hard opt-out)", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: false });
    expect(tools.bash).toBeUndefined();
    expectBackgroundControls(tools, "absent");
  });

  test("bash: { rewrite: false } → object form defaults background on", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: { rewrite: false } });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "present");
  });

  test("bash: { background: false } → bash registered without background controls", () => {
    const tools = toolsWithConfig({ tool_surface: "recommended", bash: { background: false } });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "absent");
  });

  // ---- Legacy experimental bash shape (backward compat) ----------------

  test("legacy rewrite=true only → bash registered without background controls", () => {
    const tools = toolsWithConfig({ experimental: { bash: { rewrite: true } } });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "absent");
  });

  test("legacy compress=true only → bash registered without background controls", () => {
    const tools = toolsWithConfig({ experimental: { bash: { compress: true } } });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "absent");
  });

  test("legacy background=true → full bash surface registered", () => {
    const tools = toolsWithConfig({ experimental: { bash: { background: true } } });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "present");
  });

  test("legacy all flags true → full bash surface registered", () => {
    const tools = toolsWithConfig({
      experimental: { bash: { rewrite: true, compress: true, background: true } },
    });
    expect(tools.bash).toBeDefined();
    expectBackgroundControls(tools, "present");
  });

  test("legacy empty block + tool_surface=minimal → NOT registered", () => {
    // Empty legacy block + minimal surface = no opt-in anywhere, no bash.
    const tools = toolsWithConfig({ tool_surface: "minimal", experimental: { bash: {} } });
    expect(tools.bash).toBeUndefined();
    expectBackgroundControls(tools, "absent");
  });

  // ---- Hoist-off mode --------------------------------------------------

  test("hoist-off + legacy rewrite=true only → aft_bash without background controls", () => {
    const tools = toolsWithConfig(
      { hoist_builtin_tools: false, experimental: { bash: { rewrite: true } } },
      true,
    );
    expect(tools.aft_bash).toBeDefined();
    expect(tools.bash).toBeUndefined();
    expectBackgroundControls(tools, "absent");
  });

  test("hoist-off + bash background enabled → aft_bash plus background controls", () => {
    const tools = toolsWithConfig({ hoist_builtin_tools: false, bash: true }, true);
    expect(tools.aft_bash).toBeDefined();
    expect(tools.bash).toBeUndefined();
    expectBackgroundControls(tools, "present");
  });

  test("hoist-off + bash: false → no bash-family tools registered", () => {
    const tools = toolsWithConfig({ hoist_builtin_tools: false, bash: false }, true);
    expect(tools.aft_bash).toBeUndefined();
    expect(tools.bash).toBeUndefined();
    expectBackgroundControls(tools, "absent");
  });
});
