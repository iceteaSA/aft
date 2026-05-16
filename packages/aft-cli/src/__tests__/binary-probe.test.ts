/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { probeBinaryVersion } from "../lib/binary-probe.js";
import { getAftBinaryName } from "../lib/paths.js";

describe("probeBinaryVersion", () => {
  const originalCacheDir = process.env.AFT_CACHE_DIR;

  afterEach(() => {
    if (originalCacheDir === undefined) {
      process.env.AFT_CACHE_DIR = undefined;
    } else {
      process.env.AFT_CACHE_DIR = originalCacheDir;
    }
  });

  test("uses spawn argv against the binary resolved by findAftBinary", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-binary-probe-test-"));
    process.env.AFT_CACHE_DIR = root;
    const binDir = join(root, "bin", "v9.8.7");
    mkdirSync(binDir, { recursive: true });
    const binaryPath = join(binDir, getAftBinaryName());
    writeFileSync(binaryPath, '#!/bin/sh\nprintf "aft 9.8.7\\n"\n');
    chmodSync(binaryPath, 0o755);

    expect(probeBinaryVersion("9.8.7")).toBe("9.8.7");
  });
});
