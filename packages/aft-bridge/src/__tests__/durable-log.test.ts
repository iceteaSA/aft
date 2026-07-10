import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { RotatingLogSink, resolveAftLogPath, resolveAftStorageRoot } from "../durable-log.js";

const cleanup: string[] = [];
afterEach(() => {
  for (const path of cleanup.splice(0)) rmSync(path, { force: true, recursive: true });
});

describe("durable plugin logging", () => {
  test("resolves AFT_CACHE_DIR through the Rust-compatible aft subtree", () => {
    const previous = process.env.AFT_CACHE_DIR;
    process.env.AFT_CACHE_DIR = join(tmpdir(), "aft-cache-resolution");
    try {
      expect(resolveAftStorageRoot()).toBe(join(process.env.AFT_CACHE_DIR, "aft"));
      expect(resolveAftLogPath("aft-plugin.log")).toBe(
        join(process.env.AFT_CACHE_DIR, "aft", "logs", "aft-plugin.log"),
      );
    } finally {
      if (previous === undefined) delete process.env.AFT_CACHE_DIR;
      else process.env.AFT_CACHE_DIR = previous;
    }
  });

  test("rotates at the byte threshold and enforces the generation cap", async () => {
    const root = mkdtempSync(join(tmpdir(), "aft-durable-log-"));
    cleanup.push(root);
    const path = join(root, "logs", "aft-plugin.log");
    const sink = new RotatingLogSink(path, { maxBytes: 10, generations: 2 });

    for (const value of ["aaaa\n", "bbbb\n", "cccc\n", "dddd\n", "eeee\n"]) sink.append(value);
    await sink.drain();

    expect(readFileSync(path, "utf8")).toBe("eeee\n");
    expect(readFileSync(`${path}.1`, "utf8")).toBe("cccc\ndddd\n");
    expect(readFileSync(`${path}.2`, "utf8")).toBe("aaaa\nbbbb\n");
    expect(() => readFileSync(`${path}.3`, "utf8")).toThrow();
  });
});
