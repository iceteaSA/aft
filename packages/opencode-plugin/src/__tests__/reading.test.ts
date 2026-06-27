/// <reference path="../bun-test.d.ts" />
/**
 * Unit tests for aft_outline/aft_zoom argument shaping.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { readingTools } from "../tools/reading.js";
import type { PluginContext } from "../types.js";
import { noopAsk, toolResultText } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type ToolCallCall = {
  sessionId: string | undefined;
  name: string;
  rawArgs: Record<string, unknown>;
};
type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

const tempRoots: string[] = [];

async function tempProject(): Promise<string> {
  const root = await realpath(await mkdtemp(join(tmpdir(), "aft-opencode-reading-")));
  tempRoots.push(root);
  return root;
}

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

function createPluginContext(pool: BridgePool): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: {} as PluginContext["config"],
    storageDir: "/tmp/aft-reading-test",
  };
}

function createMockSdkContext(directory: string, ask: ToolContext["ask"] = noopAsk): ToolContext {
  return {
    sessionID: "reading-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function recordingAsk(calls: AskCall[]): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}

function createMockReadingHarness(
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const toolCallCalls: ToolCallCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
    },
    toolCall: async (
      sessionId: string | undefined,
      name: string,
      rawArgs: Record<string, unknown> = {},
    ) => {
      toolCallCalls.push({ sessionId, name, rawArgs });
      return await sendImpl(name, rawArgs);
    },
  };
  const pool = {
    getBridge: () => bridge,
  } as unknown as BridgePool;

  return {
    sendCalls,
    toolCallCalls,
    tools: readingTools(createPluginContext(pool)),
  };
}

afterEach(async () => {
  await Promise.all(tempRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("reading tool adapters", () => {
  test("aft_outline files:true forwards raw target through tool_call and returns server text", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const serverText = [
      "path | language | symbols",
      "",
      "⚠ Partial result: walk truncated at 200 files. 12 additional files in this directory were not indexed.",
      "Unchecked files:",
      "  src/overflow-1.ts",
    ].join("\n");
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: serverText,
      complete: false,
      walk_truncated: true,
      unchecked_files: ["src/overflow-1.ts"],
    }));

    const output = await tools.aft_outline.execute(
      { target: "src", files: true },
      createMockSdkContext(root),
    );

    expect(sendCalls).toEqual([]);
    expect(toolCallCalls).toEqual([
      {
        sessionId: "reading-session",
        name: "outline",
        rawArgs: { target: "src", files: true },
      },
    ]);
    expect(output).toBe(serverText);
  });

  test("aft_outline files:true asks external_directory for an out-of-project directory", async () => {
    const tmpRoot = await tempProject();
    const project = join(tmpRoot, "project");
    const external = join(tmpRoot, "external");
    await mkdir(project, { recursive: true });
    await mkdir(external, { recursive: true });
    const askCalls: AskCall[] = [];
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "external files",
    }));

    await tools.aft_outline.execute(
      { target: external, files: true },
      createMockSdkContext(project, recordingAsk(askCalls)),
    );

    const externalAsks = askCalls.filter((call) => call.permission === "external_directory");
    expect(externalAsks).toHaveLength(1);
    expect(externalAsks[0]?.patterns).toEqual([join(external, "*").replaceAll("\\", "/")]);
    expect(externalAsks[0]?.metadata?.filepath).toBe(external);
    expect(sendCalls).toEqual([]);
    expect(toolCallCalls[0]).toMatchObject({
      sessionId: "reading-session",
      name: "outline",
      rawArgs: { target: external, files: true },
    });
  });

  test("aft_outline files:true target arrays ask once per unique external target", async () => {
    const tmpRoot = await tempProject();
    const project = join(tmpRoot, "project");
    const externalRoot = join(tmpRoot, "external");
    const first = join(externalRoot, "first");
    const second = join(externalRoot, "second");
    await mkdir(project, { recursive: true });
    await mkdir(first, { recursive: true });
    await mkdir(second, { recursive: true });
    const askCalls: AskCall[] = [];
    const { sendCalls, toolCallCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      text: "external files",
    }));

    await tools.aft_outline.execute(
      { target: [first, second], files: true },
      createMockSdkContext(project, recordingAsk(askCalls)),
    );

    const externalAsks = askCalls.filter((call) => call.permission === "external_directory");
    expect(externalAsks).toHaveLength(2);
    expect(externalAsks.map((call) => call.patterns?.[0])).toEqual([
      join(first, "*").replaceAll("\\", "/"),
      join(second, "*").replaceAll("\\", "/"),
    ]);
    expect(sendCalls).toEqual([]);
    expect(toolCallCalls[0]).toMatchObject({
      sessionId: "reading-session",
      name: "outline",
      rawArgs: { target: [first, second], files: true },
    });
  });

  test("aft_zoom targets array fans out one zoom request per entry across different files", async () => {
    const root = await tempProject();
    const { sendCalls, tools } = createMockReadingHarness((_command, params) => {
      const file = params.file as string;
      const symbol = params.symbol as string;
      return {
        success: true,
        name: symbol,
        kind: "function",
        range: { start_line: 10, end_line: 20 },
        content: `// body of ${symbol} from ${file}\n`,
      };
    });

    const result = toolResultText(
      await tools.aft_zoom.execute(
        {
          targets: [
            { filePath: "src/a.ts", symbol: "foo" },
            { filePath: "src/b.ts", symbol: "bar" },
          ],
        },
        createMockSdkContext(root),
      ),
    );

    expect(sendCalls).toHaveLength(2);
    expect(sendCalls[0]?.command).toBe("zoom");
    expect(sendCalls[0]?.params).toMatchObject({ file: join(root, "src/a.ts"), symbol: "foo" });
    expect(sendCalls[1]?.command).toBe("zoom");
    expect(sendCalls[1]?.params).toMatchObject({ file: join(root, "src/b.ts"), symbol: "bar" });
    // Each section uses its OWN filePath as the header label, not a shared one.
    expect(result).toContain("src/a.ts:10-20 [function foo]");
    expect(result).toContain("src/b.ts:10-20 [function bar]");
  });

  test("aft_zoom targets renders per-entry failure with the right file label", async () => {
    const root = await tempProject();
    const { tools } = createMockReadingHarness((_command, params) => {
      if (params.symbol === "missing") {
        return {
          success: false,
          code: "symbol_not_found",
          message: "symbol 'missing' not found",
        };
      }
      return {
        success: true,
        name: params.symbol as string,
        kind: "function",
        range: { start_line: 1, end_line: 2 },
        content: "ok\n",
      };
    });

    const text = toolResultText(
      await tools.aft_zoom.execute(
        {
          targets: [
            { filePath: "src/a.ts", symbol: "ok" },
            { filePath: "src/b.ts", symbol: "missing" },
          ],
        },
        createMockSdkContext(root),
      ),
    );

    expect(text).toContain("Incomplete zoom results");
    expect(text).toContain('Symbol "missing" not found in src/b.ts:');
    expect(text).toContain("src/a.ts:1-2 [function ok]");
  });

  test("aft_zoom targets is mutually exclusive with filePath/symbols/url", async () => {
    const root = await tempProject();
    const { sendCalls, tools } = createMockReadingHarness(() => ({ success: true }));

    await expect(
      tools.aft_zoom.execute(
        {
          targets: [{ filePath: "src/a.ts", symbol: "foo" }],
          filePath: "src/a.ts",
          symbols: "foo",
        },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/mutually exclusive/);
    expect(sendCalls).toHaveLength(0);
  });

  test("aft_zoom treats empty-content targets shapes as not provided (GPT empty-param regression)", async () => {
    const root = await tempProject();
    await Bun.write(`${root}/src/a.ts`, "export function foo() {}\nexport function bar() {}\n");
    const { sendCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      file: "src/a.ts",
      symbol: "foo",
      text: "export function foo() {}",
    }));

    // Array of empty-content entries — GPT models send this shape instead of omitting `targets`.
    // Plus `url: ""` (empty string). Both must be treated as not-provided so filePath wins
    // and we don't get a misleading "targets is mutually exclusive" error.
    await tools.aft_zoom.execute(
      {
        filePath: "src/a.ts",
        url: "",
        targets: [{ filePath: "", symbol: "" }],
        symbols: "foo",
      },
      createMockSdkContext(root),
    );
    expect(sendCalls).toHaveLength(1);

    // Single object form, same all-empty pattern.
    sendCalls.length = 0;
    await tools.aft_zoom.execute(
      {
        filePath: "src/a.ts",
        targets: { filePath: "", symbol: "" },
        symbols: "foo",
      },
      createMockSdkContext(root),
    );
    expect(sendCalls).toHaveLength(1);
  });

  test("aft_zoom string symbols renders Rust batch envelope from one bridge call", async () => {
    const root = await tempProject();
    const { sendCalls, tools } = createMockReadingHarness(() => ({
      success: true,
      complete: true,
      symbols: [
        {
          name: "a",
          response: {
            success: true,
            name: "a",
            kind: "function",
            range: { start_line: 1, end_line: 2 },
            content: "function a() {}\n",
            context_before: [],
            context_after: [],
            annotations: { calls_out: [], called_by: [] },
          },
        },
        {
          name: "b",
          response: {
            success: true,
            name: "b",
            kind: "function",
            range: { start_line: 5, end_line: 6 },
            content: "function b() {}\n",
            context_before: [],
            context_after: [],
            annotations: { calls_out: [], called_by: [] },
          },
        },
      ],
    }));

    const text = toolResultText(
      await tools.aft_zoom.execute(
        { filePath: "src/job.rs", symbols: "a b" },
        createMockSdkContext(root),
      ),
    );

    expect(sendCalls).toHaveLength(1);
    expect(sendCalls[0]?.params).toMatchObject({
      file: join(root, "src/job.rs"),
      symbol: "a b",
    });
    expect(text).toContain("[function a]");
    expect(text).toContain("function a() {}");
    expect(text).toContain("[function b]");
    expect(text).toContain("function b() {}");
    expect(text).not.toContain("Incomplete zoom results");
  });

  test("aft_zoom string symbols shows incomplete framing when Rust batch has a failure", async () => {
    const root = await tempProject();
    const { tools } = createMockReadingHarness(() => ({
      success: true,
      complete: false,
      symbols: [
        {
          name: "a",
          response: {
            success: true,
            name: "a",
            kind: "function",
            range: { start_line: 1, end_line: 1 },
            content: "function a() {}\n",
            context_before: [],
            context_after: [],
            annotations: { calls_out: [], called_by: [] },
          },
        },
        {
          name: "missing",
          response: {
            success: false,
            code: "symbol_not_found",
            message: "symbol 'missing' not found",
          },
        },
      ],
    }));

    const text = toolResultText(
      await tools.aft_zoom.execute(
        { filePath: "src/job.rs", symbols: "a missing" },
        createMockSdkContext(root),
      ),
    );

    expect(text).toContain("Incomplete zoom results");
    expect(text).toContain("function a() {}");
    expect(text).toContain('Symbol "missing" not found:');
  });

  test("aft_zoom targets rejects empty filePath/symbol entries", async () => {
    const root = await tempProject();
    const { sendCalls, tools } = createMockReadingHarness(() => ({ success: true }));

    await expect(
      tools.aft_zoom.execute(
        { targets: [{ filePath: "src/a.ts", symbol: "" }] },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/targets\[0\]\.symbol/);

    await expect(
      tools.aft_zoom.execute(
        { targets: [{ filePath: "", symbol: "x" }] },
        createMockSdkContext(root),
      ),
    ).rejects.toThrow(/targets\[0\]\.filePath/);

    expect(sendCalls).toHaveLength(0);
  });

  test("aft_zoom threads callgraph true to all zoom request shapes and omits it by default", async () => {
    const root = await tempProject();
    const { sendCalls, tools } = createMockReadingHarness((_command, params) => ({
      success: true,
      name: (params.symbol as string | undefined) ?? "lines",
      kind: params.symbol ? "function" : "lines",
      range: { start_line: 1, end_line: 1 },
      content: "ok\n",
    }));

    await tools.aft_zoom.execute(
      { targets: [{ filePath: "src/a.ts", symbol: "foo" }], callgraph: true },
      createMockSdkContext(root),
    );
    await tools.aft_zoom.execute(
      { filePath: "src/a.ts", symbols: ["foo"], callgraph: true },
      createMockSdkContext(root),
    );
    await tools.aft_zoom.execute(
      { filePath: "src/a.ts", callgraph: true },
      createMockSdkContext(root),
    );

    expect(sendCalls.map((call) => call.params)).toEqual([
      expect.objectContaining({ file: join(root, "src/a.ts"), symbol: "foo", callgraph: true }),
      expect.objectContaining({ file: join(root, "src/a.ts"), symbol: "foo", callgraph: true }),
      expect.objectContaining({ file: join(root, "src/a.ts"), callgraph: true }),
    ]);

    sendCalls.length = 0;
    await tools.aft_zoom.execute(
      { filePath: "src/a.ts", symbols: "foo" },
      createMockSdkContext(root),
    );
    expect(sendCalls[0]?.params).toMatchObject({ file: join(root, "src/a.ts"), symbol: "foo" });
    expect(sendCalls[0]?.params).not.toHaveProperty("callgraph");
  });
});
