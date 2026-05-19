/// <reference path="../bun-test.d.ts" />
import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type ConfigureWarning,
  deliverConfigureWarnings,
  getSessionMessages,
  SESSION_MESSAGES_LIMIT,
} from "../notifications.js";

const tempRoots = new Set<string>();

function createStorageDir(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-opencode-notifications-"));
  tempRoots.add(root);
  return root;
}

function createClient() {
  const messages: string[] = [];
  const client = {
    session: {
      prompt(input: { body?: { parts?: Array<{ text?: string }> } }): void {
        const text = input.body?.parts?.[0]?.text;
        if (text) messages.push(text);
      },
    },
  };
  return { client, messages };
}

function baseWarning(overrides: Partial<ConfigureWarning> = {}): ConfigureWarning {
  return {
    kind: "formatter_not_installed",
    language: "typescript",
    tool: "biome",
    hint: "Install biome with bun add -d @biomejs/biome.",
    ...overrides,
  };
}

afterEach(() => {
  for (const root of tempRoots) {
    rmSync(root, { recursive: true, force: true });
  }
  tempRoots.clear();
});

describe("deliverConfigureWarnings", () => {
  test("first-time warning delivers via sendIgnoredMessage", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await deliverConfigureWarnings(
      { client, sessionId: "session-1", storageDir, pluginVersion: "1.0.0", projectRoot: "/repo" },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
    expect(messages[0]).toContain("🔧 AFT: ⚠️");
    expect(messages[0]).toContain("Formatter is not installed");
    expect(messages[0]).toContain("Install biome");
  });

  test("second call with same warning skips delivery", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const opts = {
      client,
      sessionId: "session-1",
      storageDir,
      pluginVersion: "1.0.0",
      projectRoot: "/repo",
    };

    await deliverConfigureWarnings(opts, [baseWarning()]);
    await deliverConfigureWarnings(opts, [baseWarning()]);

    expect(messages).toHaveLength(1);
  });

  test("different warnings deliver independently", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await deliverConfigureWarnings(
      { client, sessionId: "session-1", storageDir, pluginVersion: "1.0.0", projectRoot: "/repo" },
      [
        baseWarning(),
        baseWarning({ kind: "checker_not_installed", tool: "tsc", hint: "Install typescript." }),
      ],
    );

    expect(messages).toHaveLength(2);
    expect(messages[0]).toContain("Formatter is not installed");
    expect(messages[1]).toContain("Checker is not installed");
  });

  test("plugin version bump does not re-fire stale warnings", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await deliverConfigureWarnings(
      { client, sessionId: "session-1", storageDir, pluginVersion: "1.0.0", projectRoot: "/repo" },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      { client, sessionId: "session-1", storageDir, pluginVersion: "2.0.0", projectRoot: "/repo" },
      [baseWarning()],
    );

    expect(messages).toHaveLength(1);
    const persisted = JSON.parse(readFileSync(join(storageDir, "warned_tools.json"), "utf-8"));
    expect(Object.values(persisted)).toEqual(["1.0.0"]);
  });

  test("file corruption and missing storage_dir are non-fatal", async () => {
    const storageDir = createStorageDir();
    writeFileSync(join(storageDir, "warned_tools.json"), "not json");
    const missingStorageDir = join(storageDir, "missing", "nested");
    const { client, messages } = createClient();

    await deliverConfigureWarnings(
      { client, sessionId: "session-1", storageDir, pluginVersion: "1.0.0", projectRoot: "/repo" },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        storageDir: missingStorageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo",
      },
      [baseWarning({ tool: "prettier", hint: "Install prettier." })],
    );

    expect(messages).toHaveLength(2);
  });

  test("lsp_binary_missing warnings dedup across project roots", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();
    const warning = baseWarning({
      kind: "lsp_binary_missing",
      language: undefined,
      tool: undefined,
      server: "typescript-language-server",
      binary: "typescript-language-server",
      hint: "Install `typescript-language-server`.",
    });

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-a",
      },
      [warning],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-b",
      },
      [warning],
    );

    expect(messages).toHaveLength(1);
  });

  test("formatter warnings remain project-scoped", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-a",
      },
      [baseWarning()],
    );
    await deliverConfigureWarnings(
      {
        client,
        sessionId: "session-1",
        storageDir,
        pluginVersion: "1.0.0",
        projectRoot: "/repo-b",
      },
      [baseWarning()],
    );

    expect(messages).toHaveLength(2);
  });

  test("parallel configure-warning delivery uses lockfile dedupe and atomic merge", async () => {
    const storageDir = createStorageDir();
    const { client, messages } = createClient();

    await Promise.all([
      deliverConfigureWarnings(
        {
          client,
          sessionId: "session-1",
          storageDir,
          pluginVersion: "1.0.0",
          projectRoot: "/repo",
        },
        [baseWarning()],
      ),
      deliverConfigureWarnings(
        {
          client,
          sessionId: "session-1",
          storageDir,
          pluginVersion: "1.0.0",
          projectRoot: "/repo",
        },
        [baseWarning({ kind: "checker_not_installed", tool: "tsc", hint: "Install typescript." })],
      ),
      deliverConfigureWarnings(
        {
          client,
          sessionId: "session-1",
          storageDir,
          pluginVersion: "1.0.0",
          projectRoot: "/repo",
        },
        [baseWarning()],
      ),
    ]);

    expect(
      messages.filter((message) => message.includes("Formatter is not installed")),
    ).toHaveLength(1);
    expect(messages.filter((message) => message.includes("Checker is not installed"))).toHaveLength(
      1,
    );
    const persisted = JSON.parse(readFileSync(join(storageDir, "warned_tools.json"), "utf-8"));
    expect(Object.keys(persisted)).toHaveLength(2);
  });
});

// Regression coverage for the unbounded-messages-call bug surfaced by
// OpenCode's plugin agent: legacy `client.session.messages()` without a
// `query.limit` hydrates the entire session. These tests pin the bounded
// contract for the cleanup paths (`sendStatus` auto-delete + `cleanupWarnings`)
// so future edits cannot accidentally drop the limit.
describe("getSessionMessages: bounded SDK call", () => {
  test("sends query.limit on every request", async () => {
    const calls: Array<{ path: { id: string }; query?: { limit?: number } }> = [];
    const client = {
      session: {
        messages: async (input: { path: { id: string }; query?: { limit?: number } }) => {
          calls.push(input);
          return { data: [] };
        },
      },
    };

    await getSessionMessages(client, "session-1");

    expect(calls).toHaveLength(1);
    expect(calls[0].path).toEqual({ id: "session-1" });
    expect(calls[0].query).toBeDefined();
    expect(calls[0].query?.limit).toBe(SESSION_MESSAGES_LIMIT);
  });

  test("limit constant is a small positive integer", () => {
    expect(SESSION_MESSAGES_LIMIT).toBeGreaterThan(0);
    // Defensive ceiling — actual is 50; if it ever grows past 200 we want
    // a deliberate review, not a silent regression toward unboundedness.
    expect(SESSION_MESSAGES_LIMIT).toBeLessThanOrEqual(200);
  });

  test("returns the data array when call succeeds", async () => {
    const fakeMsgs = [{ info: { id: "m1", role: "user" }, parts: [{ type: "text", text: "hi" }] }];
    const client = {
      session: {
        messages: async () => ({ data: fakeMsgs }),
      },
    };
    const result = await getSessionMessages(client, "session-1");
    expect(result).toEqual(fakeMsgs);
  });

  test("returns empty array when client.session.messages is unavailable", async () => {
    expect(await getSessionMessages({}, "session-1")).toEqual([]);
  });

  test("returns empty array when the messages API throws", async () => {
    const client = {
      session: {
        messages: async () => {
          throw new Error("boom");
        },
      },
    };
    expect(await getSessionMessages(client, "session-1")).toEqual([]);
  });
});
