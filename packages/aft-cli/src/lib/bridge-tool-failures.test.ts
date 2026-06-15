import { describe, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir, userInfo } from "node:os";
import { join } from "node:path";
import {
  aggregateBridgeToolFailures,
  buildRecentAftToolFailuresSectionFromLog,
  formatRecentAftToolFailuresSection,
} from "./bridge-tool-failures.js";
import { capBodyToGithubLimit, MAX_GITHUB_BODY_BYTES } from "./issue-body.js";
import { sanitizeContent } from "./sanitize.js";

const FIXTURE_LOG = [
  `[2026-06-15T10:00:00.000Z] WARN [aft-plugin] [ses_abc123secret] Request "bash" (id=1) timed out after 30000ms`,
  `[2026-06-15T10:00:01.000Z] WARN [aft-plugin] [ses_abc123secret] Request "bash" (id=2) timed out after 30000ms`,
  `[2026-06-15T10:00:02.000Z] WARN [aft-plugin] Request "status" (id=3) timed out after 30000ms`,
  `[2026-06-15T10:00:03.000Z] WARN [aft-plugin] Request "status" (id=4) timed out after 30000ms`,
  `[2026-06-15T10:00:04.000Z] WARN [aft-plugin] Request "status" (id=5) timed out after 30000ms`,
  `[2026-06-15T10:00:05.000Z] ERROR [aft-plugin] Bridge killed after timeout.`,
  `[2026-06-15T10:00:06.000Z] ERROR [aft-plugin] Bridge killed after timeout.`,
  `[2026-06-15T10:00:07.000Z] INFO [aft-plugin] RPC error: aft.status => Error: busy`,
  `[2026-06-15T10:00:08.000Z] INFO [aft-plugin] unrelated info line`,
  `[2026-06-15T10:00:09.000Z] ERROR [aft-plugin] [ses_other] configure failed: /Users/${userInfo().username}/secret-repo/foo`,
].join("\n");

describe("aggregateBridgeToolFailures", () => {
  test("aggregates timeouts, bridge kills, and rpc errors with correct counts", () => {
    const counts = aggregateBridgeToolFailures(FIXTURE_LOG);

    expect(counts.get("bash: timed out after 30000ms")).toBe(2);
    expect(counts.get("status: timed out after 30000ms")).toBe(3);
    expect(counts.get("bridge killed after timeout")).toBe(2);
    expect(counts.get("rpc: RPC error (aft.status)")).toBe(1);
    expect(counts.get("error: ERROR log line")).toBe(1);
  });

  test("strips session tags and path-like tokens from keys via sanitizer", () => {
    const counts = aggregateBridgeToolFailures(FIXTURE_LOG);
    const keys = [...counts.keys()].join("\n");

    expect(keys).not.toContain("ses_abc123secret");
    expect(keys).not.toContain("ses_other");
    expect(keys).not.toMatch(/\/Users\/[^<]/);
    expect(keys).not.toContain(userInfo().username);
  });

  test("returns empty map for empty log", () => {
    expect(aggregateBridgeToolFailures("").size).toBe(0);
    expect(aggregateBridgeToolFailures("   \n").size).toBe(0);
  });
});

describe("formatRecentAftToolFailuresSection", () => {
  test("renders compact bullet list sorted by count descending", () => {
    const counts = aggregateBridgeToolFailures(FIXTURE_LOG);
    const section = formatRecentAftToolFailuresSection(counts);

    expect(section).toStartWith("### Recent AFT tool failures");
    expect(section).toContain("- status: timed out after 30000ms ×3");
    expect(section).toContain("- bash: timed out after 30000ms ×2");
    expect(section).toContain("- bridge killed after timeout ×2");

    const statusIdx = section.indexOf("status: timed out");
    const bashIdx = section.indexOf("bash: timed out");
    expect(statusIdx).toBeLessThan(bashIdx);
  });

  test("empty counts render honest no-failures message", () => {
    expect(formatRecentAftToolFailuresSection(new Map())).toBe(
      "### Recent AFT tool failures\nNo recent AFT tool failures recorded.",
    );
  });

  test("caps distinct failure classes with +N more", () => {
    const counts = new Map<string, number>();
    for (let i = 0; i < 35; i += 1) {
      counts.set(`tool${i}: timed out after 1000ms`, 1);
    }
    const section = formatRecentAftToolFailuresSection(counts, { maxClasses: 30 });
    expect(section).toContain("+5 more failure class(es) omitted");
    expect(section.split("\n").filter((l) => l.startsWith("- tool")).length).toBe(30);
  });
});

describe("buildRecentAftToolFailuresSectionFromLog", () => {
  test("reads fixture log from disk", () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-bridge-log-"));
    const logPath = join(dir, "fixture.log");
    writeFileSync(logPath, FIXTURE_LOG, "utf8");

    const section = buildRecentAftToolFailuresSectionFromLog(logPath, { tailBytes: 64 * 1024 });
    expect(section).toContain("status: timed out after 30000ms ×3");
  });

  test("missing log file yields no-failures message", () => {
    const section = buildRecentAftToolFailuresSectionFromLog(
      join(tmpdir(), "aft-nonexistent-bridge-log-xyz.log"),
    );
    expect(section).toContain("No recent AFT tool failures recorded.");
  });
});

describe("doctor --issue body integration", () => {
  function makeIssueBodyWithToolFailures(
    toolFailuresSection: string,
    logLineCount: number,
  ): string {
    const logLines: string[] = [];
    for (let i = 0; i < logLineCount; i += 1) {
      logLines.push(`LINE${String(i).padStart(6, "0")}: ${"x".repeat(180)}`);
    }
    return [
      "## Description",
      "Repro",
      "",
      "## Environment",
      "- AFT CLI: v0.39.2",
      "",
      "## Diagnostics",
      "_ok_",
      "",
      "## Recent errors (last 20, sanitized)",
      "_none_",
      "",
      toolFailuresSection,
      "",
      "## Logs (last 200 lines per harness)",
      "#### opencode log",
      "",
      "```",
      logLines.join("\n"),
      "```",
      "",
      "_Usernames and home paths have been stripped from this report._",
    ].join("\n");
  }

  test("tool failures section is included in assembled body and survives cap helper", () => {
    const section = formatRecentAftToolFailuresSection(aggregateBridgeToolFailures(FIXTURE_LOG));
    const raw = sanitizeContent(makeIssueBodyWithToolFailures(section, 5000));
    const capped = capBodyToGithubLimit(raw);

    expect(capped).toContain("### Recent AFT tool failures");
    expect(capped).toContain("bash: timed out after 30000ms ×2");
    expect(Buffer.byteLength(capped, "utf8")).toBeLessThanOrEqual(MAX_GITHUB_BODY_BYTES);
  });
});
