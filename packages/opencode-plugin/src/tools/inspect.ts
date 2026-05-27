import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam } from "./_shared.js";

const z = tool.schema;

type ToolArg = ToolDefinition["args"][string];

type StringOrStringArray = string | string[];

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

function normalizeStringOrArray(value: unknown): StringOrStringArray | undefined {
  return isEmptyParam(value) ? undefined : (value as StringOrStringArray);
}

export interface InspectToolConfig {
  tool_surface?: "minimal" | "recommended" | "all";
  disabled_tools?: string[];
  inspect?: {
    enabled?: boolean;
    tier2_idle_minutes?: number;
  };
}

export function inspectToolSurfaceEnabled(config: InspectToolConfig): boolean {
  return (config.tool_surface ?? "recommended") !== "minimal" && config.inspect?.enabled !== false;
}

export function shouldRegisterInspectTool(config: InspectToolConfig): boolean {
  return (
    inspectToolSurfaceEnabled(config) && !(config.disabled_tools ?? []).includes("aft_inspect")
  );
}

type TimerHandle = ReturnType<typeof setTimeout>;

export interface InspectTier2IdleSchedulerOptions {
  isEnabled: () => boolean;
  idleMinutes: () => number | undefined;
  run: (sessionID: string) => Promise<void>;
  warn?: (message: string) => void;
  setTimer?: (callback: () => void, delayMs: number) => TimerHandle;
  clearTimer?: (timer: TimerHandle) => void;
}

export function createInspectTier2IdleScheduler(options: InspectTier2IdleSchedulerOptions) {
  const timers = new Map<string, TimerHandle>();
  const setTimer = options.setTimer ?? ((callback, delayMs) => setTimeout(callback, delayMs));
  const clearTimer = options.clearTimer ?? ((timer) => clearTimeout(timer));

  const clear = (sessionID: string): void => {
    const timer = timers.get(sessionID);
    if (!timer) return;
    clearTimer(timer);
    timers.delete(sessionID);
  };

  const clearAll = (): void => {
    for (const timer of timers.values()) {
      clearTimer(timer);
    }
    timers.clear();
  };

  const schedule = (sessionID: string): void => {
    if (!options.isEnabled()) return;
    clear(sessionID);
    const idleMinutes = options.idleMinutes() ?? 4;
    const delayMs = Math.max(0, idleMinutes * 60 * 1000);
    const timer = setTimer(() => {
      timers.delete(sessionID);
      options.run(sessionID).catch((err) => {
        options.warn?.(
          `inspect_tier2_run failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      });
    }, delayMs);
    timers.set(sessionID, timer);
  };

  return { schedule, clear, clearAll };
}

export function inspectTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const inspectTool: ToolDefinition = {
    description:
      "Codebase health snapshot. One call returns summary stats for: TODOs, file/symbol metrics, LSP diagnostics, dead code, unused exports, code duplicates. Pass `sections` for per-category drill-down details.\n\n" +
      "Categories run in tiers — Tier 1 (todos, metrics, diagnostics) return synchronously from cache. Tier 2 (dead_code, unused_exports, duplicates) run as background scans triggered on session idle; calls may return cached `stale_categories: [...]` results if a refresh is in progress (waits up to 1s for fresh data before falling back to cached).\n\n" +
      "Use when: starting work on unfamiliar code, before a refactor, before review, or to verify cleanup completeness.",
    args: {
      sections: arg(
        z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Categories to include in detailed drill-down (e.g. 'todos' or ['todos', 'dead_code']). Use 'all' for every active category. Omit for summary-only mode.",
          ),
      ),
      scope: arg(
        z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Restrict drill-down items to paths under this scope (file or directory, absolute or relative to project root). Tier 2 categories scan project-wide regardless of scope and apply scope as a result filter.",
          ),
      ),
      topK: arg(
        z
          .number()
          .int()
          .positive()
          .max(100)
          .optional()
          .describe("Max drill-down items per category. Default 20, max 100."),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const sections = normalizeStringOrArray(args.sections);
      const scope = normalizeStringOrArray(args.scope);
      const topK = args.topK === undefined || args.topK === null ? undefined : args.topK;

      const response = await callBridge(ctx, context, "inspect", { sections, scope, topK });
      if (response.success === false) {
        throw new Error((response.message as string) || "inspect failed");
      }
      if (typeof response.text === "string") {
        return response.text;
      }
      return JSON.stringify(response, null, 2);
    },
  };

  return {
    aft_inspect: inspectTool,
  };
}
