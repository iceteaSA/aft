/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { appendFileSync, mkdtempSync, truncateSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  collectDiagnosticIssues,
  type DiagnosticReport,
  findPluginCliVersionSkews,
  formatDiagnosticIssuesSection,
  type HarnessDiagnostic,
  renderDiagnosticsMarkdown,
  tailLogFile,
} from "../lib/diagnostics.js";

describe("tailLogFile", () => {
  test("tails a large log from the end", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-cli-tail-test-"));
    const path = join(dir, "large.log");
    writeFileSync(path, "start\n");
    truncateSync(path, 100 * 1024 * 1024);
    appendFileSync(path, "line-1\nline-2\nline-3\n");

    expect(tailLogFile(path, 2)).toBe("line-2\nline-3");
  });
});

function makeHarness(overrides: Partial<HarnessDiagnostic> = {}): HarnessDiagnostic {
  return {
    kind: "opencode",
    displayName: "OpenCode",
    hostInstalled: true,
    hostVersion: "test",
    pluginRegistered: true,
    configPaths: {
      configDir: "/tmp/aft-test",
      harnessConfig: "/tmp/aft-test/opencode.jsonc",
      harnessConfigFormat: "jsonc",
      aftConfig: "/tmp/aft-test/aft.jsonc",
      aftConfigFormat: "jsonc",
    },
    aftConfig: { exists: true, enabled: true, flags: {} },
    pluginCache: { path: "/tmp/aft-test/plugin-cache", exists: false },
    storageDir: { path: "/tmp/aft-test/storage", exists: true, accessible: true, sizesByKey: {} },
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

function makeReport(harness: HarnessDiagnostic): DiagnosticReport {
  return {
    timestamp: "2026-01-01T00:00:00.000Z",
    platform: "win32",
    arch: "x64",
    nodeVersion: "v24.0.0",
    cliVersion: "0.30.3",
    binaryVersion: "0.30.3",
    harnesses: [harness],
    binaryCache: { path: "/tmp/aft-test/bin", versions: [], totalSize: 0 },
    lspCache: {
      npm: { path: "/tmp/aft-test/npm", entries: [], totalSize: 0 },
      github: { path: "/tmp/aft-test/gh", entries: [], totalSize: 0 },
      totalSize: 0,
    },
  };
}

describe("diagnostic issue summaries", () => {
  test("reports plugin/CLI version skew as a high-severity issue", () => {
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/aft-test/plugin-cache",
          exists: true,
          cached: "0.29.1",
          latest: "0.30.3",
        },
      }),
    );

    const issues = collectDiagnosticIssues(report);
    const skews = findPluginCliVersionSkews(report);
    const section = formatDiagnosticIssuesSection(report).join("\n");

    expect(skews).toHaveLength(1);
    expect(skews[0]).toMatchObject({
      code: "plugin_cli_version_skew",
      severity: "high",
      scope: "OpenCode",
    });
    expect(section).toContain("--- Issues found ---");
    expect(section).toContain(
      "Plugin version (0.29.1) is older than CLI (0.30.3). New binary cache won't be used until you update the plugin.",
    );
    expect(section).toContain(
      "Remediation: Update `@cortexkit/aft-opencode` in your harness config to `@latest`.",
    );
    expect(section.includes(String.fromCharCode(27))).toBe(false);
    expect(issues.some((issue) => issue.code === "plugin_cli_version_skew")).toBe(true);
  });

  test("skips plugin/CLI skew when the plugin package is not installed", () => {
    const report = makeReport(
      makeHarness({ pluginCache: { path: "/tmp/missing", exists: false } }),
    );

    expect(findPluginCliVersionSkews(report)).toHaveLength(0);
  });

  test("doctor --issue markdown includes the issue summary and plugin version", () => {
    const report = makeReport(
      makeHarness({
        pluginCache: {
          path: "/tmp/aft-test/plugin-cache",
          exists: true,
          cached: "0.29.1",
          latest: "0.30.3",
        },
      }),
    );

    const markdown = renderDiagnosticsMarkdown(report);

    expect(markdown).toContain("### Issues found");
    expect(markdown).toContain(
      "**HIGH** OpenCode: Plugin version (0.29.1) is older than CLI (0.30.3)",
    );
    expect(markdown).toContain("- Plugin version: 0.29.1");
  });
});
