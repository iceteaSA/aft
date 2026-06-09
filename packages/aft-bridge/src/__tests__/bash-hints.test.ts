import { describe, expect, test } from "bun:test";
import {
  commandLeadsWithCodeSearch,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
} from "../bash-hints.js";

const AFT_SEARCH_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";
const GREP_TOOL_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

describe("maybeAppendConflictsHint", () => {
  test("appends hint on real git-merge conflict output", () => {
    const output = [
      "Auto-merging packages/opencode-plugin/src/index.ts",
      "CONFLICT (content): Merge conflict in packages/opencode-plugin/src/index.ts",
      "Automatic merge failed; fix conflicts and then commit the result.",
    ].join("\n");
    expect(maybeAppendConflictsHint(output)).toContain("[Hint] Use aft_conflicts");
  });

  test("appends hint on rebase conflict output", () => {
    const output = [
      "error: could not apply 0e3f4a2... feat: add foo",
      "Automatic merge failed; fix conflicts and then commit the result.",
    ].join("\n");
    expect(maybeAppendConflictsHint(output)).toContain("[Hint] Use aft_conflicts");
  });

  // The trigger string appears verbatim in many docs/READMEs. The hint must NOT
  // fire just because someone cat'd a README in a non-git directory.
  test("does NOT append hint when marker appears alone (e.g. README excerpt)", () => {
    const output =
      "When git can't merge automatically, you'll see:\n\n" +
      "  Automatic merge failed; fix conflicts and then commit the result.\n\n" +
      "This means you need to resolve the conflict manually.";
    expect(maybeAppendConflictsHint(output)).toBe(output);
  });

  test("does NOT fire on mid-line CONFLICT substring", () => {
    const output = "we documented: Automatic merge failed; fix conflicts. (see CONFLICT (3) below)";
    expect(maybeAppendConflictsHint(output)).toBe(output);
  });

  test("does NOT append hint when output is unrelated to git", () => {
    const output = "hello world\n+0/-0\nlinting passed.";
    expect(maybeAppendConflictsHint(output)).toBe(output);
  });

  test("does NOT append hint when output is empty", () => {
    expect(maybeAppendConflictsHint("")).toBe("");
  });
});

describe("commandLeadsWithCodeSearch", () => {
  const positives = [
    'grep -nE "x" src/',
    "grep foo file.ts | head",
    "rg -n pat",
    "cd packages/x && grep -rn foo .",
    "cd \"my dir\" && rg 'p' .",
    '"grep" -n pat file',
    "grep pat file || true",
    'grep "a|b" file | head',
  ];

  const negatives = [
    "bun test | grep fail",
    "cargo build 2>&1 | rg error",
    "echo hi | grep h",
    "make test | grep -i pass",
    "ls -la",
    "cd x; grep foo",
    "FOO=1 grep pat file",
    "2>&1 grep pat",
    "false || grep pat",
    'cd "unclosed && grep foo',
  ];

  for (const command of positives) {
    test(`positive: ${command}`, () => {
      expect(commandLeadsWithCodeSearch(command)).toBe(true);
    });
  }

  for (const command of negatives) {
    test(`negative: ${command}`, () => {
      expect(commandLeadsWithCodeSearch(command)).toBe(false);
    });
  }
});

describe("maybeAppendGrepSearchHint", () => {
  test("appends aft_search hint for a leading grep when aft_search is registered", () => {
    const result = maybeAppendGrepSearchHint("matches", "grep foo file.ts", true);
    expect(result).toBe(`matches\n\n${AFT_SEARCH_HINT}`);
  });

  test("appends grep-tool hint for a leading grep when aft_search is not registered", () => {
    const result = maybeAppendGrepSearchHint("matches", "grep foo file.ts", false);
    expect(result).toBe(`matches\n\n${GREP_TOOL_HINT}`);
  });

  test("does NOT append for a piped filtering grep", () => {
    const output = "failure details";
    expect(maybeAppendGrepSearchHint(output, "bun test | grep fail", true)).toBe(output);
    expect(maybeAppendGrepSearchHint(output, "bun test | grep fail", false)).toBe(output);
  });

  test("does NOT append when output is empty", () => {
    expect(maybeAppendGrepSearchHint("", "grep foo file.ts", true)).toBe("");
  });

  test("does NOT double-append an existing grep search hint", () => {
    const output = `matches\n\n${AFT_SEARCH_HINT}`;
    expect(maybeAppendGrepSearchHint(output, "grep foo file.ts", true)).toBe(output);
  });
});
