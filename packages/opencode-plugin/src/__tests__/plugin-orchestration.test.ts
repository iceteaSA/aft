/// <reference path="../bun-test.d.ts" />
import { describe, expect, spyOn, test } from "bun:test";
import * as childProcess from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import {
  __resetConfigureWarningQueuesForTests,
  enqueueConfigParseWarnings,
  enqueueConfigureWarningsForSession,
  flushConfigureWarningsOnIdle,
} from "../configure-warnings.js";
import { searchTools } from "../tools/search.js";
import type { PluginContext } from "../types.js";

const bridge = {
  send: async (command: string, params: Record<string, unknown>) => {
    if (command === "db_get_state") return { success: true, data: { value: null } };
    if (command === "db_set_state") return { success: true, data: params };
    return { success: false };
  },
};

describe("Lane G plugin orchestration regressions", () => {
  test("eager configure warnings buffer until session idle flush", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-eager-warnings-"));
    const messages: string[] = [];
    const client = {
      session: {
        prompt: (input: { body: { parts: Array<{ text: string }> } }) =>
          messages.push(input.body.parts[0].text),
      },
    };
    const warning = {
      kind: "formatter_not_installed" as const,
      language: "ts",
      tool: "biome",
      hint: "Install biome.",
    };
    try {
      enqueueConfigureWarningsForSession({
        projectRoot: "/repo-eager",
        warnings: [warning],
        bridge,
        fallbackClient: client,
        storageDir: root,
        pluginVersion: "1.0.0",
        delivery: "chat",
      });
      expect(messages).toHaveLength(0);

      enqueueConfigureWarningsForSession({
        projectRoot: "/repo-eager",
        sessionId: "session-1",
        client,
        bridge,
        warnings: [],
        fallbackClient: client,
        storageDir: root,
        pluginVersion: "1.0.0",
        delivery: "chat",
      });
      expect(messages).toHaveLength(0);

      await flushConfigureWarningsOnIdle("session-1");

      expect(messages).toHaveLength(1);
      expect(messages[0]).toContain("Formatter is not installed");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("config parse warnings enqueue and flush on session idle", async () => {
    __resetConfigureWarningQueuesForTests();
    const root = mkdtempSync(join(tmpdir(), "aft-config-parse-warnings-"));
    const messages: string[] = [];
    const client = {
      session: {
        prompt: (input: { body: { parts: Array<{ text: string }> } }) =>
          messages.push(input.body.parts[0].text),
      },
    };
    const configPath = join(root, "aft.jsonc");
    try {
      enqueueConfigParseWarnings("/repo-parse", [
        { path: configPath, message: "Unexpected token i" },
      ]);
      enqueueConfigureWarningsForSession({
        projectRoot: "/repo-parse",
        sessionId: "session-parse",
        client,
        bridge,
        warnings: [],
        fallbackClient: client,
        storageDir: root,
        pluginVersion: "1.0.0",
        delivery: "chat",
      });
      await flushConfigureWarningsOnIdle("session-parse");
      expect(messages).toHaveLength(1);
      expect(messages[0]).toContain("failed to parse and was ignored");
      expect(messages[0]).toContain("npx @cortexkit/aft doctor");
    } finally {
      rmSync(root, { recursive: true, force: true });
      __resetConfigureWarningQueuesForTests();
    }
  });

  test("auto-update restores package.json, lockfile, and package dir on npm failure", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-auto-update-restore-"));
    const pkgDir = join(root, "node_modules", "@cortexkit", "aft-opencode");
    mkdirSync(pkgDir, { recursive: true });
    writeFileSync(
      join(root, "package.json"),
      JSON.stringify({ dependencies: { "@cortexkit/aft-opencode": "0.1.0" } }),
    );
    writeFileSync(
      join(root, "package-lock.json"),
      JSON.stringify({
        packages: { "node_modules/@cortexkit/aft-opencode": { version: "0.1.0" } },
      }),
    );
    writeFileSync(join(pkgDir, "marker.txt"), "original");

    const proc = new EventEmitter() as childProcess.ChildProcess;
    proc.stdout = new EventEmitter() as childProcess.ChildProcess["stdout"];
    proc.stderr = new EventEmitter() as childProcess.ChildProcess["stderr"];
    const spawnMock = spyOn(childProcess, "spawn").mockImplementation(() => {
      setTimeout(() => proc.emit("exit", 1), 0);
      return proc;
    });
    const { preparePackageUpdate, runNpmInstallSafe } = await import(
      "../hooks/auto-update-checker/cache.js?restore-test"
    );
    try {
      expect(
        preparePackageUpdate("0.2.0", "@cortexkit/aft-opencode", join(pkgDir, "package.json")),
      ).toBe(root);
      expect(await runNpmInstallSafe(root, { timeoutMs: 1000 })).toMatchObject({ ok: false });
      expect(readFileSync(join(root, "package.json"), "utf-8")).toContain("0.1.0");
      expect(readFileSync(join(root, "package-lock.json"), "utf-8")).toContain("0.1.0");
      expect(readFileSync(join(pkgDir, "marker.txt"), "utf-8")).toBe("original");
      expect(spawnMock.mock.calls[0][2]).toMatchObject({ stdio: ["ignore", "pipe", "pipe"] });
    } finally {
      spawnMock.mockRestore();
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("/aft-status ignored-message helper keeps noReply payload model-free but agent-aware (issue #62)", () => {
    const source = readFileSync(resolve(import.meta.dir, "../index.ts"), "utf-8");
    const helper = source.slice(
      source.indexOf("async function sendIgnoredMessage"),
      source.indexOf("/** Read the plugin's own version"),
    );
    expect(helper).toContain("noReply: true");
    // Issue #62: agent IS passed so notifications render under the user's
    // current agent. Model/variant remain off this path (no LLM turn fires
    // for noReply: true, and earlier OpenCode versions crashed when we
    // passed model on this path).
    expect(helper).toContain("body.agent");
    expect(helper).toContain("resolvePromptContext");
    expect(helper).not.toContain("body.model");
    expect(helper).not.toContain("body.variant");
  });

  test("glob external permission treats existing outside file as file scope", async () => {
    const project = mkdtempSync(join(tmpdir(), "aft-glob-project-"));
    const outside = mkdtempSync(join(tmpdir(), "aft-glob-outside-"));
    const outsideFile = join(outside, "one.ts");
    writeFileSync(outsideFile, "export const one = 1;\n");
    const asks: Array<Record<string, unknown>> = [];
    const ctx = {
      config: { search_index: true },
      pool: { getBridge: () => ({ send: async () => ({ success: true, files: [] }) }) },
    } as unknown as PluginContext;
    const sdkCtx = {
      directory: project,
      worktree: project,
      async ask(input: Record<string, unknown>) {
        asks.push(input);
      },
    } as any;
    try {
      await searchTools(ctx).glob.execute({ pattern: "*.ts", path: outsideFile }, sdkCtx);
      const externalAsk = asks.find((ask) => ask.permission === "external_directory") as {
        patterns: string[];
      };
      expect(externalAsk.patterns[0]).toBe(`${outside}/*`);
    } finally {
      rmSync(project, { recursive: true, force: true });
      rmSync(outside, { recursive: true, force: true });
    }
  });
});
