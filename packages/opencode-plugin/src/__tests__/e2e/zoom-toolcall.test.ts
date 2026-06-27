/// <reference path="../../bun-test.d.ts" />

import { afterEach, beforeAll, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { callToolCall } from "../../tools/_shared.js";
import { readingTools } from "../../tools/reading.js";
import type { PluginContext } from "../../types.js";
import { noopAsk } from "../test-helpers";
import {
  cleanupHarnesses,
  createHarness,
  type E2EHarness,
  type PreparedBinary,
  prepareBinary,
} from "./helpers.js";

const initialBinary = await prepareBinary();
const maybeDescribe = describe.skipIf(!initialBinary.binaryPath);

function createPluginContext(harness: E2EHarness): PluginContext {
  return {
    pool: { getBridge: () => harness.bridge } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: {} as PluginContext["config"],
    storageDir: harness.path(".aft-test-storage"),
  };
}

function createToolContext(harness: E2EHarness): ToolContext {
  return {
    sessionID: "zoom-toolcall-e2e",
    messageID: "zoom-toolcall-message",
    agent: "test",
    directory: harness.tempDir,
    worktree: harness.tempDir,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  } as ToolContext;
}

function largeTsClassSource(): string {
  let source = `class BigContainer {
  methodOne(): number {
    const visibleMethodBodyLine = 1;
`;
  for (let i = 0; i < 155; i++) {
    source += `    const filler${i} = ${i};\n`;
  }
  source += `    return visibleMethodBodyLine;
  }

  methodTwo(): void {
    console.log("second");
  }
}
`;
  return source;
}

function listen(server: Server): Promise<void> {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
}

function close(server: Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });
}

maybeDescribe("e2e aft_zoom tool_call cutover", () => {
  let preparedBinary: PreparedBinary = initialBinary;
  const harnesses: E2EHarness[] = [];

  beforeAll(async () => {
    preparedBinary = await prepareBinary();
  });

  afterEach(async () => {
    await cleanupHarnesses(harnesses);
  });

  async function harness(): Promise<E2EHarness> {
    const created = await createHarness(preparedBinary, {
      fixtureNames: ["sample.ts", "sample.md", "barrel.ts"],
      timeoutMs: 20_000,
      tempPrefix: "aft-plugin-zoom-toolcall-",
    });
    harnesses.push(created);
    return created;
  }

  async function runZoom(h: E2EHarness, args: Record<string, unknown>): Promise<string> {
    const tools = readingTools(createPluginContext(h));
    const result = await tools.aft_zoom.execute(args, createToolContext(h));
    return typeof result === "string"
      ? result
      : String((result as { output?: unknown }).output ?? "");
  }

  test("single-symbol zoom returns server-rendered text through tool_call", async () => {
    const h = await harness();

    const output = await runZoom(h, {
      filePath: "sample.ts",
      symbols: "funcB",
      callgraph: true,
    });

    expect(output).toContain("sample.ts:19-21 [function funcB]");
    expect(output).toContain("export function funcB");
    expect(output).toContain("──── calls_out");
    expect(output).toContain("normalize (line 20)");
  });

  test("same-file multi-symbol partial failure returns Incomplete text", async () => {
    const h = await harness();

    const output = await runZoom(h, {
      filePath: "sample.ts",
      symbols: ["funcA", "missingSymbol"],
    });

    expect(output).toContain("Incomplete zoom results: one or more symbols failed.");
    expect(output).toContain("sample.ts:15-17 [function funcA]");
    expect(output).toContain('Symbol "missingSymbol" not found:');
  });

  test("url zoom returns server-rendered text through tool_call", async () => {
    const server = createServer((_request, response) => {
      response.writeHead(200, { "content-type": "text/markdown" });
      response.end("# Doc\n\n## Features\n\nFeature details.\n");
    });
    await listen(server);
    const address = server.address() as AddressInfo;
    const url = `http://127.0.0.1:${address.port}/doc.md`;

    try {
      const h = await harness();
      const userConfig = h.path("aft-user.jsonc");
      await writeFile(userConfig, '{ "url_fetch_allow_private": true }\n', "utf8");
      await h.bridge.send("configure", {
        project_root: h.tempDir,
        harness: "opencode",
        cortexkit_user_config_path: userConfig,
      });
      const output = await runZoom(h, { url, symbols: "Features" });

      expect(output).toContain(`${url}:3-5 [heading Features]`);
      expect(output).toContain("## Features");
    } finally {
      await close(server);
    }
  });

  test("line-range zoom is formatted by tool_call", async () => {
    const h = await harness();

    const response = await callToolCall(createPluginContext(h), createToolContext(h), "zoom", {
      filePath: "sample.md",
      startLine: 1,
      endLine: 3,
    });

    expect(response.success).toBe(true);
    expect(response.text).toContain("sample.md:1-3");
    expect(response.text).toContain("1: # Project Title");
    expect(response.text).toContain("3: Intro paragraph for the document.");
  });

  test("large containers render the member-signature menu", async () => {
    const h = await harness();
    await writeFile(h.path("large.ts"), largeTsClassSource(), "utf8");

    const output = await runZoom(h, { filePath: "large.ts", symbols: "BigContainer" });

    expect(output).toContain("member-signature menu; zoom a member for its body");
    expect(output).toContain("BigContainer.methodOne(): number");
    expect(output).toContain("BigContainer.methodTwo(): void");
    expect(output).not.toContain("visibleMethodBodyLine");
  });

  test("single-symbol not-found errors still throw", async () => {
    const h = await harness();
    const tools = readingTools(createPluginContext(h));

    await expect(
      tools.aft_zoom.execute(
        { filePath: "sample.ts", symbols: "missingSymbol" },
        createToolContext(h),
      ),
    ).rejects.toThrow("symbol 'missingSymbol' not found");
  });

  test("cross-file targets return all-success server-rendered text through tool_call", async () => {
    const h = await harness();

    const output = await runZoom(h, {
      targets: [
        { filePath: "sample.ts", symbol: "funcA" },
        { filePath: "barrel.ts", symbol: "funcA" },
      ],
    });

    expect(output).toContain("sample.ts:15-17 [function funcA]");
    expect(output).toContain("barrel.ts:15-17 [function funcA]");
  });

  test("cross-file targets return incomplete text for partial failures", async () => {
    const h = await harness();

    const output = await runZoom(h, {
      targets: [
        { filePath: "sample.ts", symbol: "funcA" },
        { filePath: "barrel.ts", symbol: "missingSymbol" },
      ],
    });

    expect(output).toContain("Incomplete zoom results: one or more symbols failed.");
    expect(output).toContain("sample.ts:15-17 [function funcA]");
    expect(output).toContain('Symbol "missingSymbol" not found in barrel.ts:');
  });

  test("cross-file targets can include callgraph annotations", async () => {
    const h = await harness();

    const output = await runZoom(h, {
      targets: [
        { filePath: "sample.ts", symbol: "funcB" },
        { filePath: "barrel.ts", symbol: "funcA" },
      ],
      callgraph: true,
    });

    expect(output).toContain("sample.ts:19-21 [function funcB]");
    expect(output).toContain("barrel.ts:15-17 [function funcA]");
    expect(output).toContain("──── calls_out");
    expect(output).toContain("normalize (line 20)");
  });
});
