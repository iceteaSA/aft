/**
 * Unit tests for hoisted read/write/edit/grep argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { formatReadFooter, registerHoistedTools } from "../tools/hoisted.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

const roots: string[] = [];

async function tempRoot(): Promise<string> {
  const root = join(tmpdir(), `aft-pi-hoisted-${process.pid}-${roots.length}-${Date.now()}`);
  roots.push(root);
  await mkdir(root, { recursive: true });
  return root;
}

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("hoisted tool adapters", () => {
  test("read maps offset/limit to inclusive start_line/end_line and appends footer", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      content: "1: a\n2: b",
      truncated: true,
      start_line: 1,
      end_line: 2,
      total_lines: 10,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const ranged = (await executeTool(tools.get("read")!, {
      path: "src/app.ts",
      offset: 5,
      limit: 3,
    })) as { content: Array<{ text: string }> };

    expect(calls[0].params).toEqual({ file: "src/app.ts", start_line: 5, end_line: 7 });
    expect(ranged.content[0].text).not.toContain("Use offset/limit");

    const unbounded = (await executeTool(tools.get("read")!, { path: "src/app.ts" })) as {
      content: Array<{ text: string }>;
    };
    expect(unbounded.content[0].text).toContain("Showing lines 1-2 of 10");
  });

  test("read emits image content for vision-capable Pi models", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      attachments: [
        {
          kind: "image",
          mime: "image/png",
          data: "aW1hZ2U=",
          bytes: 1024,
          base64_bytes: 8,
          width: 32,
          height: 16,
          resized: false,
          animation: "none",
          orientation_applied: false,
        },
      ],
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: "image.png" }, {
      ...makeExtContext(),
      model: { input: ["text", "image"] },
    } as never)) as {
      content: Array<{ type: string; text?: string; data?: string; mimeType?: string }>;
    };

    expect(result.content[0]).toMatchObject({ type: "text" });
    expect(result.content[1]).toEqual({ type: "image", data: "aW1hZ2U=", mimeType: "image/png" });
  });

  test("read omits image content for non-vision Pi models", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      attachments: [
        {
          kind: "image",
          mime: "image/png",
          data: "aW1hZ2U=",
          bytes: 1024,
          base64_bytes: 8,
          width: 32,
          height: 16,
          resized: false,
          animation: "none",
          orientation_applied: false,
        },
      ],
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: "image.png" }, {
      ...makeExtContext(),
      model: { input: ["text"] },
    } as never)) as { content: Array<{ type: string; text?: string }> };

    expect(result.content).toHaveLength(1);
    expect(result.content[0].type).toBe("text");
    expect(result.content[0].text).toContain("Current model does not support images");
  });

  test("read reports PDFs as unsupported text on Pi", async () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({
      success: true,
      attachments: [
        {
          kind: "pdf",
          mime: "application/pdf",
          data: "JVBERi0=",
          bytes: 128,
          base64_bytes: 8,
        },
      ],
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: true,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("read")!, { path: "doc.pdf" })) as {
      content: Array<{ type: string; text?: string }>;
    };

    expect(result.content).toHaveLength(1);
    expect(result.content[0].type).toBe("text");
    expect(result.content[0].text).toContain("PDFs aren't supported on the Pi harness yet.");
  });

  test("edit appendContent uses append op instead of match/replacement fields", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    await executeTool(tools.get("edit")!, {
      filePath: "README.md",
      oldString: "ignored",
      newString: "ignored",
      appendContent: "\nnext",
    });

    expect(calls[0].command).toBe("edit_match");
    expect(calls[0].params).toEqual({
      op: "append",
      file: "README.md",
      append_content: "\nnext",
      diagnostics: false,
      include_diff_content: true,
    });
  });

  test("edit defaults diagnostics off and omits LSP payload", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      replacements: 1,
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("edit")!, {
      filePath: "src/app.ts",
      oldString: "before",
      newString: "after",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].command).toBe("edit_match");
    expect(calls[0].params).toMatchObject({
      file: "src/app.ts",
      match: "before",
      replacement: "after",
      diagnostics: false,
      include_diff_content: true,
    });
    expect(result.content[0].text).not.toContain("LSP diagnostics");
    expect(result.details.diagnostics).toBeUndefined();
  });

  test("edit surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    const diagnostics = [{ severity: "error", line: 5, message: "Broken edit" }];
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      replacements: 1,
      lsp_diagnostics: diagnostics,
    }));
    registerHoistedTools(
      api,
      makePluginContext(bridge, { config: { lsp: { diagnostics_on_edit: true } } }),
      {
        hoistRead: false,
        hoistWrite: false,
        hoistEdit: true,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    const result = (await executeTool(tools.get("edit")!, {
      filePath: "src/app.ts",
      oldString: "before",
      newString: "after",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].params.diagnostics).toBe(true);
    expect(result.details.diagnostics).toEqual(diagnostics);
    expect(result.content[0].text).toContain("LSP diagnostics");
    expect(result.content[0].text).toContain("Broken edit");
  });

  test("grep resolves existing path args and preserves brace-aware include globs", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    await writeFile(join(root, "src", "app.ts"), "console.log('x');\n");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: "src", include: "*.ts,**/*.{tsx,jsx}" },
      { cwd: root } as never,
    );

    expect(calls[0].command).toBe("grep");
    // Rust grep does not consume context_lines, so Pi no longer advertises or
    // forwards it (parity with OpenCode grep, which never exposed it).
    expect(calls[0].params).toEqual({
      pattern: "console",
      path: join(root, "src"),
      include: ["*.ts", "**/*.{tsx,jsx}"],
    });
  });

  test("grep expands ~ in path arg to the user's home directory", async () => {
    // Agents commonly type `~/Work/...` paths. Without expansion, Node's
    // path.resolve treats `~` as a literal directory, the existence check
    // fails, and Rust receives the unresolved path. Expansion must happen
    // before stat() so absolute tilde paths resolve like the shell would.
    const home = (await import("node:os")).homedir();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "" }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    await executeTool(tools.get("grep")!, { pattern: "oauth", path: "~/" }, { cwd: home } as never);

    expect(calls[0].command).toBe("grep");
    // When the expanded path equals the home directory itself, stat()
    // succeeds and resolvePathArg returns the absolute form.
    expect(calls[0].params).toEqual({ pattern: "oauth", path: home });
  });

  test("grep searches existing fragments and reports skipped missing paths", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    const missing = join(root, "test");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      complete: true,
      text: "src/app.ts:1: console.log('x');",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: `${join(root, "src")} ${missing}` },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }>; details: { complete?: boolean } };

    expect(calls[0].command).toBe("grep");
    expect(calls[0].params.path).toBe(join(root, "src"));
    expect(result.content[0].text).toContain("src/app.ts:1");
    expect(result.content[0].text).toContain(`Skipped 1 path not found: ${missing}`);
    expect(result.details.complete).toBe(false);
  });

  test("grep keeps all-valid multi-path searches complete", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "src"));
    await mkdir(join(root, "e2e"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      complete: true,
      text: "ok",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: `${join(root, "src")} ${join(root, "e2e")}` },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }>; details: { complete?: boolean } };

    expect(calls[0].command).toBe("grep");
    expect(calls[0].params.path).toBe(`${join(root, "src")} ${join(root, "e2e")}`);
    expect(result.content[0].text).toBe("ok");
    expect(result.content[0].text).not.toContain("Skipped");
    expect(result.details.complete).toBe(true);
  });

  test("grep falls through to path_not_found when every fragment is missing", async () => {
    const root = await tempRoot();
    const missingA = join(root, "missing-a");
    const missingB = join(root, "missing-b");
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: false,
      code: "path_not_found",
      message: "grep: search path does not exist",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    let thrown: unknown;
    try {
      await executeTool(
        tools.get("grep")!,
        { pattern: "console", path: `${missingA} ${missingB}` },
        { cwd: root } as never,
      );
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(Error);
    expect((thrown as { code?: string }).code).toBe("path_not_found");
    expect(calls[0].params.path).toBe(`${missingA} ${missingB}`);
  });

  test("grep treats an existing single path containing a space as one path", async () => {
    const root = await tempRoot();
    await mkdir(join(root, "with space"));
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      complete: true,
      text: "ok",
    }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: true,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(
      tools.get("grep")!,
      { pattern: "console", path: "with space" },
      { cwd: root } as never,
    )) as { content: Array<{ text: string }>; details: { complete?: boolean } };

    expect(calls[0].command).toBe("grep");
    expect(calls[0].params.path).toBe(join(root, "with space"));
    expect(result.content[0].text).toBe("ok");
    expect(result.content[0].text).not.toContain("Skipped");
    expect(result.details.complete).toBe(true);
  });

  test("write defaults diagnostics off and asks Rust for a diff", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    const result = (await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].command).toBe("write");
    expect(calls[0].params).toEqual({
      file: "src/app.ts",
      content: "export {};\n",
      diagnostics: false,
      include_diff_content: true,
    });
    expect(result.content[0].text).not.toContain("LSP diagnostics");
    expect(result.details.diagnostics).toBeUndefined();
  });

  test("write follows lsp.diagnostics_on_edit (config-driven; no per-call param)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(
      api,
      makePluginContext(bridge, { config: { lsp: { diagnostics_on_edit: true } } }),
      {
        hoistRead: false,
        hoistWrite: true,
        hoistEdit: false,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    });
    expect(calls[0].params.diagnostics).toBe(true);

    // The per-call `diagnostics` param was removed (agents never used it; the
    // status bar + aft_inspect are the agent-facing diagnostics paths). A
    // stray param must NOT override the configured default.
    await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
      diagnostics: false,
    });
    expect(calls[1].params.diagnostics).toBe(true);
  });

  test("write surfaces LSP payload when diagnostics_on_edit is configured", async () => {
    const diagnostics = [{ severity: "error", line: 11, message: "Broken write" }];
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({
      success: true,
      diff: { additions: 1 },
      lsp_diagnostics: diagnostics,
    }));
    registerHoistedTools(
      api,
      makePluginContext(bridge, { config: { lsp: { diagnostics_on_edit: true } } }),
      {
        hoistRead: false,
        hoistWrite: true,
        hoistEdit: false,
        hoistGrep: false,
        restrictToProjectRoot: true,
      },
    );

    const result = (await executeTool(tools.get("write")!, {
      filePath: "src/app.ts",
      content: "export {};\n",
    })) as { content: Array<{ text: string }>; details: { diagnostics?: unknown[] } };

    expect(calls[0].command).toBe("write");
    expect(calls[0].params.diagnostics).toBe(true);
    expect(result.details.diagnostics).toEqual(diagnostics);
    expect(result.content[0].text).toContain("LSP diagnostics");
    expect(result.content[0].text).toContain("Broken write");
  });

  test("mutation schemas expose no per-call diagnostics param", () => {
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: true,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // Removed deliberately: agents never used it; diagnostics are the status
    // bar (passive) + aft_inspect (pull) + the lsp.diagnostics_on_edit config.
    const writeProps = (tools.get("write")!.parameters as { properties: Record<string, unknown> })
      .properties;
    const editProps = (tools.get("edit")!.parameters as { properties: Record<string, unknown> })
      .properties;
    expect(writeProps.diagnostics).toBeUndefined();
    expect(editProps.diagnostics).toBeUndefined();
  });

  test("restrict=true hard-blocks external write (no prompt) before the bridge", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot: true,
    });

    // restrict_to_project_root is AFT's full-isolation knob — NOT a per-call
    // prompt. An external path is hard-blocked with a clear, actionable error
    // (Pi's tool-result surface) and never reaches the bridge. No ui.confirm.
    const externalPath = join(tmpdir(), `aft-external-${process.pid}-${Date.now()}.txt`);
    const extCtx = { cwd: root, hasUI: false };

    await expect(
      executeTool(tools.get("write")!, { filePath: externalPath, content: "x" }, extCtx as never),
    ).rejects.toThrow(/restrict_to_project_root/);
    expect(calls).toEqual([]);
  });

  test("external path proceeds to the bridge when restrictToProjectRoot is false", async () => {
    const root = await tempRoot();
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, diff: { additions: 1 } }));
    registerHoistedTools(api, makePluginContext(bridge), {
      hoistRead: false,
      hoistWrite: true,
      hoistEdit: false,
      hoistGrep: false,
      // User opted in to "no restriction" — Pi has no host-level allow-list
      // to consult, so AFT defers to Rust (which accepts the path).
      restrictToProjectRoot: false,
    });

    const externalPath = join(tmpdir(), `aft-external-norestrict-${process.pid}-${Date.now()}.txt`);
    const extCtx = { cwd: root, hasUI: false };

    await executeTool(
      tools.get("write")!,
      { filePath: externalPath, content: "x" },
      extCtx as never,
    );

    expect(calls).toHaveLength(1);
    expect(calls[0].command).toBe("write");
    expect(calls[0].params).toMatchObject({ file: externalPath, content: "x" });
  });

  test("formatReadFooter only hints when Rust clamped an unbounded read", () => {
    expect(
      formatReadFooter(false, { truncated: true, start_line: 1, end_line: 100, total_lines: 500 }),
    ).toBe("\n(Showing lines 1-100 of 500. Use offset/limit to read other sections.)");
    expect(
      formatReadFooter(true, { truncated: true, start_line: 1, end_line: 100, total_lines: 500 }),
    ).toBe("");
    expect(formatReadFooter(false, { truncated: true })).toBe("");
  });
});
