/// <reference path="../bun-test.d.ts" />
import { describe, expect, spyOn, test } from "bun:test";
import * as childProcess from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
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

  test("/aft-status ignored-message helper passes session model context with model-free fallback", async () => {
    // OpenCode persists the model it resolves for EVERY user message (even
    // noReply) via setAgentModel; omitting model here reset Desktop sessions
    // to the agent's default model. The helper now mirrors the session's
    // newest model/variant, and retries model-free for legacy hosts that
    // rejected model on noReply prompts (issue #62 history).
    const { sendIgnoredMessage } = await import("../shared/ignored-message.js");
    const calls: Array<Record<string, unknown>> = [];
    const client = {
      session: {
        prompt: (input: { body: Record<string, unknown> }) => {
          calls.push(input.body);
        },
        messages: async () => [
          {
            info: {
              role: "assistant",
              agent: "build",
              providerID: "anthropic",
              modelID: "claude-x",
              variant: "max",
            },
          },
        ],
      },
    };
    await sendIgnoredMessage(client, "ses_test", "hello");
    expect(calls).toHaveLength(1);
    expect(calls[0].noReply).toBe(true);
    expect(calls[0].agent).toBe("build");
    expect(calls[0].model).toEqual({ providerID: "anthropic", modelID: "claude-x" });
    expect(calls[0].variant).toBe("max");

    // Legacy host: first send (with model) throws -> retried model-free.
    const legacyCalls: Array<Record<string, unknown>> = [];
    const legacyClient = {
      session: {
        prompt: (input: { body: Record<string, unknown> }) => {
          legacyCalls.push(input.body);
          if (input.body.model) throw new Error("model not allowed on noReply");
        },
        messages: client.session.messages,
      },
    };
    await sendIgnoredMessage(legacyClient, "ses_test", "hello");
    expect(legacyCalls).toHaveLength(2);
    expect(legacyCalls[1].model).toBeUndefined();
    expect(legacyCalls[1].variant).toBeUndefined();
    expect(legacyCalls[1].agent).toBe("build");
  });

  test("glob external permission treats existing outside file as file scope", async () => {
    const project = realpathSync(mkdtempSync(join(process.cwd(), ".aft-glob-project-")));
    const outside = realpathSync(mkdtempSync(join(process.cwd(), ".aft-glob-outside-")));
    const outsideFile = join(outside, "one.ts");
    writeFileSync(outsideFile, "export const one = 1;\n");
    const asks: Array<Record<string, unknown>> = [];
    const ctx = {
      config: { search_index: true },
      pool: {
        getBridge: () => ({
          send: async () => ({ success: true, files: [] }),
          toolCall: async () => ({ success: true, text: "", files: [] }),
        }),
      },
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
