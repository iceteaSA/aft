import { AsyncLocalStorage } from "node:async_hooks";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { sessionLog } from "./logger.js";

/**
 * Per-tool-call timing slot. The execute wrapper establishes it; `callBridge`
 * stamps the bridge window into it via the AsyncLocalStorage below, so the four
 * probe points (entry -> before bridge -> after bridge -> end) are correlated
 * without threading a timer through every tool signature.
 */
type ToolPerfSlot = {
  tool: string;
  sessionID?: string;
  t0: number;
  bridgeStart?: number;
  bridgeEnd?: number;
};

const perfStore = new AsyncLocalStorage<ToolPerfSlot>();

/**
 * Mark the instant just before a bridge request is sent. First mark wins so a
 * tool that issues several bridge requests still reports one bridge window
 * (first send -> last response); the intervening plugin work folds into it.
 */
export function markBridgeStart(): void {
  const slot = perfStore.getStore();
  if (slot && slot.bridgeStart === undefined) {
    slot.bridgeStart = performance.now();
  }
}

/** Mark the instant just after the bridge responds (last response wins). */
export function markBridgeEnd(): void {
  const slot = perfStore.getStore();
  if (slot) {
    slot.bridgeEnd = performance.now();
  }
}

function emit(slot: ToolPerfSlot): void {
  const t3 = performance.now();
  const total = Math.round(t3 - slot.t0);
  if (slot.bridgeStart !== undefined && slot.bridgeEnd !== undefined) {
    const pre = Math.round(slot.bridgeStart - slot.t0);
    const bridge = Math.round(slot.bridgeEnd - slot.bridgeStart);
    const post = Math.round(t3 - slot.bridgeEnd);
    sessionLog(
      slot.sessionID,
      `perf tool=${slot.tool} total=${total}ms pre=${pre}ms bridge=${bridge}ms post=${post}ms`,
    );
  } else {
    // Tool returned without a bridge round-trip (pure-TS path or early error).
    sessionLog(slot.sessionID, `perf tool=${slot.tool} total=${total}ms (no bridge call)`);
  }
}

/**
 * Wrap every tool's execute() so each invocation logs a one-line latency
 * breakdown: total, pre-bridge (arg/permission/session-dir work), bridge
 * round-trip, and post-bridge (result formatting). Mutates execute in place;
 * call after the final tool surface is built so disabled tools aren't wrapped.
 */
export function instrumentToolMap(
  tools: Record<string, ToolDefinition>,
): Record<string, ToolDefinition> {
  for (const [name, def] of Object.entries(tools)) {
    const original = def.execute;
    if (typeof original !== "function") continue;
    def.execute = ((args: unknown, context: { sessionID?: string }) => {
      const slot: ToolPerfSlot = {
        tool: name,
        sessionID: context?.sessionID,
        t0: performance.now(),
      };
      return perfStore.run(slot, async () => {
        try {
          return await original(args as never, context as never);
        } finally {
          emit(slot);
        }
      });
    }) as ToolDefinition["execute"];
  }
  return tools;
}
