/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import * as path from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";

import { _resetSessionDirectoryCacheForTest } from "../shared/session-directory.js";
import { astTools } from "../tools/ast.js";
import { createBashTool } from "../tools/bash.js";
import { hoistedTools } from "../tools/hoisted.js";
import { importTools } from "../tools/imports.js";
import {
  _permissionsInternalsForTest,
  assertExternalDirectoryPermission,
} from "../tools/permissions.js";
import { safetyTools } from "../tools/safety.js";
import { searchTools } from "../tools/search.js";
import type { PluginContext } from "../types.js";

type BridgeResponse = Record<string, unknown>;
type SendCall = { command: string; params: Record<string, unknown> };
type AskCall = {
  permission?: string;
  patterns?: string[];
  always?: string[];
  metadata?: Record<string, unknown>;
};

type PermissionAskFrame = {
  kind: "external_directory" | "bash";
  patterns: string[];
  always: string[];
};

const windowsTest = process.platform === "win32" ? test : test.skip;
let tmpRoot: string | null = null;

afterEach(async () => {
  if (tmpRoot) {
    await rm(tmpRoot, { recursive: true, force: true });
    tmpRoot = null;
  }
  _resetSessionDirectoryCacheForTest();
});

function createMockClient(): any {
  return {
    lsp: { status: async () => ({ data: [] }) },
    find: { symbols: async () => ({ data: [] }) },
  };
}

function createPluginContext(pool: BridgePool, client: any = createMockClient()): PluginContext {
  return { pool, client, config: {} as any, storageDir: "/tmp/aft-test" };
}

function createHarness(
  toolFactory: (ctx: PluginContext) => Record<string, ToolDefinition>,
  sendImpl: (
    command: string,
    params: Record<string, unknown>,
  ) => Promise<BridgeResponse> | BridgeResponse = () => ({ success: true, text: "ok" }),
) {
  const calls: SendCall[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      calls.push({ command, params });
      return await sendImpl(command, params);
    },
  };
  const pool = { getBridge: () => bridge } as unknown as BridgePool;
  return { calls, tools: toolFactory(createPluginContext(pool)) };
}

function createBashPermissionHarness(asks: PermissionAskFrame[]) {
  return createHarness(
    (ctx) => ({ bash: createBashTool(ctx) }),
    (_command, params) => {
      if (!("permissions_granted" in params)) {
        return { success: false, code: "permission_required", asks };
      }
      return { success: true, output: "ok\n", exit_code: 0 };
    },
  );
}

function createSdkContext(
  directory: string,
  ask: ToolContext["ask"],
  sessionID = "permission-audit-test",
): ToolContext {
  return {
    sessionID,
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

async function makeProjectAndExternalDirs(): Promise<{ project: string; external: string }> {
  tmpRoot = await realpath(await mkdtemp(path.join(tmpdir(), "aft-permission-audit-")));
  const project = path.join(tmpRoot, "project");
  const external = path.join(tmpRoot, "external");
  await mkdir(project, { recursive: true });
  await mkdir(external, { recursive: true });
  return { project, external };
}

async function makeCanonicalizationDirs(): Promise<{
  project: string;
  external: string;
  inRootTarget: string;
}> {
  tmpRoot = await realpath(await mkdtemp(path.join(tmpdir(), "aft-permission-canon-")));
  const project = path.join(tmpRoot, "project");
  const external = path.join(tmpRoot, "external");
  const inRootTarget = path.join(project, "real-target");
  await mkdir(inRootTarget, { recursive: true });
  await mkdir(external, { recursive: true });
  return { project, external, inRootTarget };
}

async function createDirectoryLink(target: string, linkPath: string): Promise<void> {
  await symlink(target, linkPath, process.platform === "win32" ? "junction" : "dir");
}

async function externalAskCallsFor(project: string, target: string): Promise<AskCall[]> {
  const askCalls: AskCall[] = [];
  const context = createSdkContext(project, recordingAsk(askCalls));
  const ctx = createPluginContext({ getBridge: () => ({}) } as unknown as BridgePool);

  const result = await assertExternalDirectoryPermission(ctx, context, target);

  expect(result).toBeUndefined();
  return askCalls.filter((call) => call.permission === "external_directory");
}

function expectExternalAsk(calls: AskCall[]): void {
  expect(calls).toHaveLength(1);
  expect(calls[0]?.permission).toBe("external_directory");
}

function parsePermissionDenied(raw: string): Record<string, unknown> {
  const parsed = JSON.parse(raw) as Record<string, unknown>;
  expect(parsed.success).toBe(false);
  expect(parsed.code).toBe("permission_denied");
  return parsed;
}

describe("permission audit regressions", () => {
  test("bash permission loop groups multi-bash pipeline asks into one prompt", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const { calls, tools } = createBashPermissionHarness([
      { kind: "bash", patterns: ["cat a"], always: ["cat *"] },
      { kind: "bash", patterns: ["grep b", "grep b"], always: ["grep *"] },
      { kind: "bash", patterns: ["wc -l"], always: ["wc *"] },
    ]);

    await tools.bash.execute(
      { command: "cat a | grep b | wc -l" },
      createSdkContext(project, recordingAsk(askCalls)),
    );

    expect(askCalls).toEqual([
      {
        permission: "bash",
        patterns: ["cat a", "grep b", "wc -l"],
        always: ["cat *", "grep *", "wc *"],
        metadata: {},
      },
    ]);
    expect(calls).toHaveLength(2);
    expect(calls[1]?.params.permissions_granted).toEqual(["cat *", "grep *", "wc *"]);
  });

  test("bash permission loop keeps external_directory asks separate from grouped bash asks", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const { calls, tools } = createBashPermissionHarness([
      { kind: "bash", patterns: ["cat a"], always: ["cat *"] },
      { kind: "external_directory", patterns: ["/tmp/aft-one/*"], always: [] },
      { kind: "bash", patterns: ["grep b"], always: ["grep *"] },
      {
        kind: "external_directory",
        patterns: ["/tmp/aft-two/*"],
        always: ["/tmp/aft-two/*"],
      },
    ]);

    await tools.bash.execute(
      { command: "cat a | grep b > /tmp/aft-one/out && touch /tmp/aft-two/done" },
      createSdkContext(project, recordingAsk(askCalls)),
    );

    expect(askCalls).toEqual([
      {
        permission: "bash",
        patterns: ["cat a", "grep b"],
        always: ["cat *", "grep *"],
        metadata: {},
      },
      {
        permission: "external_directory",
        patterns: ["/tmp/aft-one/*"],
        always: [],
        metadata: {},
      },
      {
        permission: "external_directory",
        patterns: ["/tmp/aft-two/*"],
        always: ["/tmp/aft-two/*"],
        metadata: {},
      },
    ]);
    expect(calls).toHaveLength(2);
    expect(calls[1]?.params.permissions_granted).toEqual([
      "cat *",
      "/tmp/aft-one/*",
      "grep *",
      "/tmp/aft-two/*",
    ]);
  });

  test("bash permission retry preserves original always-or-pattern grants", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const { calls, tools } = createBashPermissionHarness([
      { kind: "bash", patterns: ["custom-run --flag"], always: [] },
      { kind: "bash", patterns: ["git status"], always: ["git status *"] },
      { kind: "external_directory", patterns: ["/tmp/aft-empty-always/*"], always: [] },
    ]);

    await tools.bash.execute(
      { command: "custom-run --flag | git status > /tmp/aft-empty-always/out" },
      createSdkContext(project, recordingAsk(askCalls)),
    );

    expect(askCalls.map((call) => call.permission)).toEqual(["bash", "external_directory"]);
    expect(calls).toHaveLength(2);
    expect(calls[1]?.params.permissions_granted).toEqual([
      "custom-run --flag",
      "git status *",
      "/tmp/aft-empty-always/*",
    ]);
  });

  test("aft_import rejects empty module sentinels before bridge dispatch", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const { calls, tools } = createHarness(importTools);

    await expect(
      tools.aft_import.execute(
        { op: "add", filePath: "src/app.ts", module: "" },
        createSdkContext(project, recordingAsk([])),
      ),
    ).rejects.toThrow("'module' is required for 'add' op");
    expect(calls).toHaveLength(0);
  });

  test("relative import paths use the session project root for both approval and bridge dispatch", async () => {
    tmpRoot = await realpath(await mkdtemp(path.join(tmpdir(), "aft-path-parity-")));
    const project = path.join(tmpRoot, "project");
    const launchCwd = path.join(tmpRoot, "launch-cwd");
    await mkdir(path.join(project, "src"), { recursive: true });
    await mkdir(launchCwd, { recursive: true });

    const askCalls: AskCall[] = [];
    const calls: SendCall[] = [];
    const bridgeRoots: string[] = [];
    const bridge = {
      send: async (command: string, params: Record<string, unknown> = {}) => {
        calls.push({ command, params });
        return { success: true };
      },
    };
    const pool = {
      getBridge: (cwd: string) => {
        bridgeRoots.push(cwd);
        return bridge;
      },
    } as unknown as BridgePool;
    const client = {
      ...createMockClient(),
      session: {
        get: async () => ({ data: { directory: project } }),
      },
    };
    const sdkCtx = {
      ...createSdkContext(launchCwd, recordingAsk(askCalls)),
      sessionID: "resume-session-path-parity",
      worktree: launchCwd,
    } as ToolContext;
    const tools = importTools(createPluginContext(pool, client));

    await tools.aft_import.execute({ op: "organize", filePath: "src/app.ts" }, sdkCtx);

    const expectedFile = path.join(project, "src/app.ts");
    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(expectedFile);
    expect(calls[0]).toMatchObject({
      command: "organize_imports",
      params: { file: expectedFile },
    });
    expect(bridgeRoots[0]).toBe(project);
  });

  test("hoisted write paths use the session project root for approval and bridge dispatch", async () => {
    tmpRoot = await realpath(await mkdtemp(path.join(tmpdir(), "aft-hoisted-path-parity-")));
    const project = path.join(tmpRoot, "project");
    const launchCwd = path.join(tmpRoot, "launch-cwd");
    await mkdir(path.join(project, "src"), { recursive: true });
    await mkdir(launchCwd, { recursive: true });

    const askCalls: AskCall[] = [];
    const calls: SendCall[] = [];
    const bridgeRoots: string[] = [];
    const bridge = {
      send: async (command: string, params: Record<string, unknown> = {}) => {
        calls.push({ command, params });
        return { success: true, created: true };
      },
    };
    const pool = {
      getBridge: (cwd: string) => {
        bridgeRoots.push(cwd);
        return bridge;
      },
    } as unknown as BridgePool;
    const client = {
      ...createMockClient(),
      session: {
        get: async () => ({ data: { directory: project } }),
      },
    };
    const sdkCtx = {
      ...createSdkContext(launchCwd, recordingAsk(askCalls)),
      sessionID: "resume-hoisted-path-parity",
      worktree: launchCwd,
    } as ToolContext;
    const tools = hoistedTools(createPluginContext(pool, client));

    await tools.write.execute(
      { filePath: "src/app.ts", content: "export const ok = true;\n" },
      sdkCtx,
    );

    const expectedFile = path.join(project, "src/app.ts");
    const editAsk = askCalls.find((call) => call.permission === "edit");
    expect(editAsk?.metadata?.filepath).toBe(expectedFile);
    expect(editAsk?.patterns).toEqual(["src/app.ts"]);
    expect(calls[0]).toMatchObject({
      command: "write",
      params: { file: expectedFile },
    });
    expect(bridgeRoots[0]).toBe(project);
  });

  test("external_directory ask is skipped for an in-root real file", async () => {
    const { project } = await makeCanonicalizationDirs();
    const target = path.join(project, "real-target", "file.txt");
    await writeFile(target, "inside\n");

    const externalAsks = await externalAskCallsFor(project, target);

    expect(externalAsks).toHaveLength(0);
  });

  test("external_directory ask fires for an in-root symlink to an outside target", async () => {
    const { project, external } = await makeCanonicalizationDirs();
    const linkPath = path.join(project, "link-outside");
    const target = path.join(linkPath, "secret.txt");
    await writeFile(path.join(external, "secret.txt"), "outside\n");
    await createDirectoryLink(external, linkPath);

    const externalAsks = await externalAskCallsFor(project, target);

    expectExternalAsk(externalAsks);
  });

  test("external_directory ask is skipped for an in-root symlink to an in-root target", async () => {
    const { project, inRootTarget } = await makeCanonicalizationDirs();
    const linkPath = path.join(project, "link-inside");
    const target = path.join(linkPath, "safe.txt");
    await writeFile(path.join(inRootTarget, "safe.txt"), "inside\n");
    await createDirectoryLink(inRootTarget, linkPath);

    const externalAsks = await externalAskCallsFor(project, target);

    expect(externalAsks).toHaveLength(0);
  });

  test("external_directory ask fires for a nonexistent write under a symlinked-outside parent", async () => {
    const { project, external } = await makeCanonicalizationDirs();
    const linkPath = path.join(project, "link-outside");
    await createDirectoryLink(external, linkPath);

    const externalAsks = await externalAskCallsFor(project, path.join(linkPath, "new-file.txt"));

    expectExternalAsk(externalAsks);
  });

  test("external_directory ask fires for a plain outside-root path", async () => {
    const { project, external } = await makeCanonicalizationDirs();
    const target = path.join(external, "plain.txt");
    await writeFile(target, "outside\n");

    const externalAsks = await externalAskCallsFor(project, target);

    expectExternalAsk(externalAsks);
  });

  test("external_directory ask expands ~/ before containment", async () => {
    const { project } = await makeCanonicalizationDirs();

    const externalAsks = await externalAskCallsFor(
      project,
      "~/aft-permission-home-target-for-test.txt",
    );

    expectExternalAsk(externalAsks);
    const filepath = externalAsks[0]?.metadata?.filepath;
    expect(typeof filepath).toBe("string");
    expect((filepath as string).startsWith(path.join(project, "~"))).toBe(false);
    expect(filepath).toContain("aft-permission-home-target-for-test.txt");
  });

  windowsTest("containsPath rejects Windows cross-drive targets as external", async () => {
    const askCalls: AskCall[] = [];
    const context = createSdkContext("C:\\repo", recordingAsk(askCalls));
    const ctx = createPluginContext({ getBridge: () => ({}) } as unknown as BridgePool);

    await assertExternalDirectoryPermission(ctx, context, "D:\\secret\\file.ts");

    expect(askCalls).toHaveLength(1);
    expect(askCalls[0]?.permission).toBe("external_directory");
    expect(askCalls[0]?.patterns?.[0]).toContain("D:");
  });

  test("restrict_to_project_root blocks external paths without bubbling an ask", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const promptCalls: unknown[] = [];
    const context = createSdkContext(project, recordingAsk(askCalls), "restrict-block-sess");
    const client = {
      ...createMockClient(),
      session: { prompt: (input: unknown) => promptCalls.push(input) },
    };
    const ctx = createPluginContext({ getBridge: () => ({}) } as unknown as BridgePool, client);
    ctx.config = { restrict_to_project_root: true } as PluginContext["config"];

    const denial = await assertExternalDirectoryPermission(
      ctx,
      context,
      path.join(external, "secret.txt"),
    );

    // Blocked + agent-facing denial, and NO external_directory prompt bubbled.
    expect(typeof denial).toBe("string");
    expect(denial).toContain("restrict_to_project_root");
    expect(askCalls).toHaveLength(0);
    // User-facing ignored panel fired once.
    expect(promptCalls).toHaveLength(1);
  });

  test("restrict_to_project_root notice is throttled to once per session", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const context = createSdkContext(project, recordingAsk([]), "restrict-throttle-sess");
    const promptCalls: unknown[] = [];
    const client = {
      ...createMockClient(),
      session: { prompt: (input: unknown) => promptCalls.push(input) },
    };
    const ctx = createPluginContext({ getBridge: () => ({}) } as unknown as BridgePool, client);
    ctx.config = { restrict_to_project_root: true } as PluginContext["config"];

    const a = await assertExternalDirectoryPermission(ctx, context, path.join(external, "a.txt"));
    const b = await assertExternalDirectoryPermission(ctx, context, path.join(external, "b.txt"));

    // Both blocked (agent always informed), but the user panel fires once.
    expect(typeof a).toBe("string");
    expect(typeof b).toBe("string");
    expect(promptCalls).toHaveLength(1);
  });

  test("restrict_to_project_root allows in-root paths untouched", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const context = createSdkContext(project, recordingAsk(askCalls));
    const ctx = createPluginContext({ getBridge: () => ({}) } as unknown as BridgePool);
    ctx.config = { restrict_to_project_root: true } as PluginContext["config"];

    const denial = await assertExternalDirectoryPermission(
      ctx,
      context,
      path.join(project, "in-root.txt"),
    );

    expect(denial).toBeUndefined();
    expect(askCalls).toHaveLength(0);
  });

  test("restrict false (default) still bubbles the external_directory ask", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const context = createSdkContext(project, recordingAsk(askCalls));
    const ctx = createPluginContext({ getBridge: () => ({}) } as unknown as BridgePool);
    ctx.config = { restrict_to_project_root: false } as PluginContext["config"];

    const denial = await assertExternalDirectoryPermission(
      ctx,
      context,
      path.join(external, "ok.txt"),
    );

    // Grant path: ask bubbled, no denial.
    expect(denial).toBeUndefined();
    expect(askCalls).toHaveLength(1);
    expect(askCalls[0]?.permission).toBe("external_directory");
  });

  windowsTest(
    "normalizePathPattern preserves single-star and globstar Windows patterns",
    async () => {
      tmpRoot = await mkdtemp(path.join(tmpdir(), "aft-win-pattern-"));
      const normalizedSingle = _permissionsInternalsForTest.normalizePathPattern(`${tmpRoot}\\*`);
      const normalizedGlobstar = _permissionsInternalsForTest.normalizePathPattern(
        `${tmpRoot}\\**`,
      );

      expect(normalizedSingle).toBe(
        path.join(_permissionsInternalsForTest.normalizePathPattern(tmpRoot), "*"),
      );
      expect(normalizedGlobstar).toBe(
        path.join(_permissionsInternalsForTest.normalizePathPattern(tmpRoot), "**"),
      );
    },
  );

  test("ast_grep_replace edit denial returns the permissionDeniedResponse envelope", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(
      project,
      recordingAsk(askCalls, { permission: "edit", message: "edit denied by policy" }),
    );
    const { calls, tools } = createHarness(astTools);

    const raw = (await tools.ast_grep_replace.execute(
      {
        pattern: "console.log($MSG)",
        rewrite: "logger.info($MSG)",
        lang: "javascript",
        paths: ["."],
      },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("edit denied by policy");
    expect(calls).toHaveLength(0);
  });

  test("aft_safety checkpoint asks for explicit external files once per parent", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, () => ({ success: true, name: "snap" }));

    const raw = (await tools.aft_safety.execute(
      {
        op: "checkpoint",
        name: "snap",
        files: [path.join(external, "a.ts"), path.join(external, "b.ts")],
      },
      sdkCtx,
    )) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.filter((call) => call.permission === "external_directory")).toHaveLength(1);
    expect(calls[0]?.command).toBe("checkpoint");
  });

  test("aft_safety checkpoint preflight warms session root before resolving relative files", async () => {
    tmpRoot = await realpath(await mkdtemp(path.join(tmpdir(), "aft-checkpoint-path-parity-")));
    const project = path.join(tmpRoot, "project");
    const launchCwd = path.join(project, "subdir");
    await mkdir(launchCwd, { recursive: true });

    const askCalls: AskCall[] = [];
    const calls: SendCall[] = [];
    const bridgeRoots: string[] = [];
    const bridge = {
      send: async (command: string, params: Record<string, unknown> = {}) => {
        calls.push({ command, params });
        return { success: true, name: "snap" };
      },
    };
    const pool = {
      getBridge: (cwd: string) => {
        bridgeRoots.push(cwd);
        return bridge;
      },
    } as unknown as BridgePool;
    const client = {
      ...createMockClient(),
      session: {
        get: async () => ({ data: { directory: project } }),
      },
    };
    const sdkCtx = {
      ...createSdkContext(launchCwd, recordingAsk(askCalls)),
      sessionID: "resume-checkpoint-path-parity",
      worktree: launchCwd,
    } as ToolContext;
    const tools = safetyTools(createPluginContext(pool, client));

    const raw = (await tools.aft_safety.execute(
      { op: "checkpoint", name: "snap", files: ["../outside.ts"] },
      sdkCtx,
    )) as string;

    const expectedApprovedPath = path.resolve(project, "../outside.ts");
    const externalAsk = askCalls.find((call) => call.permission === "external_directory");
    expect(JSON.parse(raw).success).toBe(true);
    expect(externalAsk?.metadata?.filepath).toBe(expectedApprovedPath);
    expect(calls[0]).toMatchObject({
      command: "checkpoint",
      params: { files: ["../outside.ts"] },
    });
    expect(bridgeRoots[0]).toBe(project);
  });

  test("aft_safety checkpoint external denial returns the permissionDeniedResponse envelope", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(
      project,
      recordingAsk(askCalls, { permission: "external_directory", message: "external denied" }),
    );
    const { calls, tools } = createHarness(safetyTools);

    const raw = (await tools.aft_safety.execute(
      { op: "checkpoint", name: "snap", files: [path.join(external, "a.ts")] },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("external denied");
    expect(askCalls[0]?.permission).toBe("external_directory");
    expect(calls).toHaveLength(0);
  });

  test("aft_safety undo still asks edit permission and calls the bridge", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "undo_preview"
        ? { success: true, paths: [path.join(project, "inside.ts")] }
        : { success: true, backup_id: "b1" },
    );

    const raw = (await tools.aft_safety.execute(
      { op: "undo", filePath: "inside.ts" },
      sdkCtx,
    )) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.map((call) => call.permission)).toEqual(["edit"]);
    expect(calls.map((call) => call.command)).toEqual(["undo_preview", "undo"]);
  });

  test("aft_safety undo without filePath previews, asks edit, then calls bridge without file param", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const canonicalProject = await realpath(project);
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "undo_preview"
        ? {
            success: true,
            paths: [
              path.join(canonicalProject, "inside.ts"),
              path.join(canonicalProject, "inside.ts"),
            ],
          }
        : { success: true, operation: true },
    );

    const raw = (await tools.aft_safety.execute({ op: "undo" }, sdkCtx)) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.map((call) => call.permission)).toEqual(["edit"]);
    expect(askCalls[0]?.patterns).toEqual(["inside.ts"]);
    expect(calls.map((call) => call.command)).toEqual(["undo_preview", "undo"]);
    expect(calls[0]?.params).not.toHaveProperty("file");
    expect(calls[1]?.params).not.toHaveProperty("file");
  });

  test("aft_safety undo without filePath stops before undo when edit permission is denied", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(
      project,
      recordingAsk(askCalls, { permission: "edit", message: "edit denied by policy" }),
    );
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "undo_preview"
        ? { success: true, paths: [path.join(project, "inside.ts")] }
        : { success: true, operation: true },
    );

    const raw = (await tools.aft_safety.execute({ op: "undo" }, sdkCtx)) as string;

    expect(parsePermissionDenied(raw).message).toBe("edit denied by policy");
    expect(askCalls.map((call) => call.permission)).toEqual(["edit"]);
    expect(calls.map((call) => call.command)).toEqual(["undo_preview"]);
  });

  test("aft_safety undo with filePath previews and still passes file param", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(project, recordingAsk([]));
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "undo_preview"
        ? { success: true, paths: [path.join(project, "inside.ts")] }
        : { success: true, backup_id: "b1" },
    );

    await tools.aft_safety.execute({ op: "undo", filePath: "inside.ts" }, sdkCtx);

    expect(calls[0]?.command).toBe("undo_preview");
    expect(calls[0]?.params).toMatchObject({ file: "inside.ts" });
    expect(calls[1]?.command).toBe("undo");
    expect(calls[1]?.params).toMatchObject({ file: "inside.ts" });
  });

  test("aft_safety undo preflights external paths returned by preview", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const externalFile = path.join(external, "undo.ts");
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "undo_preview"
        ? { success: true, paths: [externalFile, externalFile] }
        : { success: true, operation: true },
    );

    const raw = (await tools.aft_safety.execute({ op: "undo" }, sdkCtx)) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.filter((call) => call.permission === "external_directory")).toHaveLength(1);
    expect(calls.map((call) => call.command)).toEqual(["undo_preview", "undo"]);
  });

  test("aft_safety undo preview internal paths do not ask external_directory", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "undo_preview"
        ? { success: true, paths: [path.join(project, "undo.ts")] }
        : { success: true, operation: true },
    );

    await tools.aft_safety.execute({ op: "undo" }, sdkCtx);

    expect(askCalls.some((call) => call.permission === "external_directory")).toBe(false);
    expect(calls.map((call) => call.command)).toEqual(["undo_preview", "undo"]);
  });

  test("aft_safety restore preflights external checkpoint paths", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const externalFile = path.join(external, "restore.ts");
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "checkpoint_paths"
        ? { success: true, paths: [externalFile] }
        : { success: true, name: "snap" },
    );

    const raw = (await tools.aft_safety.execute({ op: "restore", name: "snap" }, sdkCtx)) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.filter((call) => call.permission === "external_directory")).toHaveLength(1);
    expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths", "restore_checkpoint"]);
  });

  test("aft_safety restore internal checkpoint paths do not ask external_directory", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { calls, tools } = createHarness(safetyTools, (command) =>
      command === "checkpoint_paths"
        ? { success: true, paths: [path.join(project, "restore.ts")] }
        : { success: true, name: "snap" },
    );

    await tools.aft_safety.execute({ op: "restore", name: "snap" }, sdkCtx);

    expect(askCalls.some((call) => call.permission === "external_directory")).toBe(false);
    expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths", "restore_checkpoint"]);
  });

  test("aft_safety checkpoint checks a single external filePath", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const externalFile = path.join(external, "single.ts");
    const { calls, tools } = createHarness(safetyTools, () => ({ success: true, name: "snap" }));

    const raw = (await tools.aft_safety.execute(
      { op: "checkpoint", name: "snap", filePath: externalFile },
      sdkCtx,
    )) as string;

    expect(JSON.parse(raw).success).toBe(true);
    expect(askCalls.filter((call) => call.permission === "external_directory")).toHaveLength(1);
    expect(calls[0]?.command).toBe("checkpoint");
    expect(calls[0]?.params).toMatchObject({ files: [externalFile] });
  });

  test("ast_grep_search denies external paths before bridge execution", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(
      project,
      recordingAsk([], { permission: "external_directory", message: "external denied" }),
    );
    const { calls, tools } = createHarness(astTools);

    const raw = (await tools.ast_grep_search.execute(
      { pattern: "console.log($MSG)", lang: "javascript", paths: [external] },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("external denied");
    expect(calls).toHaveLength(0);
  });

  test("ast_grep_replace dry-run denies external paths before bridge execution", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(
      project,
      recordingAsk([], { permission: "external_directory", message: "external denied" }),
    );
    const { calls, tools } = createHarness(astTools);

    const raw = (await tools.ast_grep_replace.execute(
      {
        pattern: "console.log($MSG)",
        rewrite: "logger.info($MSG)",
        lang: "javascript",
        paths: [external],
        dryRun: true,
      },
      sdkCtx,
    )) as string;

    expect(parsePermissionDenied(raw).message).toBe("external denied");
    expect(calls).toHaveLength(0);
  });

  test("grep asks external_directory with directory scope for directory path targets", async () => {
    const { project, external } = await makeProjectAndExternalDirs();
    const askCalls: AskCall[] = [];
    const sdkCtx = createSdkContext(project, recordingAsk(askCalls));
    const { tools } = createHarness(searchTools, () => ({ success: true, text: "ok" }));

    await tools.grep.execute({ pattern: "TODO", path: external }, sdkCtx);

    const externalAsk = askCalls.find((call) => call.permission === "external_directory");
    const expected = path.join(external, "*").split("\\").join("/");
    const widened = path.join(path.dirname(external), "*").split("\\").join("/");
    expect(externalAsk?.patterns).toEqual([expected]);
    expect(externalAsk?.patterns).not.toEqual([widened]);
  });

  test("read permission denial returns the permissionDeniedResponse envelope", async () => {
    const { project } = await makeProjectAndExternalDirs();
    const sdkCtx = createSdkContext(
      project,
      recordingAsk([], { permission: "read", message: "read denied by policy" }),
    );
    const { calls, tools } = createHarness(hoistedTools);

    const raw = (await tools.read.execute({ filePath: "inside.ts" }, sdkCtx)) as string;

    expect(parsePermissionDenied(raw).message).toBe("read denied by policy");
    expect(calls).toHaveLength(0);
  });
});
