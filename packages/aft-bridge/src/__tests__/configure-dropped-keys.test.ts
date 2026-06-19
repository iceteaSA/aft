/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";

import { BinaryBridge } from "../bridge.js";

describe("BinaryBridge configure dropped keys", () => {
  test("forwards config_dropped_keys even when configure has no tool warnings", async () => {
    const deliveries: unknown[] = [];
    const bridge = new BinaryBridge(
      "/tmp/aft-does-not-need-to-exist",
      resolve(import.meta.dir, "../../.."),
      {
        onConfigureWarnings: (context) => {
          deliveries.push(context);
        },
      },
      { harness: "test" },
    );

    await (bridge as any).deliverConfigureWarnings(
      {
        success: true,
        warnings: [],
        config_dropped_keys: [
          {
            key: "semantic.backend",
            tier: "project",
            reason: "security: use user config for external backends",
          },
        ],
      },
      { session_id: "session-1" },
      { configureWarningClient: { name: "client" } },
    );

    expect(deliveries).toEqual([
      {
        projectRoot: resolve(import.meta.dir, "../../.."),
        sessionId: "session-1",
        client: { name: "client" },
        warnings: [],
        configDroppedKeys: [
          {
            key: "semantic.backend",
            tier: "project",
            reason: "security: use user config for external backends",
          },
        ],
      },
    ]);
    await bridge.shutdown();
  });
});
