/// <reference path="../bun-test.d.ts" />
import { afterAll, afterEach, beforeEach, describe, expect, mock, spyOn, test } from "bun:test";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const logMock = mock(() => {});
const warnMock = mock(() => {});

const checkerMocks = {
  extractChannel: mock(() => "latest"),
  findPluginEntry: mock(() => null as null | Record<string, unknown>),
  getCachedVersion: mock((_entry?: string) => "0.1.0" as string | null),
  getCurrentRuntimePackageJsonPath: mock(() => null as string | null),
  getLatestVersion: mock(async () => "0.1.0" as string | null),
  getLocalDevVersion: mock(() => null as string | null),
};

const cacheMocks = {
  preparePackageUpdate: mock(() => "/tmp/opencode" as string | null),
  resolveInstallContext: mock(() => ({ installDir: "/tmp/opencode" })),
  runNpmInstallSafe: mock(async () => ({ ok: true })),
};

mock.module("../logger.js", () => ({
  log: logMock,
  debug: mock(() => {}),
  warn: warnMock,
  error: mock(() => {}),
}));
mock.module("../hooks/auto-update-checker/checker.js", () => checkerMocks);
mock.module("../hooks/auto-update-checker/cache.js", () => cacheMocks);

afterAll(() => {
  mock.restore();
});

let importCounter = 0;
const tempRoots = new Set<string>();

function cleanupTempRoots(): void {
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
}

function freshIndexImport() {
  return import(`../hooks/auto-update-checker/index.ts?audit=${importCounter++}`);
}

function createStorageDir(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-auto-update-audit-"));
  tempRoots.add(root);
  return root;
}

function createCtx(withToast = true) {
  const showToast = mock(() => Promise.resolve(undefined));
  return {
    ctx: {
      directory: "/test",
      client: withToast ? { tui: { showToast } } : {},
    },
    showToast,
  };
}

async function waitFor(predicate: () => boolean, message: string): Promise<void> {
  const deadline = Date.now() + 1000;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error(message);
    await new Promise((resolve) => setTimeout(resolve, 0));
  }
}

beforeEach(() => {
  cleanupTempRoots();
  logMock.mockClear();
  warnMock.mockClear();
  checkerMocks.extractChannel.mockReset();
  checkerMocks.extractChannel.mockImplementation(() => "latest");
  checkerMocks.findPluginEntry.mockReset();
  checkerMocks.findPluginEntry.mockImplementation(() => null);
  checkerMocks.getCachedVersion.mockReset();
  checkerMocks.getCachedVersion.mockImplementation(() => "0.1.0");
  checkerMocks.getCurrentRuntimePackageJsonPath.mockReset();
  checkerMocks.getCurrentRuntimePackageJsonPath.mockImplementation(() => null);
  checkerMocks.getLatestVersion.mockReset();
  checkerMocks.getLatestVersion.mockImplementation(async () => "0.1.0");
  checkerMocks.getLocalDevVersion.mockReset();
  checkerMocks.getLocalDevVersion.mockImplementation(() => null);
  cacheMocks.preparePackageUpdate.mockReset();
  cacheMocks.preparePackageUpdate.mockImplementation(() => "/tmp/opencode");
  cacheMocks.resolveInstallContext.mockReset();
  cacheMocks.resolveInstallContext.mockImplementation(() => ({ installDir: "/tmp/opencode" }));
  cacheMocks.runNpmInstallSafe.mockReset();
  cacheMocks.runNpmInstallSafe.mockImplementation(async () => ({ ok: true }));
});

afterEach(() => {
  cleanupTempRoots();
});

describe("auto-update audit regressions", () => {
  test("missing TUI toast API does not abort the background update path", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => null);
    const storageDir = createStorageDir();
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx } = createCtx(false);

    createAutoUpdateCheckerHook(ctx as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      storageDir,
      initDelayMs: 0,
    });

    await waitFor(
      () => checkerMocks.findPluginEntry.mock.calls.length > 0,
      "timed out waiting for non-TUI auto-update check",
    );
    expect(warnMock).not.toHaveBeenCalledWith(
      expect.stringContaining("Background update check failed"),
    );
  });

  test("check lock remains held until the update check completes", async () => {
    checkerMocks.findPluginEntry.mockImplementation(() => ({
      entry: "@cortexkit/aft-opencode@latest",
      pinnedVersion: null,
      isPinned: false,
      configPath: "/config/opencode.jsonc",
    }));
    let resolveLatest: (value: string | null) => void = () => {};
    checkerMocks.getLatestVersion.mockImplementation(
      () =>
        new Promise<string | null>((resolve) => {
          resolveLatest = resolve;
        }),
    );
    const storageDir = createStorageDir();
    const lockPath = join(storageDir, "opencode", "last-update-check.json.lock");
    const { createAutoUpdateCheckerHook } = await freshIndexImport();
    const { ctx: ctx1 } = createCtx();
    const { ctx: ctx2 } = createCtx();

    createAutoUpdateCheckerHook(ctx1 as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      storageDir,
      initDelayMs: 0,
    });
    await waitFor(
      () => checkerMocks.getLatestVersion.mock.calls.length === 1 && existsSync(lockPath),
      "timed out waiting for held auto-update lock",
    );

    createAutoUpdateCheckerHook(ctx2 as Parameters<typeof createAutoUpdateCheckerHook>[0], {
      showStartupToast: false,
      storageDir,
      initDelayMs: 0,
    });
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(checkerMocks.getLatestVersion).toHaveBeenCalledTimes(1);

    resolveLatest("0.1.0");
    await waitFor(() => !existsSync(lockPath), "timed out waiting for auto-update lock release");
  });

  test("reclaims a fresh lock whose owner process has exited", async () => {
    const storageDir = createStorageDir();
    const lockDir = join(storageDir, "opencode");
    const lockPath = join(lockDir, "last-update-check.json.lock");
    mkdirSync(lockDir, { recursive: true });
    const child = spawnSync(process.execPath, ["-e", ""]);
    expect(child.status).toBe(0);
    if (!child.pid) throw new Error("spawned child did not report a pid");
    writeFileSync(lockPath, JSON.stringify({ pid: child.pid, startedMs: Date.now() }));

    const { __test__ } = await freshIndexImport();
    const lock = __test__.claimCheckSlot(storageDir, 0);

    expect(lock).not.toBeNull();
    const replacement = JSON.parse(readFileSync(lockPath, "utf-8"));
    expect(replacement.pid).toBe(process.pid);
    expect(typeof replacement.startedMs).toBe("number");
    lock?.release();
    expect(existsSync(lockPath)).toBe(false);
  });

  test("reclaims an old empty lock using its filesystem timestamp", async () => {
    const storageDir = createStorageDir();
    const lockDir = join(storageDir, "opencode");
    const lockPath = join(lockDir, "last-update-check.json.lock");
    mkdirSync(lockDir, { recursive: true });
    writeFileSync(lockPath, "");
    const old = new Date(Date.now() - 61 * 60 * 1000);
    utimesSync(lockPath, old, old);

    const { __test__ } = await freshIndexImport();
    const lock = __test__.claimCheckSlot(storageDir, 0);

    expect(lock).not.toBeNull();
    expect(JSON.parse(readFileSync(lockPath, "utf-8")).pid).toBe(process.pid);
    lock?.release();
    expect(existsSync(lockPath)).toBe(false);
  });

  test("does not reclaim a fresh lock owned by a live process", async () => {
    const storageDir = createStorageDir();
    const lockDir = join(storageDir, "opencode");
    const lockPath = join(lockDir, "last-update-check.json.lock");
    mkdirSync(lockDir, { recursive: true });
    const owner = { pid: process.pid, startedMs: Date.now() };
    writeFileSync(lockPath, JSON.stringify(owner));

    const killSpy = spyOn(process, "kill");
    const { __test__ } = await freshIndexImport();
    const lock = __test__.claimCheckSlot(storageDir, 0);

    expect(lock).toBeNull();
    expect(killSpy).toHaveBeenCalledWith(process.pid, 0);
    expect(JSON.parse(readFileSync(lockPath, "utf-8"))).toEqual(owner);
    killSpy.mockRestore();
  });

  test("release preserves a replacement lock owned by another acquisition", async () => {
    const storageDir = createStorageDir();
    const lockPath = join(storageDir, "opencode", "last-update-check.json.lock");
    const { __test__ } = await freshIndexImport();
    const lock = __test__.claimCheckSlot(storageDir, 0);
    expect(lock).not.toBeNull();

    const replacement = { pid: process.pid, startedMs: Date.now() + 1 };
    writeFileSync(lockPath, JSON.stringify(replacement));
    lock?.release();

    expect(existsSync(lockPath)).toBe(true);
    expect(JSON.parse(readFileSync(lockPath, "utf-8"))).toEqual(replacement);
  });
});
