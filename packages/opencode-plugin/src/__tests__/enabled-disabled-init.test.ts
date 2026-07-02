/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import * as logger from "../logger.js";

type OpenCodePlugin = typeof import("../index.js").default;

let importCounter = 0;
let releaseEnv: (() => void) | undefined;
let tempDir: string | undefined;

async function loadPlugin(): Promise<OpenCodePlugin> {
  const mod = await import(`../index.js?enabled-disabled=${importCounter++}`);
  return mod.default;
}

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  if (tempDir) rmSync(tempDir, { recursive: true, force: true });
  tempDir = undefined;
  mock.restore();
});

describe.serial("OpenCode enabled config toggle", () => {
  // Explicit 30s budget: the poll below plus env-guard acquisition can exceed
  // bun's 5s default on a loaded CI runner.
  test("disabled config returns zero tools without resolving a binary or creating a bridge pool", async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-opencode-disabled-"));
    const projectDir = join(tempDir, "project");
    mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
    writeFileSync(join(projectDir, ".cortexkit", "aft.jsonc"), '{ "enabled": false }\n');
    releaseEnv = await acquireEnv({
      HOME: join(tempDir, "home"),
      XDG_CONFIG_HOME: join(tempDir, "config"),
      XDG_CACHE_HOME: join(tempDir, "cache"),
      XDG_DATA_HOME: join(tempDir, "data"),
    });
    // Assert the log CALL, not the log file: the logger buffers behind a
    // 500ms flush timer onto a file shared by every test in the process, so
    // file-content assertions are racy/pollutable in full-suite runs.
    const logSpy = spyOn(logger, "log");
    const findBinarySpy = spyOn(bridge, "findBinary").mockImplementation(async () => {
      throw new Error("findBinary should not run when AFT is disabled");
    });
    const createPoolSpy = spyOn(bridge, "createAftTransportPool").mockImplementation(async () => {
      throw new Error("createAftTransportPool should not run when AFT is disabled");
    });

    const plugin = await loadPlugin();
    const surface = (await plugin({
      directory: projectDir,
      client: {},
    } as Parameters<OpenCodePlugin>[0])) as { tool?: Record<string, unknown> };

    expect(surface.tool).toEqual({});
    expect(findBinarySpy).not.toHaveBeenCalled();
    expect(createPoolSpy).not.toHaveBeenCalled();
    const logged = logSpy.mock.calls.map((call) => String(call[0]));
    expect(logged).toContain(`AFT disabled by config for ${projectDir}`);
  });
});
