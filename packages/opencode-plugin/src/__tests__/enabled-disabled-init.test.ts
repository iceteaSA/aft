/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, mock, spyOn, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as bridge from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { getLogFilePath } from "../logger.js";

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
    const logFile = getLogFilePath();
    rmSync(logFile, { force: true });
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
    await new Promise((resolve) => setTimeout(resolve, 600));
    const logText = existsSync(logFile) ? readFileSync(logFile, "utf8") : "";
    expect(logText).toContain(`AFT disabled by config for ${projectDir}`);
  });
});
