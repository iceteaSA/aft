/// <reference path="../bun-test.d.ts" />

import { describe, expect, mock, test } from "bun:test";
import { signalBashWaitDetachForProject } from "../bash-wait-detach.js";

describe("bash wait detach helper", () => {
  test("user-message detach sends bash_wait_detach on the active bridge", async () => {
    const calls: Array<[string, Record<string, unknown>, Record<string, unknown>]> = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return { success: true, detached: true };
      },
    };
    const pool = {
      getActiveBridgeForRoot: (projectRoot: string) => {
        expect(projectRoot).toBe("/repo");
        return bridge;
      },
      activeBridges: () => [bridge],
    };

    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      "/repo",
      "session-1",
    );

    expect(calls).toHaveLength(1);
    expect(calls[0][0]).toBe("bash_wait_detach");
    expect(calls[0][1]).toEqual({ session_id: "session-1" });
    expect(calls[0][2]).toMatchObject({
      keepBridgeOnTimeout: true,
      transportTimeoutMs: 30_000,
    });
  });

  test("user-message detach is skipped without a session or any live bridge", async () => {
    const send = mock(async () => ({ success: true }));
    const pool = {
      getActiveBridgeForRoot: () => null,
      activeBridges: () => [],
    };

    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      "/repo",
      undefined,
    );
    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      "/repo",
      "session-2",
    );

    expect(send).not.toHaveBeenCalled();
  });

  test("root-key miss fans out to every live bridge instead of dropping", async () => {
    const sends: string[] = [];
    const bridgeFor = (label: string) => ({
      send: mock(async (command: string, params: Record<string, unknown>) => {
        sends.push(`${label}:${command}:${String(params.session_id)}`);
        return { success: true };
      }),
    });
    const bridgeA = bridgeFor("a");
    const bridgeB = bridgeFor("b");
    const pool = {
      // Exact root resolution misses (the silent-drop bug this guards):
      getActiveBridgeForRoot: () => null,
      activeBridges: () => [bridgeA, bridgeB],
    };

    await signalBashWaitDetachForProject(
      pool as unknown as Parameters<typeof signalBashWaitDetachForProject>[0],
      "/repo-that-does-not-match",
      "session-3",
    );

    expect(sends.sort()).toEqual([
      "a:bash_wait_detach:session-3",
      "b:bash_wait_detach:session-3",
    ]);
  });
});
