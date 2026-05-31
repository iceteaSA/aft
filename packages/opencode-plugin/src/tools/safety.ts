import * as path from "node:path";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, resolveProjectRoot } from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveAbsolutePath,
  resolveRelativePatternFromAbsolute,
  workspacePattern,
} from "./permissions.js";

const z = tool.schema;

function responsePaths(response: Record<string, unknown>): string[] {
  return Array.isArray(response.paths)
    ? response.paths.filter((path): path is string => typeof path === "string" && path.length > 0)
    : [];
}

function bridgeErrorMessage(response: Record<string, unknown>, fallback: string): string {
  return typeof response.message === "string" && response.message.length > 0
    ? response.message
    : fallback;
}

function relativePatternsFromPaths(context: ToolContext, paths: string[]): string[] {
  const seen = new Set<string>();
  const patterns: string[] = [];

  for (const filePath of paths) {
    const absolutePath = resolveAbsolutePath(context, filePath);
    const pattern = resolveRelativePatternFromAbsolute(context, absolutePath);
    if (seen.has(pattern)) continue;
    seen.add(pattern);
    patterns.push(pattern);
  }

  return patterns;
}

/**
 * Tool definitions for safety & recovery commands: undo, edit_history,
 * checkpoint, restore_checkpoint, list_checkpoints.
 */
export function safetyTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_safety: {
      description:
        "File safety and recovery operations.\n\n" +
        "Per-file undo stack is capped at 20 entries (oldest evicted).\n\n" +
        "Ops:\n" +
        "- 'undo': Undo the entire last tool call when 'filePath' is omitted (typical), or undo the last edit to one file when 'filePath' is provided. Note: pops from the undo stack (irreversible, no redo). Use 'history' to inspect per-file history before undoing.\n" +
        "- 'history': List all edit snapshots for a file. Requires 'filePath'.\n" +
        "- 'checkpoint': Save a named snapshot of tracked files. Requires 'name'. Optional 'files' to snapshot specific files only.\n" +
        "- 'restore': Restore files to a previously saved checkpoint. Requires 'name'.\n" +
        "- 'list': List all available named checkpoints. No extra params needed.\n\n" +
        "Each op requires specific parameters — see parameter descriptions for requirements.\n\n" +
        "Use checkpoint before risky multi-file changes. Use undo for quick single-file rollback.",
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z
          .enum(["undo", "history", "checkpoint", "restore", "list"])
          .describe("Safety operation"),
        filePath: z
          .string()
          .optional()
          .describe(
            "File path (required for history, optional for undo). Absolute or relative to project root",
          ),
        name: z.string().optional().describe("Checkpoint name (required for checkpoint, restore)"),
        files: z
          .array(z.string())
          .optional()
          .describe(
            "Specific files to include in checkpoint (optional, defaults to all tracked files)",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const op = args.op as string;

        if (op === "history" && typeof args.filePath !== "string") {
          throw new Error(`'filePath' is required for '${op}' op`);
        }
        if ((op === "checkpoint" || op === "restore") && typeof args.name !== "string") {
          throw new Error(`'name' is required for '${op}' op`);
        }

        if (op === "undo") {
          const previewParams: Record<string, unknown> = {};
          if (typeof args.filePath === "string") previewParams.file = args.filePath;
          const preview = await callBridge(ctx, context, "undo_preview", previewParams);
          if (preview.success === false) {
            throw new Error(bridgeErrorMessage(preview, "undo preview failed"));
          }

          const previewPaths = Array.from(new Set(responsePaths(preview)));
          for (const filePath of previewPaths) {
            const denial = await assertExternalDirectoryPermission(context, filePath);
            if (denial) return permissionDeniedResponse(denial);
          }

          const filePath =
            typeof args.filePath === "string"
              ? resolveAbsolutePath(context, args.filePath)
              : undefined;
          const permissionError = await askEditPermission(
            context,
            relativePatternsFromPaths(context, previewPaths),
            filePath ? { filepath: filePath } : { operation: "undo", paths: previewPaths },
          );
          if (permissionError) return permissionDeniedResponse(permissionError);
        }

        if (op === "checkpoint") {
          const checkpointFiles = Array.isArray(args.files)
            ? (args.files as string[])
            : typeof args.filePath === "string"
              ? [args.filePath]
              : undefined;
          if (Array.isArray(checkpointFiles)) {
            const projectRoot = await resolveProjectRoot(ctx, context);
            const uniqueParents = new Set<string>();
            for (const file of checkpointFiles) {
              if (typeof file !== "string") continue;
              const abs = path.isAbsolute(file) ? file : path.resolve(projectRoot, file);
              const parent = path.dirname(abs);
              if (uniqueParents.has(parent)) continue;
              uniqueParents.add(parent);
              const denial = await assertExternalDirectoryPermission(context, abs, {
                kind: "file",
              });
              if (denial) return permissionDeniedResponse(denial);
            }
          }
        }

        if (op === "restore") {
          const preview = await callBridge(ctx, context, "checkpoint_paths", { name: args.name });
          if (preview.success === false) {
            throw new Error(bridgeErrorMessage(preview, "checkpoint path preview failed"));
          }

          for (const filePath of new Set(responsePaths(preview))) {
            const denial = await assertExternalDirectoryPermission(context, filePath);
            if (denial) return permissionDeniedResponse(denial);
          }

          const permissionError = await askEditPermission(context, [workspacePattern(context)], {
            checkpoint: args.name,
          });
          if (permissionError) return permissionDeniedResponse(permissionError);
        }

        const commandMap: Record<string, string> = {
          undo: "undo",
          history: "edit_history",
          checkpoint: "checkpoint",
          restore: "restore_checkpoint",
          list: "list_checkpoints",
        };
        const params: Record<string, unknown> = {};
        if (args.name !== undefined) params.name = args.name;
        if (op === "checkpoint") {
          // For checkpoint, Rust only knows `files`. If the agent passes
          // `filePath` (a reasonable mistake — the tool schema exposes both),
          // auto-promote it into a single-entry `files` list rather than
          // silently dropping it and falling back to the whole tracked-file
          // set.
          if (args.files !== undefined) {
            params.files = args.files;
          } else if (args.filePath !== undefined) {
            params.files = [args.filePath];
          }
        } else {
          // undo / history / restore / list all take `file` as-is.
          if (args.filePath !== undefined) params.file = args.filePath;
          if (args.files !== undefined) params.files = args.files;
        }
        const response = await callBridge(ctx, context, commandMap[op], params);
        if (response.success === false) {
          throw new Error((response.message as string) || `${op} failed`);
        }
        return JSON.stringify(response);
      },
    },
  };
}
