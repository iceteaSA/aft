import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  cleanupPiIsolatedEnv,
  createPiIsolatedEnv,
  type PiIsolatedEnv,
  type RpcClient,
  resolvePiPluginDir,
  spawnPiRpc,
  startAimock,
} from "./helpers";

async function withPiTool(
  toolCall: { name: string; arguments: Record<string, unknown> },
  opts: {
    message: string;
    setup?: (env: PiIsolatedEnv) => Promise<void>;
    onClient?: (client: RpcClient) => void;
    afterTool?: (env: PiIsolatedEnv, toolEnd: Record<string, unknown>) => Promise<void>;
    /**
     * Force `restrict_to_project_root: true` in the generated AFT config.
     * Under true, an out-of-root path is hard-blocked at the plugin layer
     * with NO ui.confirm prompt (issue #125 — the isolation knob is not a
     * per-call permission). Under the Pi default (false) the plugin defers
     * to Rust, which accepts the path.
     */
    restrictToProjectRoot?: boolean;
  },
) {
  const env = createPiIsolatedEnv();
  const aimock = await startAimock();
  let client: RpcClient | undefined;
  try {
    await opts.setup?.(env);
    aimock.registerToolCallFixture({
      predicate: () => true,
      toolCalls: [toolCall],
      followupText: "Done.",
    });
    const spawned = spawnPiRpc({
      mockProviderURL: aimock.url,
      aftPluginDir: resolvePiPluginDir(),
      configDir: env.configDir,
      workdir: env.workdir,
      restrictToProjectRoot: opts.restrictToProjectRoot,
    });
    client = spawned.client;
    opts.onClient?.(client);
    expect((await client.sendCommand({ type: "prompt", message: opts.message })).success).toBe(
      true,
    );
    const toolEnd = await client.waitForEvent(
      (event) => event.type === "tool_execution_end" && event.toolName === toolCall.name,
      30_000,
    );
    await opts.afterTool?.(env, toolEnd);
    return toolEnd;
  } finally {
    await client?.close();
    await aimock.close();
    await cleanupPiIsolatedEnv(env);
  }
}

describe("permission matrix (real Pi RPC)", () => {
  test("project-internal edit does not ask for permission", async () => {
    // Pi's contract matches native Pi tools: mutations inside the project
    // root apply without a confirmation prompt. Only external-directory
    // paths involve the permission surface (see the strict-mode test below).
    let uiRequestSeen = false;
    const toolEnd = await withPiTool(
      {
        name: "edit",
        arguments: { filePath: "inside.txt", oldString: "before", newString: "after" },
      },
      {
        message: "Edit the project file inside.txt.",
        setup: async (env) => writeFile(join(env.workdir, "inside.txt"), "before content\n"),
        onClient: (client) => {
          client.onExtensionUIRequest(() => {
            uiRequestSeen = true;
          });
        },
        afterTool: async (env) => {
          expect(await readFile(join(env.workdir, "inside.txt"), "utf8")).toBe("after content\n");
        },
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(uiRequestSeen).toBe(false);
    // Compact agent-facing summary — path is no longer echoed in the headline.
    expect(JSON.stringify(toolEnd.result)).toContain("Edited (");
  }, 120_000);

  test("external edit under strict mode is hard-blocked at the plugin layer (no prompt)", async () => {
    // Issue #125: `restrict_to_project_root` is AFT's full-isolation knob,
    // NOT a per-call permission. Under true, an out-of-root path is blocked
    // up front at the plugin layer with a clear error and NO ui.confirm
    // prompt (a grant could never override Rust's boundary anyway — that was
    // the "approved but still fails" footgun). The file stays unchanged.
    //
    // Pi users who want external paths to work should set
    // `restrict_to_project_root: false` — the Pi default — which lets Rust
    // accept the path.)
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
    try {
      const target = join(outsideDir, "confirmed-edit.txt");
      await writeFile(target, "original content\n");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        {
          name: "edit",
          arguments: { filePath: target, oldString: "original", newString: "modified" },
        },
        {
          message: `Edit ${target}.`,
          restrictToProjectRoot: true,
          onClient: (client) => {
            client.onExtensionUIRequest((request) => {
              uiRequestSeen = true;
              client.sendExtensionUIResponse({ id: request.id as string, confirmed: true });
            });
          },
        },
      );
      // No prompt under the new contract — blocked up front.
      expect(uiRequestSeen).toBe(false);
      expect(toolEnd.isError).toBe(true);
      expect(JSON.stringify(toolEnd.result).toLowerCase()).toMatch(/restrict_to_project_root/);
      // The file content stays unchanged.
      expect(await readFile(target, "utf8")).toBe("original content\n");
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);

  test("external edit under Pi default (restrict_to_project_root: false) skips prompt and succeeds", async () => {
    // The Pi default: no plugin-side prompt, Rust accepts external paths.
    // This is the path the v0.31.0 hang fix unblocked — agents calling
    // grep/write/edit on `~/Work/...` no longer get stuck on an
    // unanswerable ui.confirm prompt.
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-default-"));
    try {
      const target = join(outsideDir, "default-edit.txt");
      await writeFile(target, "original content\n");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        {
          name: "edit",
          arguments: { filePath: target, oldString: "original", newString: "modified" },
        },
        {
          message: `Edit ${target}.`,
          onClient: (client) => {
            client.onExtensionUIRequest(() => {
              uiRequestSeen = true;
            });
          },
        },
      );
      expect(uiRequestSeen).toBe(false);
      expect(toolEnd.isError).toBe(false);
      expect(await readFile(target, "utf8")).toBe("modified content\n");
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);

  test("external write under strict mode is blocked (no prompt) and leaves file absent", async () => {
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
    try {
      const target = join(outsideDir, "blocked-write.txt");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        { name: "write", arguments: { filePath: target, content: "new content\n" } },
        {
          message: `Write ${target}.`,
          restrictToProjectRoot: true,
          onClient: (client) => {
            client.onExtensionUIRequest(() => {
              uiRequestSeen = true;
            });
          },
        },
      );
      expect(uiRequestSeen).toBe(false);
      expect(toolEnd.isError).toBe(true);
      expect(JSON.stringify(toolEnd.result).toLowerCase()).toMatch(/restrict_to_project_root/);
      expect(existsSync(target)).toBe(false);
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);

  test("external grep under strict mode is blocked (no prompt)", async () => {
    const outsideDir = await mkdtemp(join(tmpdir(), "aft-pi-rpc-outside-"));
    try {
      await writeFile(join(outsideDir, "search.txt"), "needle\n");
      let uiRequestSeen = false;
      const toolEnd = await withPiTool(
        { name: "grep", arguments: { pattern: "needle", path: outsideDir } },
        {
          message: `Search ${outsideDir}.`,
          restrictToProjectRoot: true,
          onClient: (client) => {
            client.onExtensionUIRequest(() => {
              uiRequestSeen = true;
            });
          },
        },
      );
      expect(uiRequestSeen).toBe(false);
      expect(toolEnd.isError).toBe(true);
      expect(JSON.stringify(toolEnd.result).toLowerCase()).toMatch(/restrict_to_project_root/);
    } finally {
      await rm(outsideDir, { recursive: true, force: true });
    }
  }, 120_000);
});
