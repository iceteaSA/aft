/**
 * Unit tests for the /aft-status command adapter.
 *
 * The UI path opens a custom overlay dialog (see dialogs/status-dialog.ts).
 * The dialog itself fetches and re-renders status reactively, so these
 * tests only assert that the command opens the overlay; render details
 * are covered by the dialog component tests.
 */

/// <reference path="../bun-test.d.ts" />

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { registerStatusCommand } from "../commands/aft-status.js";
import { makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("aft-status command", () => {
  test("opens a custom overlay dialog when UI is available", async () => {
    const { api, commands } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true, version: "0.19.0" }));
    const customCalls: Array<{ overlay: boolean; width?: number }> = [];
    registerStatusCommand(api, makePluginContext(bridge));

    await commands.get("aft-status")!.handler("", {
      cwd: projectRoot,
      hasUI: true,
      ui: {
        // Custom overlay — accept any factory + options shape, just record
        // that it was called with overlay:true. We deliberately do NOT
        // run the factory (it would need a real Pi TUI + theme + done
        // callback); the dialog component is exercised by its own tests.
        custom: async (
          _factory: unknown,
          options?: { overlay?: boolean; overlayOptions?: { width?: number } },
        ) => {
          customCalls.push({
            overlay: options?.overlay === true,
            width: options?.overlayOptions?.width,
          });
          return undefined;
        },
        notify: () => undefined,
      },
    });

    expect(customCalls).toHaveLength(1);
    expect(customCalls[0].overlay).toBe(true);
    expect(customCalls[0].width).toBeGreaterThanOrEqual(60);
  });

  test("falls back to notify in non-UI mode", async () => {
    const { api, commands } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true, version: "0.19.0" }));
    const notifications: Array<{ message: string; level: string }> = [];
    registerStatusCommand(api, makePluginContext(bridge));

    await commands.get("aft-status")!.handler("", {
      cwd: projectRoot,
      hasUI: false,
      ui: {
        notify: (message: string, level: string) => notifications.push({ message, level }),
      },
    });

    expect(notifications).toHaveLength(1);
    expect(notifications[0]).toMatchObject({ level: "info" });
    expect(notifications[0].message).toContain("AFT version: 0.19.0");
  });

  test("reports bridge failures as UI errors without throwing (non-UI mode)", async () => {
    const { api, commands } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: false, message: "bridge down" }));
    const notifications: Array<{ message: string; level: string }> = [];
    registerStatusCommand(api, makePluginContext(bridge));

    // Non-UI path is where the bridge response is consumed directly by the
    // command handler. UI-path bridge errors surface inside the dialog
    // component instead (its own render() shows the error banner).
    await commands.get("aft-status")!.handler("", {
      cwd: projectRoot,
      hasUI: false,
      ui: {
        notify: (message: string, level: string) => notifications.push({ message, level }),
      },
    });

    expect(notifications).toEqual([{ message: "AFT status failed: bridge down", level: "error" }]);
  });

  test("fetches fresh non-UI status when cached snapshot has no session id", async () => {
    const { api, commands } = makeMockApi();
    const { bridge, calls } = makeMockBridge((_command, params) => ({
      success: true,
      version: "fresh",
      session: { id: params.session_id },
    }));
    bridge.cacheStatusSnapshot({ version: "cached" });
    const notifications: Array<{ message: string; level: string }> = [];
    registerStatusCommand(api, makePluginContext(bridge));

    await commands.get("aft-status")!.handler("", {
      cwd: projectRoot,
      hasUI: false,
      sessionManager: { getSessionId: () => "session-new" },
      ui: {
        notify: (message: string, level: string) => notifications.push({ message, level }),
      },
    });

    expect(calls).toHaveLength(1);
    expect(calls[0].params).toEqual({ session_id: "session-new" });
    expect(notifications[0].message).toContain("AFT version: fresh");
  });

  test("reuses non-UI cached status only when session id matches", async () => {
    const { api, commands } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, version: "fresh" }));
    bridge.cacheStatusSnapshot({ version: "cached", session: { id: "session-1" } });
    const notifications: Array<{ message: string; level: string }> = [];
    registerStatusCommand(api, makePluginContext(bridge));

    await commands.get("aft-status")!.handler("", {
      cwd: projectRoot,
      hasUI: false,
      sessionManager: { getSessionId: () => "session-1" },
      ui: {
        notify: (message: string, level: string) => notifications.push({ message, level }),
      },
    });

    expect(calls).toHaveLength(0);
    expect(notifications[0].message).toContain("AFT version: cached");
  });
});
