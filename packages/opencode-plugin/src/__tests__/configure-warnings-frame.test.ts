/// <reference path="../bun-test.d.ts" />
import { afterAll, afterEach, beforeAll, describe, expect, mock, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  __resetConfigureWarningQueuesForTests,
  enqueueConfigureWarningsForSession,
  flushConfigureWarningsOnIdle,
  handleConfigureWarningsForSession,
} from "../configure-warnings.js";

const tempRoots = new Set<string>();
let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

function createStorageDir(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-opencode-configure-warnings-"));
  tempRoots.add(root);
  return root;
}

function createClient() {
  const messages: string[] = [];
  const client = {
    session: {
      prompt(input: { path?: { id?: string }; body?: { parts?: Array<{ text?: string }> } }): void {
        const text = input.body?.parts?.[0]?.text;
        if (input.path?.id && text) messages.push(`${input.path.id}:${text}`);
      },
    },
  };
  return { client, messages };
}

const bridge = {
  send: async (command: string, params: Record<string, unknown>) => {
    if (command === "db_get_state") return { success: true, data: { value: null } };
    if (command === "db_set_state") return { success: true, data: params };
    return { success: false };
  },
};

function baseWarning() {
  return {
    kind: "formatter_not_installed",
    language: "typescript",
    tool: "biome",
    hint: "Install biome with bun add -d @biomejs/biome.",
  };
}

afterEach(() => {
  __resetConfigureWarningQueuesForTests();
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("configure_warnings push-frame handler", () => {
  test("delivers a valid session_id to that session's notification handler", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await handleConfigureWarningsForSession({
      projectRoot: projectRoot,
      sessionId: "session-1",
      client,
      bridge,
      fallbackClient: { unused: true },
      warnings: [baseWarning()],
      storageDir,
      pluginVersion: "1.0.0",
      delivery: "chat",
    });

    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("session-1:");
    expect(messages[0]).toContain("Formatter is not installed");
  });

  test("missing session_id falls back gracefully without crashing", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await expect(
      handleConfigureWarningsForSession({
        projectRoot: projectRoot,
        sessionId: null,
        client,
        bridge,
        fallbackClient: client,
        warnings: [baseWarning()],
        storageDir,
        pluginVersion: "1.0.0",
      }),
    ).resolves.toBeUndefined();

    expect(messages).toHaveLength(0);
  });

  test("enqueue then idle flush delivers batched toast warnings", async () => {
    const storageDir = createStorageDir();
    const showToast = mock(async () => undefined);
    const client = { tui: { showToast } };

    enqueueConfigureWarningsForSession({
      projectRoot: projectRoot,
      sessionId: "session-1",
      client,
      bridge,
      warnings: [baseWarning()],
      fallbackClient: client,
      storageDir,
      pluginVersion: "1.0.0",
      delivery: "toast",
    });
    expect(showToast).not.toHaveBeenCalled();

    await flushConfigureWarningsOnIdle("session-1");

    expect(showToast).toHaveBeenCalledTimes(1);
    expect(showToast.mock.calls[0]?.[0]?.body?.duration).toBe(10_000);
  });
});
