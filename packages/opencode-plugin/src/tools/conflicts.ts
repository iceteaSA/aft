import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam } from "./_shared.js";

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
        if (!isEmptyParam(args?.path)) params.path = args.path;
        const response = await callBridge(ctx, context, "git_conflicts", params);
        if (response.success === false) {
          throw new Error((response.message as string) || "git_conflicts failed");
        }
        return response.text as string;
      },
    },
  };
}
