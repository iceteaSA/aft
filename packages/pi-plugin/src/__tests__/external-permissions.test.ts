/**
 * Unit tests for Pi external-directory permission parity on AFT tools.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import type { BinaryBridge } from "@cortexkit/aft-bridge";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { registerAstTools } from "../tools/ast.js";
import { registerFsTools } from "../tools/fs.js";
import { registerImportTools } from "../tools/imports.js";
import { registerRefactorTool } from "../tools/refactor.js";
import { registerSafetyTool } from "../tools/safety.js";
import { registerStructureTool } from "../tools/structure.js";
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
        label: "aft_transform",
        toolName: "aft_transform",
        params: {
          op: "add_member",
          filePath: "/outside/structure.ts",
          container: "Service",
          code: "value = 1;",
        },
        command: "add_member",
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
      const { bridge, calls } = makeMockBridge((command) => {
        if (command === "delete_file") {
          return { success: true, deleted: [{ file: "/outside/delete.ts" }] };
        }
        return { success: true, text: "ok" };
      });

      if (entry.label === "aft_import") registerImportTools(api, restrictedContext(bridge));
      if (entry.label === "aft_refactor") registerRefactorTool(api, restrictedContext(bridge));
      if (entry.label === "aft_safety undo") registerSafetyTool(api, restrictedContext(bridge));
      if (entry.label === "aft_transform") registerStructureTool(api, restrictedContext(bridge));
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

      expect(prompts).toHaveLength(1);
      expect(prompts[0].title).toBe("Allow external directory access?");
      expect(prompts[0].message).toContain(`AFT wants to ${entry.action} outside the project:`);
      expect(calls[0].command).toBe(entry.command);
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
});
