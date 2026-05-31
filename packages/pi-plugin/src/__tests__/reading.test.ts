/**
 * Unit tests for aft_outline/aft_zoom argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { registerReadingTools } from "../tools/reading.js";
import { executeTool, makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

const tempRoots: string[] = [];

async function tempProject(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "aft-pi-reading-"));
  tempRoots.push(root);
  return root;
}

afterEach(async () => {
  await Promise.all(tempRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("reading tool adapters", () => {
  test("aft_outline maps a target array to the bridge files request", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "outline" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });

    await executeTool(tools.get("aft_outline")!, { target: ["src/a.ts", "src/b.ts"] });

    expect(calls[0].command).toBe("outline");
    expect(calls[0].params).toEqual({ files: ["src/a.ts", "src/b.ts"] });
  });

  test("aft_outline detects directories and sends an absolute directory path", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, files: [] }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    await executeTool(tools.get("aft_outline")!, { target: "src" }, { cwd: root } as never);

    expect(calls[0].command).toBe("outline");
    expect(calls[0].params).toEqual({ directory: join(root, "src") });
  });

  test("aft_outline treats missing local paths as file targets and lets Rust report errors", async () => {
    const root = await tempProject();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "missing" }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    await executeTool(tools.get("aft_outline")!, { target: "src/missing.ts" }, {
      cwd: root,
    } as never);

    expect(calls[0].params).toEqual({ file: "src/missing.ts" });
  });

  test("aft_outline files:true appends a walk-cap footer after the file table", async () => {
    const root = await tempProject();
    await mkdir(join(root, "src"));
    const uncheckedFiles = Array.from({ length: 12 }, (_, index) => `src/overflow-${index + 1}.ts`);
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      text: "path | language | symbols",
      complete: false,
      walk_truncated: true,
      unchecked_files: uncheckedFiles,
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: false });

    const result = (await executeTool(tools.get("aft_outline")!, { target: "src", files: true }, {
      cwd: root,
    } as never)) as { content: Array<{ text: string }> };

    expect(calls[0].params).toEqual({ directory: join(root, "src"), files: true });
    expect(result.content[0].text).toContain("path | language | symbols");
    expect(result.content[0].text).toContain(
      "⚠ Partial result: walk truncated at 200 files. 12 additional files in this directory were not indexed.",
    );
    expect(result.content[0].text).toContain("Unchecked files:");
    expect(result.content[0].text).toContain("src/overflow-1.ts");
    expect(result.content[0].text).toContain("src/overflow-10.ts");
    expect(result.content[0].text).not.toContain("src/overflow-11.ts");
    expect(result.content[0].text).toContain("... +2 more");
  });

  test("aft_zoom maps contextLines to each batched symbol request and preserves failures", async () => {
    const root = await tempProject();
    await writeFile(join(root, "src.ts"), "export function ok() {}\n");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => {
      if (params.symbol === "missing") return { success: false, message: "not found" };
      return { success: true, symbol: params.symbol, text: "1: export function ok() {}" };
    });
    registerReadingTools(api, makePluginContext(bridge), { outline: true, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      symbols: ["ok", "missing"],
      contextLines: 2,
    })) as { content: Array<{ text: string }> };

    expect(calls.map((call) => call.params)).toEqual([
      { file: "src.ts", symbol: "ok", context_lines: 2 },
      { file: "src.ts", symbol: "missing", context_lines: 2 },
    ]);
    expect(result.content[0].text).toContain("src.ts:1-1");
    expect(result.content[0].text).toContain('Symbol "missing" not found: not found');
  });

  test("aft_zoom rejects ambiguous filePath/url input before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    await expect(
      executeTool(tools.get("aft_zoom")!, { filePath: "src.ts", url: "https://example.com/a.md" }),
    ).rejects.toThrow("not both");
    expect(calls).toHaveLength(0);
  });

  test("aft_zoom targets array fans out one zoom request per entry across different files", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      name: params.symbol as string,
      kind: "function",
      range: { start_line: 10, end_line: 20 },
      content: `// body of ${params.symbol} from ${params.file}\n`,
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      targets: [
        { filePath: "src/a.ts", symbol: "foo" },
        { filePath: "src/b.ts", symbol: "bar" },
      ],
    })) as { content: Array<{ text: string }> };

    expect(calls.map((call) => call.params)).toEqual([
      { file: "src/a.ts", symbol: "foo" },
      { file: "src/b.ts", symbol: "bar" },
    ]);
    // Each section uses its own filePath as the header label.
    expect(result.content[0].text).toContain("src/a.ts:10-20 [function foo]");
    expect(result.content[0].text).toContain("src/b.ts:10-20 [function bar]");
  });

  test("aft_zoom targets renders per-entry failure with the right file label", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge((_command, params) => {
      if (params.symbol === "missing") {
        return { success: false, message: "symbol 'missing' not found" };
      }
      return {
        success: true,
        name: params.symbol as string,
        kind: "function",
        range: { start_line: 1, end_line: 2 },
        content: "ok\n",
      };
    });
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    const result = (await executeTool(tools.get("aft_zoom")!, {
      targets: [
        { filePath: "src/a.ts", symbol: "ok" },
        { filePath: "src/b.ts", symbol: "missing" },
      ],
    })) as { content: Array<{ text: string }> };

    expect(result.content[0].text).toContain("Incomplete zoom results");
    expect(result.content[0].text).toContain('Symbol "missing" not found in src/b.ts:');
    expect(result.content[0].text).toContain("src/a.ts:1-2 [function ok]");
  });

  test("aft_zoom targets is mutually exclusive with filePath/symbols/url", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    await expect(
      executeTool(tools.get("aft_zoom")!, {
        targets: [{ filePath: "src/a.ts", symbol: "foo" }],
        filePath: "src/a.ts",
        symbols: "foo",
      }),
    ).rejects.toThrow(/mutually exclusive/);
    expect(calls).toHaveLength(0);
  });

  test("aft_zoom treats empty-content targets shapes as not provided (GPT empty-param regression)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      name: params.symbol as string,
      kind: "function",
      range: { start_line: 1, end_line: 5 },
      content: "ok\n",
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    // Array of empty-content entries — GPT models send this shape instead of omitting `targets`.
    // Plus `url: ""` (empty string). Both must be treated as not-provided so filePath wins
    // and we don't get a misleading "targets is mutually exclusive" error.
    await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      url: "",
      targets: [{ filePath: "", symbol: "" }],
      symbols: "ok",
    });
    expect(calls).toHaveLength(1);

    // Single object form, same all-empty pattern.
    calls.length = 0;
    await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      targets: { filePath: "", symbol: "" },
      symbols: "ok",
    });
    expect(calls).toHaveLength(1);
  });

  test("aft_zoom symbols accepts string form for single-symbol lookup", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      name: params.symbol as string,
      kind: "function",
      range: { start_line: 1, end_line: 5 },
      content: "ok\n",
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    // Single string for `symbols` — no array, no `symbol`.
    const result = (await executeTool(tools.get("aft_zoom")!, {
      filePath: "src.ts",
      symbols: "ok",
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(calls[0]?.params).toMatchObject({ file: "src.ts", symbol: "ok" });
    // Single-symbol shortcut returns raw formatZoomText (no "Incomplete" framing).
    expect(result.content[0].text).not.toContain("Incomplete zoom results");
    expect(result.content[0].text).toContain("src.ts");
  });

  test("aft_zoom targets accepts single object form for one-target lookup", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      name: params.symbol as string,
      kind: "function",
      range: { start_line: 1, end_line: 2 },
      content: "ok\n",
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    // Single object for `targets` — no array.
    const result = (await executeTool(tools.get("aft_zoom")!, {
      targets: { filePath: "src/a.ts", symbol: "foo" },
    })) as { content: Array<{ text: string }> };

    expect(calls).toHaveLength(1);
    expect(calls[0]?.params).toMatchObject({ file: "src/a.ts", symbol: "foo" });
    expect(result.content[0].text).toContain("src/a.ts:1-2 [function foo]");
  });

  test("aft_zoom threads callgraph true to all zoom request shapes and omits it by default", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      name: (params.symbol as string | undefined) ?? "lines",
      kind: params.symbol ? "function" : "lines",
      range: { start_line: 1, end_line: 1 },
      content: "ok\n",
    }));
    registerReadingTools(api, makePluginContext(bridge), { outline: false, zoom: true });

    await executeTool(tools.get("aft_zoom")!, {
      targets: [{ filePath: "src/a.ts", symbol: "foo" }],
      callgraph: true,
    });
    await executeTool(tools.get("aft_zoom")!, {
      filePath: "src/a.ts",
      symbols: ["foo"],
      callgraph: true,
    });
    await executeTool(tools.get("aft_zoom")!, { filePath: "src/a.ts", callgraph: true });

    expect(calls.map((call) => call.params)).toEqual([
      expect.objectContaining({ file: "src/a.ts", symbol: "foo", callgraph: true }),
      expect.objectContaining({ file: "src/a.ts", symbol: "foo", callgraph: true }),
      expect.objectContaining({ file: "src/a.ts", callgraph: true }),
    ]);

    calls.length = 0;
    await executeTool(tools.get("aft_zoom")!, { filePath: "src/a.ts", symbols: "foo" });
    expect(calls[0]?.params).toMatchObject({ file: "src/a.ts", symbol: "foo" });
    expect(calls[0]?.params).not.toHaveProperty("callgraph");
  });
});
