/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "../platform.js";
import { acquireEnv } from "./test-utils/env-guard.js";

const skipShellFixture =
  (process.platform === "linux" && process.env.CI === "true") || process.platform === "win32";

describe("downloadBinary hardened transport", () => {
  let tmpDir: string;
  let releaseEnv: (() => void) | undefined;
  let originalFetch: typeof fetch;

  beforeEach(async () => {
    tmpDir = mkdtempSync(join(tmpdir(), "aft-download-test-"));
    // getCacheDir() reads different env vars per platform:
    // Windows → LOCALAPPDATA, POSIX → XDG_CACHE_HOME
    const cacheEnv =
      process.platform === "win32" ? { LOCALAPPDATA: tmpDir } : { XDG_CACHE_HOME: tmpDir };
    releaseEnv = await acquireEnv(cacheEnv);
    originalFetch = globalThis.fetch;
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    globalThis.fetch = originalFetch;
    rmSync(tmpDir, { recursive: true, force: true });
  });

  function currentAssetName(): string {
    const platformKey = PLATFORM_ARCH_MAP[process.platform]?.[process.arch];
    const assetName = platformKey ? PLATFORM_ASSET_MAP[platformKey] : undefined;
    if (!assetName)
      throw new Error(`unsupported test platform ${process.platform}-${process.arch}`);
    return assetName;
  }

  test("dedupes concurrent same-version downloads and writes one final binary", async () => {
    if (skipShellFixture) return;
    const { downloadBinary, getBinaryName } = await import(
      `../downloader.js?transport-dedupe-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("#!/bin/sh\necho aft 1.2.3\n");
    const sha256 = createHash("sha256").update(payload).digest("hex");
    let binaryFetches = 0;

    globalThis.fetch = (async (url: string | URL | Request) => {
      const rawUrl = String(url);
      if (rawUrl.endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      binaryFetches += 1;
      return new Response(payload, {
        status: 200,
        headers: { "content-length": String(payload.byteLength) },
      });
    }) as typeof fetch;

    const [first, second] = await Promise.all([downloadBinary("v1.2.3"), downloadBinary("1.2.3")]);

    const expectedPath = join(tmpDir, "aft", "bin", "v1.2.3", getBinaryName());
    expect(first).toBe(expectedPath);
    expect(second).toBe(expectedPath);
    expect(binaryFetches).toBe(1);
    expect(existsSync(expectedPath)).toBe(true);
    expect(
      readdirSync(join(tmpDir, "aft", "bin", "v1.2.3")).filter((name) => name.includes(".tmp")),
    ).toEqual([]);
  });

  test("ensureBinary redownloads mismatched versioned cache entries", async () => {
    if (skipShellFixture) return;

    const { ensureBinary, getBinaryName, readBinaryVersion } = await import(
      `../downloader.js?ensure-cache-validate-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("#!/bin/sh\necho aft 1.2.3\n");
    const sha256 = createHash("sha256").update(payload).digest("hex");
    const versionedDir = join(tmpDir, "aft", "bin", "v1.2.3");
    const cachedPath = join(versionedDir, getBinaryName());
    let binaryFetches = 0;

    mkdirSync(versionedDir, { recursive: true });
    writeFileSync(cachedPath, '#!/bin/sh\necho "aft 9.9.9"\n');
    chmodSync(cachedPath, 0o755);
    expect(readBinaryVersion(cachedPath)).toBe("9.9.9");

    globalThis.fetch = (async (url: string | URL | Request) => {
      const rawUrl = String(url);
      if (rawUrl.endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      binaryFetches += 1;
      return new Response(payload, {
        status: 200,
        headers: { "content-length": String(payload.byteLength) },
      });
    }) as typeof fetch;

    await expect(ensureBinary("v1.2.3")).resolves.toBe(cachedPath);
    expect(binaryFetches).toBe(1);
    expect(readFileSync(cachedPath, "utf8")).toContain("1.2.3");
  });

  test("download lock release preserves a reclaimed lock owned by another process", async () => {
    const { __test__ } = await import(`../downloader.js?download-lock-${Date.now()}`);
    const lockDir = join(tmpDir, "lock-owner");
    const lockPath = join(lockDir, ".download.lock");
    mkdirSync(lockDir, { recursive: true });

    const release = await __test__.acquireDownloadLock(lockPath);
    writeFileSync(lockPath, "other-owner");
    release();

    expect(readFileSync(lockPath, "utf8")).toBe("other-owner");
  });

  test("rejects oversized advertised downloads before buffering", async () => {
    const { downloadBinary, getBinaryName } = await import(
      `../downloader.js?transport-oversize-${Date.now()}`
    );
    const assetName = currentAssetName();
    const payload = Buffer.from("small");
    const sha256 = createHash("sha256").update(payload).digest("hex");

    globalThis.fetch = (async (url: string | URL | Request) => {
      const rawUrl = String(url);
      if (rawUrl.endsWith("checksums.sha256")) {
        return new Response(`${sha256}  ${assetName}\n`, { status: 200 });
      }
      return new Response(payload, {
        status: 200,
        headers: { "content-length": String(201 * 1024 * 1024) },
      });
    }) as typeof fetch;

    await expect(downloadBinary("v1.2.4")).resolves.toBeNull();
    expect(existsSync(join(tmpDir, "aft", "bin", "v1.2.4", getBinaryName()))).toBe(false);
  });
});
