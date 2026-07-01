/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { HarnessAdapter, HarnessConfigPaths } from "../adapters/types.js";
import type { DiagnosticReport, HarnessDiagnostic } from "../lib/diagnostics.js";
import { AFT_SCHEMA_URL } from "../lib/jsonc.js";
import {
  buildDoctorFixPlan,
  type DoctorFixPlanItem,
  doctorSkewBinaryDownloadDecision,
  formatDoctorStorageStatus,
  shouldSkipDoctorFixConfirmation,
} from "./doctor.js";

function configPaths(kind: "opencode" | "pi" = "opencode"): HarnessConfigPaths {
  return {
    configDir: "/tmp/aft-test",
    harnessConfig: kind === "pi" ? "/tmp/aft-test/settings.json" : "/tmp/aft-test/opencode.jsonc",
    harnessConfigFormat: "jsonc",
    aftConfig: "/tmp/aft-test/aft.jsonc",
    aftConfigFormat: "jsonc",
  };
}

function makeAdapter(kind: "opencode" | "pi" = "opencode"): HarnessAdapter {
  const paths = configPaths(kind);
  return {
    kind,
    displayName: kind === "pi" ? "Pi" : "OpenCode",
    pluginPackageName: kind === "pi" ? "@cortexkit/aft-pi" : "@cortexkit/aft-opencode",
    pluginEntryWithVersion:
      kind === "pi" ? "npm:@cortexkit/aft-pi" : "@cortexkit/aft-opencode@latest",
    isInstalled: () => true,
    getHostVersion: () => "test",
    detectConfigPaths: () => paths,
    hasPluginEntry: () => false,
    ensurePluginEntry: async () => ({
      ok: true,
      action: "added",
      message: "registered",
      configPath: paths.harnessConfig,
    }),
    getPluginCacheInfo: () => ({ path: "/tmp/aft-test/plugin-cache", exists: false }),
    getStorageDir: () => "/tmp/aft-test/storage",
    getLogFile: () => "/tmp/aft-test/aft.log",
    getInstallHint: () => "install harness",
    clearPluginCache: async () => ({ action: "not_found", path: "/tmp/aft-test/plugin-cache" }),
  };
}

function makeHarness(overrides: Partial<HarnessDiagnostic> = {}): HarnessDiagnostic {
  const kind = (overrides.kind as "opencode" | "pi" | undefined) ?? "opencode";
  return {
    kind,
    displayName: kind === "pi" ? "Pi" : "OpenCode",
    hostInstalled: true,
    hostVersion: "test",
    pluginRegistered: true,
    configPaths: configPaths(kind),
    aftConfig: { exists: true, enabled: true, flags: {} },
    pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
    storageDir: { path: "/tmp/aft-test/storage", exists: false, accessible: false, sizesByKey: {} },
    onnxRuntime: {
      required: false,
      systemPath: null,
      systemVersion: null,
      systemCompatible: null,
      cachedPath: null,
      cachedVersion: null,
      cachedCompatible: null,
      platform: "test-test",
      installHint: "install onnx",
      requirement: ">=1.20",
    },
    logFile: { path: "/tmp/aft-test/aft.log", exists: false, sizeKb: 0 },
    ...overrides,
  };
}

function makeReport(
  harnesses: HarnessDiagnostic[],
  binaryVersion: string | null,
): DiagnosticReport {
  return {
    timestamp: "2026-01-01T00:00:00.000Z",
    platform: "darwin",
    arch: "arm64",
    nodeVersion: "v24.0.0",
    cliVersion: "0.30.1",
    binaryVersion,
    harnesses,
    binaryCache: { path: "/tmp/aft-test/bin", versions: [], totalSize: 0 },
    lspCache: {
      npm: { path: "/tmp/aft-test/npm", entries: [], totalSize: 0 },
      github: { path: "/tmp/aft-test/gh", entries: [], totalSize: 0 },
      totalSize: 0,
    },
  };
}

function messages(plan: DoctorFixPlanItem[]): string[] {
  return plan.map((item) => item.message);
}

// The `$schema` planning item depends on real on-disk state of the harness's
// aft.jsonc (a hardcoded /tmp path in these mocks), so exact-match plan
// assertions filter it out and a dedicated isolated test below covers it.
function nonSchemaMessages(plan: DoctorFixPlanItem[]): string[] {
  return plan.filter((item) => item.kind !== "schema").map((item) => item.message);
}

describe("doctor --fix planning", () => {
  test("lists plugin and binary mutations before applying fixes", () => {
    const report = makeReport([makeHarness({ pluginRegistered: false })], null);

    const plan = buildDoctorFixPlan([makeAdapter()], report);

    expect(nonSchemaMessages(plan)).toEqual([
      "Will add @cortexkit/aft-opencode@latest to /tmp/aft-test/opencode.jsonc",
      "Will download/cache the aft binary matching CLI v0.30.1",
    ]);
  });

  test("does not plan binary or plugin updates for a disabled registered harness", () => {
    const report = makeReport(
      [
        makeHarness({
          aftConfig: { exists: true, enabled: false, flags: { enabled: false } },
          pluginCache: {
            path: "/tmp/aft-test/plugin-cache",
            exists: true,
            cached: "0.29.0",
            latest: "0.30.1",
          },
        }),
      ],
      null,
    );

    expect(nonSchemaMessages(buildDoctorFixPlan([makeAdapter()], report))).toEqual([]);
  });

  test("describes Pi registration as a pi install mutation", () => {
    const report = makeReport(
      [makeHarness({ kind: "pi", displayName: "Pi", pluginRegistered: false })],
      "0.30.1",
    );

    const plan = buildDoctorFixPlan([makeAdapter("pi")], report);

    expect(nonSchemaMessages(plan)).toEqual([
      "Will run `pi install npm:@cortexkit/aft-pi` to register Pi",
    ]);
  });

  test("skips the confirmation prompt for explicit automation flags", () => {
    expect(shouldSkipDoctorFixConfirmation(["--yes"])).toBe(true);
    expect(shouldSkipDoctorFixConfirmation(["--ci"])).toBe(true);
  });

  test("warns that a skewed plugin will not use a freshly cached CLI binary", () => {
    const report = makeReport(
      [
        makeHarness({
          pluginCache: {
            path: "/tmp/aft-test/plugin-cache",
            exists: true,
            cached: "0.29.1",
            latest: "0.30.3",
          },
        }),
      ],
      null,
    );
    report.cliVersion = "0.30.3";

    const plan = buildDoctorFixPlan([makeAdapter()], report);

    expect(messages(plan)).toContain(
      "Will ask before caching CLI v0.30.3 because the installed plugin will not use it until updated",
    );
  });

  test("plans lazy storage directory creation only for registered plugins", () => {
    const report = makeReport(
      [
        makeHarness({
          storageDir: {
            path: "/tmp/aft-test/storage",
            exists: false,
            accessible: false,
            sizesByKey: {},
          },
        }),
      ],
      "0.30.1",
    );

    const plan = buildDoctorFixPlan([makeAdapter()], report);

    expect(messages(plan)).toContain("Will create AFT storage directory at /tmp/aft-test/storage");
  });

  test("plans a $schema fix when the harness aft config lacks the schema URL", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-cli-doctor-schema-"));
    const aftConfig = join(dir, "aft.jsonc");
    writeFileSync(aftConfig, JSON.stringify({ semantic_search: true }, null, 2));
    const adapter = makeAdapter();
    adapter.detectConfigPaths = () => ({
      configDir: dir,
      harnessConfig: join(dir, "opencode.jsonc"),
      harnessConfigFormat: "jsonc",
      aftConfig,
      aftConfigFormat: "jsonc",
    });
    const report = makeReport([makeHarness({ pluginRegistered: true })], "0.30.1");

    const plan = buildDoctorFixPlan([adapter], report);

    expect(messages(plan)).toContain(
      `Will add the AFT config $schema URL to ${aftConfig} (editor autocomplete + validation)`,
    );
  });

  test("does not plan a $schema fix when the schema URL is already present", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-cli-doctor-schema-"));
    const aftConfig = join(dir, "aft.jsonc");
    writeFileSync(aftConfig, JSON.stringify({ $schema: AFT_SCHEMA_URL }, null, 2));
    const adapter = makeAdapter();
    adapter.detectConfigPaths = () => ({
      configDir: dir,
      harnessConfig: join(dir, "opencode.jsonc"),
      harnessConfigFormat: "jsonc",
      aftConfig,
      aftConfigFormat: "jsonc",
    });
    const report = makeReport([makeHarness({ pluginRegistered: true })], "0.30.1");

    const plan = buildDoctorFixPlan([adapter], report);

    expect(plan.some((item) => item.kind === "schema")).toBe(false);
  });
});

describe("doctor skew download prompt decision", () => {
  test("defaults to skipping skewed binary downloads in non-interactive runs", () => {
    expect(doctorSkewBinaryDownloadDecision([])).toBe("skip");
    expect(doctorSkewBinaryDownloadDecision(["--ci"])).toBe("skip");
    expect(doctorSkewBinaryDownloadDecision(["--yes"])).toBe("proceed");
  });
});

describe("doctor storage wording", () => {
  test("explains registered-plugin storage is lazy-created", () => {
    expect(formatDoctorStorageStatus(makeHarness())).toContain(
      "not yet created (lazy — created on first tool call)",
    );
  });

  test("keeps plain not-created wording when the plugin is not registered", () => {
    const text = formatDoctorStorageStatus(makeHarness({ pluginRegistered: false }));

    expect(text).toContain("not created");
    expect(text).not.toContain("lazy");
  });
});
