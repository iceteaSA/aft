import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";

const z = tool.schema;

/**
 * Write bytes to a running PTY background task.
 *
 * This tool only works for tasks spawned with `bash({ pty: true, background: true })`.
 * Escape sequences should be encoded as JSON string escapes, for example
 * `"\u001b[A"` for arrow-up. Rust enforces a 1 MiB maximum per call. Agents
 * should check `bash_status` first and only write when the task reports
 * `mode: "pty"`.
 */
export function createBashWriteTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      'Write input bytes to a running PTY bash task. PTY-only; use JSON escapes such as "\\u001b[A" for arrow-up. Maximum 1 MiB per call. Check bash_status reports mode: "pty" before writing.',
    args: {
      taskId: z
        .string()
        .describe("Background PTY task ID returned by bash({ pty: true, background: true })."),
      input: z
        .string()
        .describe('Input bytes to write to the PTY, e.g. "print(1)\\n" or "\\u001b[A".'),
    },
    execute: async (args, context) => {
      const data = await callBridge(ctx, context, "bash_write", {
        task_id: args.taskId as string,
        input: args.input as string,
      });
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_write failed");
      }
      return JSON.stringify({ bytes_written: data.bytes_written }, null, 2);
    },
  };
}
