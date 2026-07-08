/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { summarizeLegacyPartitionDuplication } from "./legacy-storage.js";

describe("summarizeLegacyPartitionDuplication", () => {
  test("groups legacy callgraph and inspect bytes by harness", () => {
    const root = mkdtempSync(join(tmpdir(), "aft-cli-legacy-storage-"));

    mkdirSync(join(root, "opencode", "callgraph"), { recursive: true });
    writeFileSync(join(root, "opencode", "callgraph", "0123456789abcdef.current"), "pointer");
    writeFileSync(join(root, "opencode", "callgraph", "0123456789abcdef.g1.1.sqlite"), "db");
    writeFileSync(join(root, "opencode", "callgraph", "0123456789abcdef.g1.1.sqlite-wal"), "wal");
    writeFileSync(join(root, "opencode", "callgraph", "0123456789abcdef.current.tmp.1"), "tmp");

    mkdirSync(join(root, "opencode", "inspect"), { recursive: true });
    writeFileSync(join(root, "opencode", "inspect", "fedcba9876543210.sqlite"), "inspect-db");
    writeFileSync(join(root, "opencode", "inspect", "fedcba9876543210.sqlite-wal"), "sidecar");

    mkdirSync(join(root, "pi", "inspect"), { recursive: true });
    writeFileSync(join(root, "pi", "inspect", "1111111111111111.sqlite"), "pi-db");

    mkdirSync(join(root, "index"), { recursive: true });
    writeFileSync(join(root, "index", "shared-cache.sqlite"), "ignored");

    expect(summarizeLegacyPartitionDuplication(root)).toEqual({
      totalPartitions: 3,
      totalBytes: 34,
      byHarness: [
        {
          harness: "opencode",
          partitions: 2,
          bytes: 29,
        },
        {
          harness: "pi",
          partitions: 1,
          bytes: 5,
        },
      ],
    });
  });
});
