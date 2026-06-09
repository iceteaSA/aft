import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import {
  callBridge,
  expandTilde,
  isEmptyParam,
  resolvePathFromProjectRoot,
  resolveProjectRoot,
} from "./_shared.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

/**
 * Tool definition for the git conflict discovery and parsing tool.
 */
export function conflictTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_conflicts: {
      description:
        "Show all git merge conflicts across the repository — returns line-numbered conflict regions with context for every conflicted file in a single call. Conflicts are discovered from the git repository's top level. By default it inspects the session's project repository; pass `path` to inspect a different repository or git worktree (e.g. where a rebase/merge is running).",
      args: {
        path: z
          .string()
          .describe(
            "Optional path inside the git repository or worktree to inspect (absolute or relative to project root). Conflicts are discovered from that repository's top level. Defaults to the session project root.",
          )
          .optional(),
      },
      execute: async (args, context): Promise<string> => {
        const params: Record<string, unknown> = {};
        if (!isEmptyParam(args?.path)) {
          // `path` points at a repo/worktree to inspect — gate it through the
          // external-directory permission like the other path-taking tools, so
          // inspecting a repo outside the project root still prompts. Resolve
          // tilde/relative the same way Rust will before the check.
          const expanded = expandTilde(String(args.path));
          const projectRoot = await resolveProjectRoot(ctx, context);
          const resolved = resolvePathFromProjectRoot(projectRoot, expanded);
          const denied = await assertExternalDirectoryPermission(context, resolved, {
            kind: "directory",
          });
          if (denied) return permissionDeniedResponse(denied);
          // Send the SAME resolved path the permission check approved (closes the
          // check-vs-use gap and gives Rust an absolute path it won't re-resolve
          // against a possibly-different cwd).
          params.path = resolved;
        }
        const response = await callBridge(ctx, context, "git_conflicts", params);
        if (response.success === false) {
          throw new Error((response.message as string) || "git_conflicts failed");
        }
        return response.text as string;
      },
    },
  };
}
