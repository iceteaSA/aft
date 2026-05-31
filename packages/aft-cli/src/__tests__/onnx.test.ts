/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test__ as bridgeOnnxTest } from "../../../aft-bridge/src/onnx-runtime.js";
import {
  findCachedOnnxRuntime,
  findSystemOnnxRuntime,
  getOnnxLibraryName,
  ONNX_RUNTIME_VERSION,
} from "../lib/onnx.js";

type EnvSnapshot = Map<string, string | undefined>;

let workDir: string;
let envSnapshot: EnvSnapshot;

beforeEach(() => {
  workDir = mkdtempSync(join(tmpdir(), "aft-cli-onnx-test-"));
  envSnapshot = new Map([
    ["PATH", process.env.PATH],
    ["Path", process.env.Path],
    ["path", process.env.path],
  ]);
});

afterEach(() => {
  for (const [key, value] of envSnapshot) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
  rmSync(workDir, { recursive: true, force: true });
});

function withPlatform<T>(platform: NodeJS.Platform, fn: () => T): T {
  const descriptor = Object.getOwnPropertyDescriptor(process, "platform");
  Object.defineProperty(process, "platform", { configurable: true, value: platform });
  try {
    return fn();
  } finally {
    if (descriptor) Object.defineProperty(process, "platform", descriptor);
  }
}

describe("CLI ONNX system detection", () => {
  test("doctor detects Windows ONNX Runtime from PATH", () => {
    const runtimeDir = join(workDir, "onnxruntime", "bin");
    mkdirSync(runtimeDir, { recursive: true });
    writeFileSync(join(runtimeDir, "OnNxRuNtImE.DlL"), "binary");
    process.env.PATH = `${join(workDir, "missing")};${runtimeDir}`;
    delete process.env.Path;
    delete process.env.path;

    const found = withPlatform("win32", () => findSystemOnnxRuntime());

    expect(found).toBe(runtimeDir);
  });
});

describe("CLI ONNX cached detection (#71)", () => {
  test("finds the cached library in the version root", () => {
    const versionDir = join(workDir, "onnxruntime", ONNX_RUNTIME_VERSION);
    mkdirSync(versionDir, { recursive: true });
    writeFileSync(join(versionDir, getOnnxLibraryName()), "stub");
    expect(findCachedOnnxRuntime(workDir)).toBe(versionDir);
  });

  test("finds the cached library in a lib/ subdir (manual install)", () => {
    const versionDir = join(workDir, "onnxruntime", ONNX_RUNTIME_VERSION);
    const libDir = join(versionDir, "lib");
    mkdirSync(libDir, { recursive: true });
    // Microsoft's archive lays the library out under lib/, not the version root.
    writeFileSync(join(libDir, getOnnxLibraryName()), "stub");
    expect(findCachedOnnxRuntime(workDir)).toBe(libDir);
  });

  test("returns null when no library is present in either layout", () => {
    mkdirSync(join(workDir, "onnxruntime", ONNX_RUNTIME_VERSION), { recursive: true });
    expect(findCachedOnnxRuntime(workDir)).toBeNull();
  });
});

describe("bridge ONNX cached resolution (#71 stale metadata)", () => {
  test("anchors cleanup/meta to the version root while returning the lib directory", () => {
    const versionDir = join(workDir, "onnxruntime", bridgeOnnxTest.ORT_VERSION);
    const libDir = join(versionDir, "lib");
    const libName = "libonnxruntime.so";
    mkdirSync(libDir, { recursive: true });
    writeFileSync(join(libDir, libName), "manual archive layout");
    writeFileSync(
      join(versionDir, bridgeOnnxTest.ONNX_INSTALLED_META_FILE),
      JSON.stringify({
        version: bridgeOnnxTest.ORT_VERSION,
        installedAt: "2026-01-01T00:00:00.000Z",
        sha256: "stale-root-meta",
      }),
    );

    const resolvedDir = bridgeOnnxTest.resolveCachedOnnxRuntimeDir(versionDir, libName);

    expect(resolvedDir).toBe(libDir);
    expect(existsSync(join(resolvedDir, libName))).toBe(true);

    // Regression guard: with stale root metadata and a lib-only layout, the
    // post-lock cleanup target must be the version root. Cleaning the resolved
    // lib/ dir would treat it as unowned and delete the real library, causing a
    // re-download loop.
    bridgeOnnxTest.cleanupIncompleteTargetIfUnowned(versionDir);

    expect(existsSync(join(versionDir, bridgeOnnxTest.ONNX_INSTALLED_META_FILE))).toBe(true);
    expect(existsSync(join(libDir, libName))).toBe(true);
  });
});
