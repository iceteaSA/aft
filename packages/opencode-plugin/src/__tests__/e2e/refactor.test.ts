/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { refactoringTools } from "../../tools/refactoring.js";
import type { PluginContext } from "../../types.js";
import { toolResultText } from "../test-helpers.js";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
  readTextFile,
} from "./helpers.js";

type AskCall = {
  permission?: string;
  patterns?: string[];
  metadata?: Record<string, unknown>;
};

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

maybeDescribe("e2e refactor commands", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary);
    harnesses.push(created);
    return created;
  }

  test("aft_refactor returns readable sections for extract, move, and inline", async () => {
    const h = await harness();
    const tool = refactoringTools(pluginContext(h)).aft_refactor;

    const extractFile = h.path("extract-src.ts");
    await writeFile(
      extractFile,
      `export function process(): number {\n  const a = 1;\n  const b = 2;\n  const outside = 99;\n  return outside;\n}\n`,
      "utf8",
    );
    const extracted = toolResultText(
      await tool.execute(
        {
          op: "extract",
          filePath: "extract-src.ts",
          name: "computeBase",
          startLine: 2,
          endLine: 3,
        },
        runtime(h),
      ),
    );
    expect(extracted).toContain("extracted computeBase");
    expect(extracted.trim().startsWith("{")).toBe(false);
    const afterExtract = await readTextFile(extractFile);
    const processBody =
      afterExtract.match(/export function process\(\): number \{([\s\S]*?)\n\}/)?.[1] ?? "";
    const helperBody = afterExtract.match(/function computeBase[^{]*\{([\s\S]*?)\n\}/)?.[1] ?? "";
    expect(processBody).toContain("const outside = 99;");
    expect(processBody).not.toContain("const a = 1;");
    expect(processBody).not.toContain("const b = 2;");
    expect(helperBody).toContain("const a = 1;");
    expect(helperBody).toContain("const b = 2;");
    expect(helperBody).not.toContain("const outside = 99;");

    await writeFile(
      h.path("src-origin.ts"),
      `export function utility(x: number): number {\n  return x * 2;\n}\n\nexport function caller(): number {\n  return utility(3);\n}\n`,
      "utf8",
    );
    await writeFile(
      h.path("consumer.ts"),
      `import { utility } from './src-origin';\n\nexport function render(): number {\n  return utility(4);\n}\n`,
      "utf8",
    );
    await writeFile(h.path("src-dest.ts"), "// destination module\n", "utf8");
    const moved = toolResultText(
      await tool.execute(
        { op: "move", filePath: "src-origin.ts", symbol: "utility", destination: "src-dest.ts" },
        runtime(h),
      ),
    );
    expect(moved).toContain("moved symbol utility");
    expect(moved).toContain("consumers updated");
    expect(moved.trim().startsWith("{")).toBe(false);
    expect(await readTextFile(h.path("src-origin.ts"))).not.toContain("function utility");
    expect(await readTextFile(h.path("src-dest.ts"))).toContain("function utility");

    await writeFile(
      h.path("inline-src.ts"),
      `function helper(a: number, b: number): number {\n  return a + b;\n}\n\nfunction main() {\n  const result = helper(10, 20);\n  console.log(result);\n}\n`,
      "utf8",
    );
    const inlined = toolResultText(
      await tool.execute(
        { op: "inline", filePath: "inline-src.ts", symbol: "helper", callSiteLine: 6 },
        runtime(h),
      ),
    );
    expect(inlined).toContain("inlined helper");
    expect(inlined).toContain("substitutions");
    expect(inlined.trim().startsWith("{")).toBe(false);
    expect(await readTextFile(h.path("inline-src.ts"))).not.toContain("helper(10, 20)");
  });

  test("aft_refactor validates move destination before dispatch", async () => {
    const h = await harness();
    const tool = refactoringTools(pluginContext(h)).aft_refactor;

    await expect(
      tool.execute({ op: "move", filePath: "src-origin.ts", symbol: "utility" }, runtime(h)),
    ).rejects.toThrow("'destination' is required for 'move' op");
  });

  test("aft_refactor asks external-directory and edit permissions", async () => {
    const h = await harness();
    const tool = refactoringTools(pluginContext(h)).aft_refactor;
    const external = mkdtempSync(join(tmpdir(), "aft-refactor-external-"));
    try {
      const externalFile = join(external, "external.ts");
      await writeFile(
        externalFile,
        `export function external(): number {\n  const value = 1;\n  return value;\n}\n`,
        "utf8",
      );

      const asks: AskCall[] = [];
      const output = toolResultText(
        await tool.execute(
          { op: "extract", filePath: externalFile, name: "readValue", startLine: 2, endLine: 2 },
          runtime(h, recordingAsk(asks)),
        ),
      );

      expect(output).toContain("extracted readValue");
      expect(asks.some((call) => call.permission === "external_directory")).toBe(true);
      expect(asks.some((call) => call.permission === "edit")).toBe(true);
      expect(await readFile(externalFile, "utf8")).toContain("function readValue");
    } finally {
      rmSync(external, { recursive: true, force: true });
    }
  });
});

function pluginContext(harness: E2EHarness): PluginContext {
  const pool = {
    getBridge: () => harness.bridge,
  } as unknown as BridgePool;
  return {
    pool,
    client: {
      lsp: { status: async () => ({ data: [] }) },
      find: { symbols: async () => ({ data: [] }) },
    } as PluginContext["client"],
    config: {
      hoist_builtin_tools: true,
      lsp: { diagnostics_on_edit: false },
    } as PluginContext["config"],
    storageDir: harness.path(".storage"),
  };
}

function runtime(
  harness: E2EHarness,
  ask: ToolContext["ask"] = async () => undefined,
): Parameters<ToolDefinition["execute"]>[1] {
  return {
    directory: harness.tempDir,
    worktree: harness.tempDir,
    sessionID: undefined,
    messageID: "refactor-toolcall-e2e",
    agent: "test",
    abort: new AbortController().signal,
    metadata: () => {},
    ask,
  } as unknown as Parameters<ToolDefinition["execute"]>[1];
}

function recordingAsk(calls: AskCall[]): ToolContext["ask"] {
  return (async (input: AskCall) => {
    calls.push(input);
  }) as unknown as ToolContext["ask"];
}
