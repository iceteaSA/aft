/**
 * Unit tests for Pi external-directory permission parity on AFT tools.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { homedir } from "node:os";
import { resolve } from "node:path";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
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

interface Prompt {
  title: string;
  message: string;
}

function restrictedContext(bridge: BinaryBridge): PluginContext {
  return makePluginContext(bridge, { config: { restrict_to_project_root: true } });
}

function confirmingExtContext(prompts: Prompt[]): ExtensionContext {
  return {
    cwd: "/repo",
    hasUI: true,
    ui: {
      confirm: async (title: string, message: string) => {
        prompts.push({ title, message });
        return true;
      },
    },
  } as unknown as ExtensionContext;
}

describe("AFT external-directory permissions", () => {
  test("AFT path tools prompt before bridge calls with the expected action", async () => {
    const cases = [
      {
        label: "aft_import",
        toolName: "aft_import",
        params: { op: "organize", filePath: "/outside/imports.ts" },
        command: "organize_imports",
        action: "modify",
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
        command: "extract_function",
        action: "modify",
      },
      {
        label: "aft_safety undo",
        toolName: "aft_safety",
        params: { op: "undo", filePath: "/outside/safety.ts" },
        command: "undo",
        action: "modify",
      },
      {
        label: "ast_grep_search",
        toolName: "ast_grep_search",
        params: { pattern: "console.log($MSG)", lang: "typescript", paths: ["/outside/src"] },
        command: "ast_search",
        action: "search",
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
        command: "ast_replace",
        action: "modify",
      },
      {
        label: "aft_outline",
        toolName: "aft_outline",
        params: { target: "/outside/outline.ts" },
        command: "outline",
        action: "read",
      },
      {
        label: "aft_zoom",
        toolName: "aft_zoom",
        params: { filePath: "/outside/zoom.ts" },
        command: "zoom",
        action: "read",
      },
      {
        label: "aft_callgraph",
        toolName: "aft_callgraph",
        params: { op: "callers", filePath: "/outside/nav.ts", symbol: "run" },
        command: "callers",
        action: "read",
      },
      {
        label: "aft_inspect",
        toolName: "aft_inspect",
        params: { scope: "/outside/scope" },
        command: "inspect",
        action: "read",
      },
      {
        label: "aft_delete",
        toolName: "aft_delete",
        params: { files: ["/outside/delete.ts"] },
        command: "delete_file",
        action: "modify",
      },
      {
        label: "aft_move",
        toolName: "aft_move",
        params: { filePath: "/outside/old.ts", destination: "src/new.ts" },
        command: "move_file",
        action: "modify",
      },
    ];

    for (const entry of cases) {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge, calls } = makeMockBridge((command, params) => {
        if (command === "undo_preview") {
          return { success: true, paths: [params.file].filter(Boolean) };
        }
        if (command === "checkpoint_paths") {
          return { success: true, paths: [] };
        }
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

      await executeTool(tools.get(entry.toolName)!, entry.params, confirmingExtContext(prompts));

      expect(prompts).toHaveLength(1);
      expect(prompts[0].title).toBe("Allow external directory access?");
      expect(prompts[0].message).toContain(`AFT wants to ${entry.action} outside the project:`);
      const commandCall = calls.find((call) => call.command === entry.command);
      expect(commandCall?.command).toBe(entry.command);
    }
  });

  test("external paths are denied without UI before bridge calls", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerImportTools(api, restrictedContext(bridge));

    await expect(
      executeTool(
        tools.get("aft_import")!,
        { op: "organize", filePath: "/outside/no-ui.ts" },
        makeExtContext("/repo"),
      ),
    ).rejects.toThrow("cannot prompt for modify outside the project");
    expect(calls).toHaveLength(0);
  });

  test("hoisted read/write/edit/grep prompt for external absolute, parent-relative, and tilde paths", async () => {
    const pathForms = [
      { input: "/outside/hoisted.ts", expected: "/outside/hoisted.ts" },
      { input: "../outside/hoisted.ts", expected: resolve("/repo", "../outside/hoisted.ts") },
      {
        input: "~/aft-pi-hoisted/hoisted.ts",
        expected: resolve(homedir(), "aft-pi-hoisted/hoisted.ts"),
      },
    ];
    const cases = [
      {
        toolName: "read",
        command: "read",
        action: "read",
        params: (target: string) => ({ path: target }),
      },
      {
        toolName: "write",
        command: "write",
        action: "modify",
        params: (target: string) => ({ filePath: target, content: "updated\n" }),
      },
      {
        toolName: "edit",
        command: "edit_match",
        action: "modify",
        params: (target: string) => ({ filePath: target, oldString: "before", newString: "after" }),
      },
      {
        toolName: "grep",
        command: "grep",
        action: "search",
        params: (target: string) => ({ pattern: "needle", path: target }),
      },
    ];

    for (const entry of cases) {
      for (const form of pathForms) {
        const { api, tools } = makeMockApi();
        const prompts: Prompt[] = [];
        const { bridge, calls } = makeMockBridge((command) => {
          if (command === "read") return { success: true, content: "ok" };
          if (command === "grep") return { success: true, text: "ok" };
          return {
            success: true,
            diff: { before: "", after: "updated\n", additions: 1, deletions: 0 },
          };
        });
        registerHoistedTools(api, restrictedContext(bridge), {
          hoistRead: true,
          hoistWrite: true,
          hoistEdit: true,
          hoistGrep: true,
          restrictToProjectRoot: true,
        });

        await executeTool(
          tools.get(entry.toolName)!,
          entry.params(form.input),
          confirmingExtContext(prompts),
        );

        expect(prompts).toHaveLength(1);
        expect(prompts[0]).toEqual({
          title: "Allow external directory access?",
          message: `AFT wants to ${entry.action} outside the project: ${form.expected}`,
        });
        expect(calls.some((call) => call.command === entry.command)).toBe(true);
      }
    }
  });

  test("hoisted read/write/edit/grep deny external paths without UI before bridge calls", async () => {
    const cases = [
      {
        toolName: "read",
        action: "read",
        params: { path: "/outside/no-ui.ts" },
      },
      {
        toolName: "write",
        action: "modify",
        params: { filePath: "/outside/no-ui.ts", content: "updated\n" },
      },
      {
        toolName: "edit",
        action: "modify",
        params: { filePath: "/outside/no-ui.ts", oldString: "before", newString: "after" },
      },
      {
        toolName: "grep",
        action: "search",
        params: { pattern: "needle", path: "/outside" },
      },
    ];

    for (const entry of cases) {
      const { api, tools } = makeMockApi();
      const { bridge, calls } = makeMockBridge();
      registerHoistedTools(api, restrictedContext(bridge), {
        hoistRead: true,
        hoistWrite: true,
        hoistEdit: true,
        hoistGrep: true,
        restrictToProjectRoot: true,
      });

      await expect(
        executeTool(tools.get(entry.toolName)!, entry.params, makeExtContext("/repo")),
      ).rejects.toThrow(`cannot prompt for ${entry.action} outside the project`);
      expect(calls).toHaveLength(0);
    }
  });

  test("restrict_to_project_root=false skips external prompts", async () => {
    const { api, tools } = makeMockApi();
    const prompts: Prompt[] = [];
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerImportTools(
      api,
      makePluginContext(bridge, { config: { restrict_to_project_root: false } }),
    );

    await executeTool(
      tools.get("aft_import")!,
      { op: "organize", filePath: "/outside/open.ts" },
      confirmingExtContext(prompts),
    );

    expect(prompts).toHaveLength(0);
    expect(calls).toHaveLength(1);
  });

  test("aft_safety undo and restore preflight external preview paths", async () => {
    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge, calls } = makeMockBridge((command) =>
        command === "undo_preview"
          ? { success: true, paths: ["/outside/undo.ts", "/outside/undo.ts"] }
          : { success: true, operation: true },
      );
      registerSafetyTool(api, restrictedContext(bridge));

      await executeTool(tools.get("aft_safety")!, { op: "undo" }, confirmingExtContext(prompts));

      expect(prompts).toHaveLength(1);
      expect(prompts[0].message).toContain("/outside/undo.ts");
      expect(calls.map((call) => call.command)).toEqual(["undo_preview", "undo"]);
    }

    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge, calls } = makeMockBridge((command) =>
        command === "checkpoint_paths"
          ? { success: true, paths: ["/outside/restore.ts"] }
          : { success: true, name: "snap" },
      );
      registerSafetyTool(api, restrictedContext(bridge));

      await executeTool(
        tools.get("aft_safety")!,
        { op: "restore", name: "snap" },
        confirmingExtContext(prompts),
      );

      expect(prompts).toHaveLength(1);
      expect(prompts[0].message).toContain("/outside/restore.ts");
      expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths", "restore_checkpoint"]);
    }
  });

  test("aft_safety internal preview paths do not prompt", async () => {
    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge, calls } = makeMockBridge((command) =>
        command === "undo_preview"
          ? { success: true, paths: ["/repo/internal-undo.ts"] }
          : { success: true, operation: true },
      );
      registerSafetyTool(api, restrictedContext(bridge));

      await executeTool(tools.get("aft_safety")!, { op: "undo" }, confirmingExtContext(prompts));

      expect(prompts).toHaveLength(0);
      expect(calls.map((call) => call.command)).toEqual(["undo_preview", "undo"]);
    }

    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge, calls } = makeMockBridge((command) =>
        command === "checkpoint_paths"
          ? { success: true, paths: ["/repo/internal-restore.ts"] }
          : { success: true, name: "snap" },
      );
      registerSafetyTool(api, restrictedContext(bridge));

      await executeTool(
        tools.get("aft_safety")!,
        { op: "restore", name: "snap" },
        confirmingExtContext(prompts),
      );

      expect(prompts).toHaveLength(0);
      expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths", "restore_checkpoint"]);
    }
  });

  test("aft_safety checkpoint checks a single external filePath", async () => {
    const { api, tools } = makeMockApi();
    const prompts: Prompt[] = [];
    const { bridge, calls } = makeMockBridge(() => ({ success: true }));
    registerSafetyTool(api, restrictedContext(bridge));

    await executeTool(
      tools.get("aft_safety")!,
      { op: "checkpoint", name: "single", filePath: "/outside/checkpoint.ts" },
      confirmingExtContext(prompts),
    );

    expect(prompts).toHaveLength(1);
    expect(prompts[0].message).toContain("/outside/checkpoint.ts");
    expect(calls[0].command).toBe("checkpoint");
    expect(calls[0].params).toMatchObject({ files: ["/outside/checkpoint.ts"] });
  });

  test("multi-path tools dedupe permission prompts", async () => {
    const extFile = "/outside/same.ts";

    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge } = makeMockBridge(() => ({
        success: true,
        deleted: [{ file: extFile }],
      }));
      registerFsTools(api, restrictedContext(bridge), { delete: true, move: false });

      await executeTool(
        tools.get("aft_delete")!,
        { files: [extFile, extFile] },
        confirmingExtContext(prompts),
      );

      expect(prompts).toHaveLength(1);
    }

    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge } = makeMockBridge(() => ({ success: true }));
      registerFsTools(api, restrictedContext(bridge), { delete: false, move: true });

      await executeTool(
        tools.get("aft_move")!,
        { filePath: extFile, destination: extFile },
        confirmingExtContext(prompts),
      );

      expect(prompts).toHaveLength(1);
    }

    {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge } = makeMockBridge(() => ({ success: true }));
      registerSafetyTool(api, restrictedContext(bridge));

      await executeTool(
        tools.get("aft_safety")!,
        { op: "checkpoint", name: "external", files: [extFile, extFile] },
        confirmingExtContext(prompts),
      );

      expect(prompts).toHaveLength(1);
    }
  });

  test("tilde paths are expanded consistently in prompts and bridge requests", async () => {
    const homePath = (leaf: string) => {
      const relative = `aft-pi-tilde/${leaf}`;
      return { input: `~/${relative}`, resolved: resolve(homedir(), relative) };
    };

    const importFile = homePath("imports.ts");
    const refactorFile = homePath("refactor-source.ts");
    const refactorDestination = homePath("refactor-destination.ts");
    const safetyUndoFile = homePath("safety-undo.ts");
    const safetyCheckpointFile = homePath("safety-checkpoint.ts");
    const astSearchPath = homePath("ast-search");
    const astReplacePath = homePath("ast-replace");
    const deleteFile = homePath("delete.ts");
    const moveFile = homePath("move-source.ts");
    const moveDestination = homePath("move-destination.ts");

    const cases: Array<{
      label: string;
      toolName: string;
      params: Record<string, unknown>;
      command: string;
      action: "modify" | "search";
      promptPaths: string[];
      expectedParams: Record<string, unknown>;
    }> = [
      {
        label: "aft_import",
        toolName: "aft_import",
        params: { op: "organize", filePath: importFile.input },
        command: "organize_imports",
        action: "modify",
        promptPaths: [importFile.resolved],
        expectedParams: { file: importFile.resolved },
      },
      {
        label: "aft_refactor",
        toolName: "aft_refactor",
        params: {
          op: "move",
          filePath: refactorFile.input,
          destination: refactorDestination.input,
          symbol: "Widget",
        },
        command: "move_symbol",
        action: "modify",
        promptPaths: [refactorFile.resolved, refactorDestination.resolved],
        expectedParams: {
          file: refactorFile.resolved,
          destination: refactorDestination.resolved,
        },
      },
      {
        label: "aft_safety undo",
        toolName: "aft_safety",
        params: { op: "undo", filePath: safetyUndoFile.input },
        command: "undo",
        action: "modify",
        promptPaths: [safetyUndoFile.resolved],
        expectedParams: { file: safetyUndoFile.resolved },
      },
      {
        label: "aft_safety checkpoint",
        toolName: "aft_safety",
        params: { op: "checkpoint", name: "tilde", files: [safetyCheckpointFile.input] },
        command: "checkpoint",
        action: "modify",
        promptPaths: [safetyCheckpointFile.resolved],
        expectedParams: { files: [safetyCheckpointFile.resolved] },
      },
      {
        label: "ast_grep_search",
        toolName: "ast_grep_search",
        params: { pattern: "console.log($MSG)", lang: "typescript", paths: [astSearchPath.input] },
        command: "ast_search",
        action: "search",
        promptPaths: [astSearchPath.resolved],
        expectedParams: { paths: [astSearchPath.resolved] },
      },
      {
        label: "ast_grep_replace",
        toolName: "ast_grep_replace",
        params: {
          pattern: "console.log($MSG)",
          rewrite: "logger.info($MSG)",
          lang: "typescript",
          paths: [astReplacePath.input],
        },
        command: "ast_replace",
        action: "modify",
        promptPaths: [astReplacePath.resolved],
        expectedParams: { paths: [astReplacePath.resolved] },
      },
      {
        label: "aft_delete",
        toolName: "aft_delete",
        params: { files: [deleteFile.input] },
        command: "delete_file",
        action: "modify",
        promptPaths: [deleteFile.resolved],
        expectedParams: { files: [deleteFile.resolved] },
      },
      {
        label: "aft_move",
        toolName: "aft_move",
        params: { filePath: moveFile.input, destination: moveDestination.input },
        command: "move_file",
        action: "modify",
        promptPaths: [moveFile.resolved, moveDestination.resolved],
        expectedParams: {
          file: moveFile.resolved,
          destination: moveDestination.resolved,
        },
      },
    ];

    for (const entry of cases) {
      const { api, tools } = makeMockApi();
      const prompts: Prompt[] = [];
      const { bridge, calls } = makeMockBridge((command, params) => {
        if (command === "undo_preview") {
          return { success: true, paths: [params.file].filter(Boolean) };
        }
        if (command === "checkpoint_paths") {
          return { success: true, paths: [] };
        }
        if (command === "delete_file") {
          return {
            success: true,
            deleted: ((params.files as string[] | undefined) ?? []).map((file) => ({ file })),
          };
        }
        return { success: true, text: "ok" };
      });

      if (entry.label === "aft_import") registerImportTools(api, restrictedContext(bridge));
      if (entry.label === "aft_refactor") registerRefactorTool(api, restrictedContext(bridge));
      if (entry.label === "aft_safety undo" || entry.label === "aft_safety checkpoint") {
        registerSafetyTool(api, restrictedContext(bridge));
      }
      if (entry.label === "ast_grep_search") {
        registerAstTools(api, restrictedContext(bridge), { astSearch: true, astReplace: false });
      }
      if (entry.label === "ast_grep_replace") {
        registerAstTools(api, restrictedContext(bridge), { astSearch: false, astReplace: true });
      }
      if (entry.label === "aft_delete") {
        registerFsTools(api, restrictedContext(bridge), { delete: true, move: false });
      }
      if (entry.label === "aft_move") {
        registerFsTools(api, restrictedContext(bridge), { delete: false, move: true });
      }

      await executeTool(tools.get(entry.toolName)!, entry.params, confirmingExtContext(prompts));

      expect(prompts.map((prompt) => prompt.title)).toEqual(
        entry.promptPaths.map(() => "Allow external directory access?"),
      );
      expect(prompts.map((prompt) => prompt.message)).toEqual(
        entry.promptPaths.map(
          (path) => `AFT wants to ${entry.action} outside the project: ${path}`,
        ),
      );
      const commandCall = calls.find((call) => call.command === entry.command);
      expect(commandCall?.command).toBe(entry.command);
      expect(commandCall?.params).toMatchObject(entry.expectedParams);
    }
  });
});
