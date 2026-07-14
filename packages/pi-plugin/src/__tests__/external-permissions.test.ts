/**
 * Pi external-directory isolation on AFT tools.
 *
 * Contract (issue #125): `restrict_to_project_root` is AFT's full-isolation
 * knob, deliberately NOT conflated with any per-call permission prompt. Pi has
 * no host-level permission/allow-list to bubble to, so the knob is binary:
 *   - restrict_to_project_root: true  → ordinary out-of-root paths are
 *     HARD-BLOCKED before any bridge call. Hoisted reads are forwarded so Rust
 *     can recognize exact session-owned bash artifacts without duplicating its
 *     task registry in this plugin; Rust rejects every other external read.
 *   - restrict_to_project_root: false → external paths are allowed; the tool
 *     proceeds to the bridge (Rust accepts them).
 * In-root paths are always allowed and never blocked.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import { registerAstTools } from "../tools/ast.js";
import { registerFsTools } from "../tools/fs.js";
import { registerHoistedTools } from "../tools/hoisted.js";
import { registerImportTools } from "../tools/imports.js";
import { registerInspectTool } from "../tools/inspect.js";
import { registerNavigateTool } from "../tools/navigate.js";
import { registerReadingTools } from "../tools/reading.js";
import { registerRefactorTool } from "../tools/refactor.js";
import { registerSafetyTool } from "../tools/safety.js";
import type { PluginContext } from "../types.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

function restrictedContext(bridge: BinaryBridge): PluginContext {
  return makePluginContext(bridge, { config: { restrict_to_project_root: true } });
}

// The thrown denial is the agent-facing surface; assert its actionable shape.
const BLOCK_MESSAGE = /restrict_to_project_root/;

describe("AFT external-directory isolation (restrict_to_project_root)", () => {
  test("restrict=true HARD-BLOCKS external paths on every path tool before any bridge call", async () => {
    const cases = [
      {
        label: "aft_import",
        toolName: "aft_import",
        params: { op: "organize", filePath: "/outside/imports.ts" },
      },
      {
        label: "aft_refactor",
        toolName: "aft_refactor",
        params: {
          op: "extract",
          filePath: "/outside/refactor.ts",
          name: "pulledOut",
          startLine: 1,
          endLine: 2,
        },
      },
      {
        label: "aft_safety undo",
        toolName: "aft_safety",
        params: { op: "undo", filePath: "/outside/safety.ts" },
      },
      {
        label: "ast_grep_search",
        toolName: "ast_grep_search",
        params: { pattern: "console.log($MSG)", lang: "typescript", paths: ["/outside/src"] },
      },
      {
        label: "ast_grep_replace",
        toolName: "ast_grep_replace",
        params: {
          pattern: "console.log($MSG)",
          rewrite: "logger.info($MSG)",
          lang: "typescript",
          paths: ["/outside/src"],
        },
      },
      {
        label: "aft_outline",
        toolName: "aft_outline",
        params: { target: "/outside/outline.ts" },
      },
      {
        label: "aft_zoom",
        toolName: "aft_zoom",
        params: { filePath: "/outside/zoom.ts" },
      },
      {
        label: "aft_callgraph",
        toolName: "aft_callgraph",
        params: { op: "callers", filePath: "/outside/nav.ts", symbol: "run" },
      },
      {
        label: "aft_inspect",
        toolName: "aft_inspect",
        params: { scope: "/outside/scope" },
      },
      {
        label: "aft_delete",
        toolName: "aft_delete",
        params: { files: ["/outside/delete.ts"] },
      },
      {
        label: "aft_move",
        toolName: "aft_move",
        params: { filePath: "/outside/old.ts", destination: "src/new.ts" },
      },
    ];

    for (const entry of cases) {
      const { api, tools } = makeMockApi();
      const { bridge, calls } = makeMockBridge((command, params) => {
        if (command === "undo_preview") {
          return { success: true, paths: [params.file].filter(Boolean) };
        }
        if (command === "checkpoint_paths") return { success: true, paths: [] };
        if (command === "delete_file") {
          return { success: true, deleted: [{ file: "/outside/delete.ts" }] };
        }
        return { success: true, text: "ok" };
      });

      if (entry.label === "aft_import") registerImportTools(api, restrictedContext(bridge));
      if (entry.label === "aft_refactor") registerRefactorTool(api, restrictedContext(bridge));
      if (entry.label === "aft_safety undo") registerSafetyTool(api, restrictedContext(bridge));
      if (entry.label === "ast_grep_search") {
        registerAstTools(api, restrictedContext(bridge), { astSearch: true, astReplace: false });
      }
      if (entry.label === "ast_grep_replace") {
        registerAstTools(api, restrictedContext(bridge), { astSearch: false, astReplace: true });
      }
      if (entry.label === "aft_outline") {
        registerReadingTools(api, restrictedContext(bridge), { outline: true, zoom: false });
      }
      if (entry.label === "aft_zoom") {
        registerReadingTools(api, restrictedContext(bridge), { outline: false, zoom: true });
      }
      if (entry.label === "aft_callgraph") registerNavigateTool(api, restrictedContext(bridge));
      if (entry.label === "aft_inspect") registerInspectTool(api, restrictedContext(bridge));
      if (entry.label === "aft_delete") {
        registerFsTools(api, restrictedContext(bridge), { delete: true, move: false });
      }
      if (entry.label === "aft_move") {
        registerFsTools(api, restrictedContext(bridge), { delete: false, move: true });
      }

      await expect(
        executeTool(tools.get(entry.toolName)!, entry.params, makeExtContext(projectRoot)),
        `${entry.label} must hard-block external path under restrict=true`,
      ).rejects.toThrow(BLOCK_MESSAGE);
      // The mutating bridge command must never run for a blocked external path.
      // (Safety undo/restore may preview first; assert the terminal op did not fire.)
      const terminalCommands = new Set([
        "organize_imports",
        "extract_function",
        "undo",
        "ast_search",
        "ast_replace",
        "outline",
        "zoom",
        "callers",
        "inspect",
        "delete_file",
        "move_file",
      ]);
      expect(calls.some((call) => terminalCommands.has(call.command))).toBe(false);
    }
  });

  test("restrict=true routes hoisted reads to Rust but blocks write/edit/grep external paths", async () => {
    const pathForms = [
      "/outside/hoisted.ts",
      "../outside/hoisted.ts",
      "~/aft-pi-hoisted/hoisted.ts",
    ];
    const cases = [
      { toolName: "read", params: (t: string) => ({ path: t }) },
      { toolName: "write", params: (t: string) => ({ filePath: t, content: "updated\n" }) },
      {
        toolName: "edit",
        params: (t: string) => ({ filePath: t, oldString: "before", newString: "after" }),
      },
      { toolName: "grep", params: (t: string) => ({ pattern: "needle", path: t }) },
    ];

    for (const entry of cases) {
      for (const form of pathForms) {
        const { api, tools } = makeMockApi();
        const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
        registerHoistedTools(api, restrictedContext(bridge), {
          hoistRead: true,
          hoistWrite: true,
          hoistEdit: true,
          hoistGrep: true,
          restrictToProjectRoot: true,
        });

        const execution = executeTool(
          tools.get(entry.toolName)!,
          entry.params(form),
          makeExtContext(projectRoot),
        );
        if (entry.toolName === "read") {
          await execution;
          expect(calls).toHaveLength(1);
          expect(calls[0]).toMatchObject({
            command: "tool_call",
            params: { name: "read" },
          });
        } else {
          await expect(execution).rejects.toThrow(BLOCK_MESSAGE);
          expect(calls).toHaveLength(0);
        }
      }
    }
  });

  test("restrict=false allows external paths through to the bridge (no block, no prompt)", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerImportTools(
      api,
      makePluginContext(bridge, { config: { restrict_to_project_root: false } }),
    );

    await executeTool(
      tools.get("aft_import")!,
      { op: "organize", filePath: "/outside/open.ts" },
      makeExtContext(projectRoot),
    );

    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({ command: "tool_call", params: { name: "import" } });
  });

  test("restrict=true allows IN-ROOT paths untouched", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerImportTools(api, restrictedContext(bridge));

    await executeTool(
      tools.get("aft_import")!,
      { op: "organize", filePath: join(projectRoot, "src", "in-root.ts") },
      makeExtContext(projectRoot),
    );

    expect(
      calls.some((call) => call.command === "tool_call" && call.params.name === "import"),
    ).toBe(true);
  });

  test("restrict=true blocks tilde paths that resolve outside the project", async () => {
    const tildeOutside = "~/aft-pi-tilde/imports.ts";
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerImportTools(api, restrictedContext(bridge));

    await expect(
      executeTool(
        tools.get("aft_import")!,
        { op: "organize", filePath: tildeOutside },
        makeExtContext(projectRoot),
      ),
    ).rejects.toThrow(BLOCK_MESSAGE);
    // Sanity: the resolved home path really is outside the temporary project.
    expect(resolve(homedir(), "aft-pi-tilde/imports.ts").startsWith(projectRoot)).toBe(false);
    expect(
      calls.some((call) => call.command === "tool_call" && call.params.name === "import"),
    ).toBe(false);
  });
});
