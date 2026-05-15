/**
 * /aft-status — show AFT status (version, indexes, LSP, storage).
 *
 * In interactive mode this opens as an input dialog (read-only preview of
 * a formatted snapshot). When UI is unavailable (print / RPC mode), we fall
 * back to a notification.
 */

import type { ExtensionAPI, ExtensionCommandContext } from "@earendil-works/pi-coding-agent";
import { coerceAftStatus, formatStatusDialogMessage } from "../shared/status.js";
import { bridgeFor, callBridge } from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

export function registerStatusCommand(pi: ExtensionAPI, ctx: PluginContext): void {
  pi.registerCommand("aft-status", {
    description: "Show AFT plugin status (search/semantic indexes, LSP, storage)",
    handler: async (_args: string, extCtx: ExtensionCommandContext) => {
      try {
        const bridge = bridgeFor(ctx, extCtx.cwd);
        const cached = bridge.getCachedStatus();
        const response = cached
          ? { success: true, ...cached }
          : await callBridge(bridge, "status", {}, extCtx);
        if (!cached) {
          bridge.cacheStatusSnapshot(response);
        }
        const snapshot = coerceAftStatus(response);
        const text = formatStatusDialogMessage(snapshot);

        if (extCtx.hasUI) {
          // Open as a read-only input dialog so the user can scroll through
          // the full snapshot and dismiss with Esc.
          await extCtx.ui.input("AFT Status", text);
        } else {
          extCtx.ui.notify(text, "info");
        }
      } catch (err) {
        const message = `AFT status failed: ${err instanceof Error ? err.message : String(err)}`;
        if (extCtx.hasUI) {
          extCtx.ui.notify(message, "error");
        } else {
          // Print mode: write to stdout as fallback.
          console.error(`[aft-plugin] ${message}`);
        }
      }
    },
  });
}
