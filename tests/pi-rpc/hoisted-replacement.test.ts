import { describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
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

function resultText(event: Record<string, unknown>): string {
  return JSON.stringify(event.result ?? "");
}

async function enableAftBash(env: PiIsolatedEnv): Promise<void> {
  await mkdir(join(env.workdir, ".pi"), { recursive: true });
  await writeFile(
    join(env.workdir, ".pi", "aft.jsonc"),
    JSON.stringify({ experimental: { bash: { background: true, compress: true } } }),
  );
}

async function withPiTool(
  toolCall: { name: string; arguments: Record<string, unknown> },
  opts: {
    message: string;
    setup?: (env: PiIsolatedEnv) => Promise<void>;
    afterTool?: (env: PiIsolatedEnv, toolEnd: Record<string, unknown>) => Promise<void>;
    confirmUi?: boolean;
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
    });
    client = spawned.client;
    client.onExtensionUIRequest((request) => {
      client?.sendExtensionUIResponse({
        id: request.id as string,
        confirmed: opts.confirmUi ?? true,
      });
    });
    expect(spawned.child.pid).toBeGreaterThan(0);
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

describe("hoisted tool replacement matrix (real Pi RPC)", () => {
  test("write creates a relative file and returns AFT diff metadata", async () => {
    const toolEnd = await withPiTool(
      { name: "write", arguments: { filePath: "created.txt", content: "hello\n" } },
      {
        message: "Create created.txt.",
        afterTool: async (env, event) => {
          expect(existsSync(join(env.workdir, "created.txt"))).toBe(true);
          expect(await readFile(join(env.workdir, "created.txt"), "utf8")).toBe("hello\n");
          // The text the agent sees from the tool_call is only a compact
          // summary; the full diff body lives in the details field instead.
          expect(resultText(event)).toContain("Created new file.");
          expect(resultText(event)).toContain("diff");
        },
      },
    );
    expect(toolEnd.isError).toBe(false);
  }, 120_000);

  test("edit replaceAll replaces every occurrence", async () => {
    const toolEnd = await withPiTool(
      {
        name: "edit",
        arguments: {
          filePath: "replace.txt",
          oldString: "same",
          newString: "changed",
          replaceAll: true,
        },
      },
      {
        message: "Replace every occurrence in replace.txt.",
        setup: async (env) => writeFile(join(env.workdir, "replace.txt"), "same\nsame\nsame\n"),
        afterTool: async (env) => {
          expect(await readFile(join(env.workdir, "replace.txt"), "utf8")).toBe(
            "changed\nchanged\nchanged\n",
          );
        },
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(resultText(toolEnd)).toContain("3 replacements");
  }, 120_000);

  test("edit edits[] applies two real file mutations atomically", async () => {
    const toolEnd = await withPiTool(
      {
        name: "edit",
        arguments: {
          filePath: "batch.txt",
          edits: [
            { oldString: "one", newString: "ONE" },
            { startLine: 3, endLine: 3, content: "THREE" },
          ],
        },
      },
      {
        message: "Apply two edits to batch.txt in one tool call.",
        setup: async (env) => writeFile(join(env.workdir, "batch.txt"), "one\ntwo\nthree\n"),
        afterTool: async (env) => {
          expect(await readFile(join(env.workdir, "batch.txt"), "utf8")).toBe("ONE\ntwo\nTHREE\n");
        },
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(resultText(toolEnd)).toContain("2 edits");
  }, 120_000);

  test("edit accepts integer and string batch line numbers through Pi validation", async () => {
    for (const lineValue of [3, "3"] as const) {
      const toolEnd = await withPiTool(
        {
          name: "edit",
          arguments: {
            filePath: `line-range-${String(lineValue)}.txt`,
            edits: [{ startLine: lineValue, endLine: lineValue, content: "THREE" }],
          },
        },
        {
          message: `Replace line ${String(lineValue)} in line-range-${String(lineValue)}.txt.`,
          setup: async (env) =>
            writeFile(
              join(env.workdir, `line-range-${String(lineValue)}.txt`),
              "one\ntwo\nthree\n",
            ),
          afterTool: async (env) => {
            expect(
              await readFile(join(env.workdir, `line-range-${String(lineValue)}.txt`), "utf8"),
            ).toBe("one\ntwo\nTHREE\n");
          },
        },
      );
      expect(toolEnd.isError).toBe(false);
      expect(resultText(toolEnd)).toContain("Edited (+1/-1).");
    }
  }, 120_000);

  test("grep accepts brace-glob include filters across TypeScript and Rust", async () => {
    const toolEnd = await withPiTool(
      { name: "grep", arguments: { pattern: "needle", include: "*.{ts,rs}" } },
      {
        message: "Search TypeScript and Rust files for needle.",
        setup: async (env) => {
          await writeFile(join(env.workdir, "match.ts"), "export const value = 'needle';\n");
          await writeFile(join(env.workdir, "match.rs"), 'const VALUE: &str = "needle";\n');
          await writeFile(join(env.workdir, "ignored.txt"), "needle\n");
        },
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(resultText(toolEnd)).toContain("match.ts");
    expect(resultText(toolEnd)).toContain("match.rs");
    expect(resultText(toolEnd)).not.toContain("ignored.txt");
  }, 120_000);

  test("bash accepts AFT-only compressed flag", async () => {
    const toolEnd = await withPiTool(
      { name: "bash", arguments: { command: "echo hi", compressed: false } },
      {
        message: "Run echo hi without output compression.",
        setup: enableAftBash,
      },
    );
    expect(toolEnd.isError).toBe(false);
    expect(resultText(toolEnd)).toContain("hi");
  }, 120_000);
});
