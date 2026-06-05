import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

describe("OpenCode background callback bridge root routing", () => {
  test("push handlers derive directory from the callback bridge cwd", () => {
    const source = readFileSync(resolve(import.meta.dir, "../index.ts"), "utf8");

    expect(source).toContain("onBashCompletion: (completion, bridge) =>");
    expect(source).toContain("onBashLongRunning: (reminder, bridge) =>");
    expect(source).toContain("onBashPatternMatch: (frame, bridge) =>");
    expect(source.match(/const sessionDir = bridge\.getCwd\(\);/g)).toHaveLength(3);
    expect(source).not.toContain(
      "getSessionDirectoryCached(completion.session_id) ?? input.directory",
    );
    expect(source).not.toContain(
      "getSessionDirectoryCached(reminder.session_id) ?? input.directory",
    );
    expect(source).not.toContain("getSessionDirectoryCached(frame.session_id) ?? input.directory");
  });
});
