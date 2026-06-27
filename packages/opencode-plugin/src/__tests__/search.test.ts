/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";
import * as fs from "node:fs";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { searchTools, splitIncludeArg } from "../tools/search.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type BridgeCall = { projectRoot: string };
type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

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

function createPluginContext(pool: BridgePool, config: Record<string, unknown>): PluginContext {
  return {
    pool,
    client: createMockClient(),
    config: config as PluginContext["config"],
    storageDir: "/tmp/aft-test",
  };
}

function createMockSdkContext(
  directory = "/tmp/search-tests",
  ask: ToolContext["ask"] = noopAsk,
): ToolContext {
  return {
    sessionID: "search-session",
    messageID: "message-id",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  };
}

function recordingAsk(
  calls: AskCall[],
  deny?: { permission: string; message: string },
): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
    if (deny && input.permission === deny.permission) {
      throw new Error(deny.message);
    }
  }) as unknown as ToolContext["ask"];
}

function createMockSearchHarness(
  config: Record<string, unknown>,
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse,
) {
  const sendCalls: SendCall[] = [];
  const bridgeCalls: BridgeCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      sendCalls.push({ command, params });
      return await sendImpl(command, params);
    },
  };

  const pool = {
    getBridge: (projectRoot: string) => {
      bridgeCalls.push({ projectRoot });
      return bridge;
    },
  } as unknown as BridgePool;

  return {
    bridgeCalls,
    sendCalls,
    tools: searchTools(createPluginContext(pool, config)),
  };
}

describe("searchTools", () => {
  test("registers hoisted tool names when built-in hoisting is enabled", () => {
    const { tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
    }));

    expect(Object.keys(tools).sort()).toEqual(["glob", "grep"]);
  });

  test("registers aft-prefixed tool names when built-in hoisting is disabled", () => {
    const { tools } = createMockSearchHarness({ hoist_builtin_tools: false }, () => ({
      success: true,
    }));

    expect(Object.keys(tools).sort()).toEqual(["aft_glob", "aft_grep"]);
  });

  test("returns grep response.text when provided and uses session-scoped bridges", async () => {
    const sdkCtx = createMockSdkContext("/tmp/project");
    const { bridgeCalls, sendCalls, tools } = createMockSearchHarness(
      { hoist_builtin_tools: true },
      () => ({
        success: true,
        text: [
          "── src/main.rs (2 matches) ──",
          "  42: fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {",
          "  80: fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {",
          "",
          "Found 2 match across 1 file",
        ].join("\n"),
      }),
    );

    const output = await tools.grep.execute({ pattern: "dispatch" }, sdkCtx);

    // Bridge is project-keyed now; sessionID travels in params via callBridge.
    expect(bridgeCalls.length).toBe(1);
    expect(sendCalls).toHaveLength(1);
    expect(sendCalls[0]?.command).toBe("grep");
    expect(sendCalls[0]?.params).toEqual({
      pattern: "dispatch",
      case_sensitive: true,
      include: undefined,
      path: undefined,
      max_results: 100,
      session_id: "search-session",
    });
    expect(output).toBe(
      [
        "── src/main.rs (2 matches) ──",
        "  42: fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {",
        "  80: fn dispatch(req: RawRequest, ctx: &AppContext) -> Response {",
        "",
        "Found 2 match across 1 file",
      ].join("\n"),
    );
  });

  test("returns glob response.text when provided", async () => {
    const { tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
      text: [
        "21 files matching src/**/*.ts",
        "",
        "src/ (21 files)",
        "  one.ts, two.ts, three.ts, four.ts, five.ts, ...",
      ].join("\n"),
      files: ["src/one.ts", "src/two.ts"],
    }));

    const output = await tools.glob.execute({ pattern: "src/**/*.ts" }, createMockSdkContext());

    expect(output).toBe(
      [
        "21 files matching src/**/*.ts",
        "",
        "src/ (21 files)",
        "  one.ts, two.ts, three.ts, four.ts, five.ts, ...",
      ].join("\n"),
    );
  });

  test("falls back to newline-joined glob paths when text is unavailable", async () => {
    const { tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
      files: ["src/one.ts", "src/two.ts"],
    }));

    const output = await tools.glob.execute({ pattern: "src/**/*.ts" }, createMockSdkContext());

    expect(output).toBe(["src/one.ts", "src/two.ts"].join("\n"));
  });

  test("brace-aware include forwards a single glob with a brace alternation intact (regression)", async () => {
    // Regression: naive String.split(",") used to chop "**/*.{vue,ts}"
    // into ["**/*.{vue", "ts}"], yielding the user-facing
    // "unclosed alternate group; missing '}'" globset error.
    const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
      text: "ok",
    }));
    await tools.grep.execute(
      { pattern: "foo", include: "**/*.{vue,ts,tsx}" },
      createMockSdkContext(),
    );
    expect(sendCalls[0]?.params.include).toEqual(["**/*.{vue,ts,tsx}"]);
  });

  test("brace-aware include preserves brace groups while still splitting top-level commas", async () => {
    const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
      text: "ok",
    }));
    await tools.grep.execute(
      { pattern: "foo", include: "*.ts,**/*.{vue,tsx}" },
      createMockSdkContext(),
    );
    expect(sendCalls[0]?.params.include).toEqual(["**/*.ts", "**/*.{vue,tsx}"]);
  });

  test("backward-compatible: comma-separated includes without braces still split", async () => {
    const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
      text: "ok",
    }));
    await tools.grep.execute({ pattern: "foo", include: "*.tsx,*.ts" }, createMockSdkContext());
    expect(sendCalls[0]?.params.include).toEqual(["**/*.tsx", "**/*.ts"]);
  });

  test("grep checks external permission per parsed multi-path fragment", async () => {
    const tmpRoot = fs.realpathSync(fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-")));
    try {
      const project = path.join(tmpRoot, "project");
      const inside = path.join(project, "src");
      const external = path.join(tmpRoot, "external");
      fs.mkdirSync(inside, { recursive: true });
      fs.mkdirSync(external, { recursive: true });
      const askCalls: AskCall[] = [];
      const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
        success: true,
        text: "ok",
      }));

      await tools.grep.execute(
        { pattern: "TODO", path: `${inside} ${external}` },
        createMockSdkContext(project, recordingAsk(askCalls)),
      );

      const externalAsks = askCalls.filter((call) => call.permission === "external_directory");
      expect(externalAsks).toHaveLength(1);
      expect(externalAsks[0]?.patterns).toEqual([path.join(external, "*").replaceAll("\\", "/")]);
      expect(externalAsks[0]?.metadata?.filepath).toBe(external);
      expect(sendCalls[0]?.params.path).toBe(`${inside} ${external}`);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("glob rejects when any parsed multi-path fragment is externally denied", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const inside = path.join(project, "src");
      const external = path.join(tmpRoot, "external");
      fs.mkdirSync(inside, { recursive: true });
      fs.mkdirSync(external, { recursive: true });
      const askCalls: AskCall[] = [];
      const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
        success: true,
        files: [],
      }));

      const raw = await tools.glob.execute(
        { pattern: "**/*.ts", path: `${inside} ${external}` },
        createMockSdkContext(
          project,
          recordingAsk(askCalls, {
            permission: "external_directory",
            message: "external denied",
          }),
        ),
      );

      expect(JSON.parse(raw).message).toBe("external denied");
      expect(askCalls.filter((call) => call.permission === "external_directory")).toHaveLength(1);
      expect(sendCalls).toHaveLength(0);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("grep searches existing fragments and reports skipped missing paths", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const src = path.join(project, "src");
      const missing = path.join(project, "test");
      fs.mkdirSync(src, { recursive: true });
      const bridgeResponse = {
        success: true,
        complete: true,
        text: "src/hit.ts:1: const value = 'needle';\n\nFound 1 match across 1 file",
      };
      const { sendCalls, tools } = createMockSearchHarness(
        { hoist_builtin_tools: true },
        () => bridgeResponse,
      );

      const output = await tools.grep.execute(
        { pattern: "needle", path: `${src} ${missing}` },
        createMockSdkContext(project),
      );

      expect(sendCalls[0]?.params.path).toBe(src);
      expect(output).toContain("src/hit.ts:1");
      expect(output).toContain(`Skipped 1 path not found: ${missing}`);
      expect(bridgeResponse.complete).toBe(false);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("grep keeps all-valid multi-path searches complete", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const src = path.join(project, "src");
      const e2e = path.join(project, "e2e");
      fs.mkdirSync(src, { recursive: true });
      fs.mkdirSync(e2e, { recursive: true });
      const bridgeResponse = { success: true, complete: true, text: "ok" };
      const { sendCalls, tools } = createMockSearchHarness(
        { hoist_builtin_tools: true },
        () => bridgeResponse,
      );

      const output = await tools.grep.execute(
        { pattern: "needle", path: `${src} ${e2e}` },
        createMockSdkContext(project),
      );

      expect(sendCalls[0]?.params.path).toBe(`${src} ${e2e}`);
      expect(output).toBe("ok");
      expect(output).not.toContain("Skipped");
      expect(bridgeResponse.complete).toBe(true);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("grep falls through to path_not_found when every fragment is missing", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      fs.mkdirSync(project, { recursive: true });
      const missingA = path.join(project, "missing-a");
      const missingB = path.join(project, "missing-b");
      const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
        success: false,
        code: "path_not_found",
        message: "path_not_found",
      }));

      let thrown: unknown;
      try {
        await tools.grep.execute(
          { pattern: "needle", path: `${missingA} ${missingB}` },
          createMockSdkContext(project),
        );
      } catch (error) {
        thrown = error;
      }

      expect(thrown).toBeInstanceOf(Error);
      expect((thrown as Error).message).toBe("path_not_found");
      expect(sendCalls[0]?.params.path).toBe(`${missingA} ${missingB}`);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("grep treats an existing single path containing a space as one path", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const spaced = path.join(project, "with space");
      fs.mkdirSync(spaced, { recursive: true });
      const bridgeResponse = { success: true, complete: true, text: "ok" };
      const { sendCalls, tools } = createMockSearchHarness(
        { hoist_builtin_tools: true },
        () => bridgeResponse,
      );

      const output = await tools.grep.execute(
        { pattern: "needle", path: "with space" },
        createMockSdkContext(project),
      );

      expect(sendCalls[0]?.params.path).toBe(fs.realpathSync(spaced));
      expect(output).toBe("ok");
      expect(output).not.toContain("Skipped");
      expect(bridgeResponse.complete).toBe(true);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("glob reports skipped missing fragments while searching existing paths", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const src = path.join(project, "src");
      const missing = path.join(project, "test");
      fs.mkdirSync(src, { recursive: true });
      const bridgeResponse = { success: true, complete: true, text: "src/hit.ts" };
      const { sendCalls, tools } = createMockSearchHarness(
        { hoist_builtin_tools: true },
        () => bridgeResponse,
      );

      const output = await tools.glob.execute(
        { pattern: "**/*.ts", path: `${src} ${missing}` },
        createMockSdkContext(project),
      );

      expect(sendCalls[0]?.params.path).toBe(src);
      expect(output).toContain("src/hit.ts");
      expect(output).toContain(`Skipped 1 path not found: ${missing}`);
      expect(bridgeResponse.complete).toBe(false);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("glob keeps all-valid multi-path searches complete", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const src = path.join(project, "src");
      const e2e = path.join(project, "e2e");
      fs.mkdirSync(src, { recursive: true });
      fs.mkdirSync(e2e, { recursive: true });
      const bridgeResponse = { success: true, complete: true, text: "src/a.ts\ne2e/b.ts" };
      const { sendCalls, tools } = createMockSearchHarness(
        { hoist_builtin_tools: true },
        () => bridgeResponse,
      );

      const output = await tools.glob.execute(
        { pattern: "**/*.ts", path: `${src} ${e2e}` },
        createMockSdkContext(project),
      );

      expect(sendCalls[0]?.params.path).toBe(`${src} ${e2e}`);
      expect(output).toBe("src/a.ts\ne2e/b.ts");
      expect(output).not.toContain("Skipped");
      expect(bridgeResponse.complete).toBe(true);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("glob falls through to path_not_found when every fragment is missing", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      fs.mkdirSync(project, { recursive: true });
      const missingA = path.join(project, "missing-a");
      const missingB = path.join(project, "missing-b");
      const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
        success: false,
        code: "path_not_found",
        message: "path_not_found",
      }));

      let thrown: unknown;
      try {
        await tools.glob.execute(
          { pattern: "**/*.ts", path: `${missingA} ${missingB}` },
          createMockSdkContext(project),
        );
      } catch (error) {
        thrown = error;
      }

      expect(thrown).toBeInstanceOf(Error);
      expect((thrown as Error).message).toBe("path_not_found");
      expect(sendCalls[0]?.params.path).toBe(`${missingA} ${missingB}`);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("glob treats an existing single path containing a space as one path", async () => {
    const tmpRoot = fs.mkdtempSync(path.join(tmpdir(), "aft-search-plugin-"));
    try {
      const project = path.join(tmpRoot, "project");
      const spaced = path.join(project, "with space");
      fs.mkdirSync(spaced, { recursive: true });
      const bridgeResponse = { success: true, complete: true, text: "with space/a.ts" };
      const { sendCalls, tools } = createMockSearchHarness(
        { hoist_builtin_tools: true },
        () => bridgeResponse,
      );

      const output = await tools.glob.execute(
        { pattern: "**/*.ts", path: "with space" },
        createMockSdkContext(project),
      );

      expect(sendCalls[0]?.params.path).toBe(fs.realpathSync(spaced));
      expect(output).toBe("with space/a.ts");
      expect(output).not.toContain("Skipped");
      expect(bridgeResponse.complete).toBe(true);
    } finally {
      fs.rmSync(tmpRoot, { recursive: true, force: true });
    }
  });

  test("glob splits exact absolute file patterns into path and basename", async () => {
    const project = "/tmp/search-tests";
    const absoluteFile = path.join(project, "src", "exact.ts");
    const { sendCalls, tools } = createMockSearchHarness({ hoist_builtin_tools: true }, () => ({
      success: true,
      files: [absoluteFile],
    }));

    await tools.glob.execute({ pattern: absoluteFile }, createMockSdkContext(project));

    expect(sendCalls[0]?.params).toMatchObject({
      pattern: "exact.ts",
      path: path.dirname(absoluteFile),
    });
  });
});

describe("splitIncludeArg", () => {
  test("splits plain comma-separated patterns", () => {
    expect(splitIncludeArg("*.ts,*.tsx")).toEqual(["*.ts", "*.tsx"]);
  });

  test("preserves a single brace group as one pattern", () => {
    expect(splitIncludeArg("**/*.{vue,ts,tsx}")).toEqual(["**/*.{vue,ts,tsx}"]);
  });

  test("splits top-level commas while preserving nested brace groups", () => {
    expect(splitIncludeArg("*.ts,**/*.{vue,tsx},*.go")).toEqual(["*.ts", "**/*.{vue,tsx}", "*.go"]);
  });

  test("handles nested braces correctly", () => {
    expect(splitIncludeArg("**/*.{a,{b,c},d}")).toEqual(["**/*.{a,{b,c},d}"]);
  });

  test("trims whitespace and drops empty segments", () => {
    expect(splitIncludeArg(" *.ts , *.tsx , ")).toEqual(["*.ts", "*.tsx"]);
  });

  test("tolerates an unclosed brace by treating remaining commas as content (no crash)", () => {
    // Unmatched '{' shouldn't throw — pattern is forwarded to the backend
    // as one chunk so globset's own error surfaces, not a JS crash here.
    expect(splitIncludeArg("**/*.{vue,ts")).toEqual(["**/*.{vue,ts"]);
  });
});
