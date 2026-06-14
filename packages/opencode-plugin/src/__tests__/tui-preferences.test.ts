import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  computeEffectiveOrder,
  DEFAULT_PREFS,
  DEFAULT_SLOT_ORDER,
  getTuiPreferencesFile,
  PLUGIN_KEY,
  queueTuiPreferenceUpdate,
  readTuiPreferencesFile,
  resolveAftPrefs,
  TUI_PREFS_FILE_ENV,
} from "../tui/preferences";

let dir: string;
let file: string;
const savedEnv: Record<string, string | undefined> = {};
const ENV_KEYS = [TUI_PREFS_FILE_ENV, "OPENCODE_CONFIG_DIR", "XDG_CONFIG_HOME"];

beforeEach(async () => {
  for (const key of ENV_KEYS) savedEnv[key] = process.env[key];
  dir = await mkdtemp(join(tmpdir(), "aft-tui-prefs-test-"));
  file = join(dir, "tui-preferences.jsonc");
  process.env[TUI_PREFS_FILE_ENV] = file;
});

afterEach(async () => {
  for (const key of ENV_KEYS) {
    if (savedEnv[key] === undefined) delete process.env[key];
    else process.env[key] = savedEnv[key];
  }
  await rm(dir, { recursive: true, force: true });
});

describe("getTuiPreferencesFile", () => {
  test("env override wins", () => {
    expect(getTuiPreferencesFile()).toBe(file);
  });

  test("OPENCODE_CONFIG_DIR beats XDG_CONFIG_HOME", () => {
    delete process.env[TUI_PREFS_FILE_ENV];
    process.env.OPENCODE_CONFIG_DIR = "/cfg/opencode-dir";
    process.env.XDG_CONFIG_HOME = "/xdg";
    expect(getTuiPreferencesFile()).toBe("/cfg/opencode-dir/tui-preferences.jsonc");
  });

  test("XDG_CONFIG_HOME fallback appends opencode/", () => {
    delete process.env[TUI_PREFS_FILE_ENV];
    delete process.env.OPENCODE_CONFIG_DIR;
    process.env.XDG_CONFIG_HOME = "/xdg";
    expect(getTuiPreferencesFile()).toBe("/xdg/opencode/tui-preferences.jsonc");
  });
});

describe("readTuiPreferencesFile", () => {
  test("missing file returns empty object", async () => {
    expect(await readTuiPreferencesFile()).toEqual({});
  });

  test("parses JSONC with comments and trailing commas", async () => {
    await writeFile(
      file,
      `// header comment\n{\n  // plugin\n  "aft": { "order": 5, },\n}\n`,
      "utf8",
    );
    const root = await readTuiPreferencesFile();
    expect(root).toEqual({ aft: { order: 5 } });
  });

  test("malformed file returns empty object", async () => {
    await writeFile(file, "{{{{ not json", "utf8");
    expect(await readTuiPreferencesFile()).toEqual({});
  });

  test("non-object root returns empty object", async () => {
    await writeFile(file, "[1, 2, 3]", "utf8");
    expect(await readTuiPreferencesFile()).toEqual({});
  });
});

describe("resolveAftPrefs", () => {
  test("empty root yields defaults", () => {
    const prefs = resolveAftPrefs({});
    expect(prefs).toEqual(DEFAULT_PREFS);
    expect(prefs.order).toBe(DEFAULT_SLOT_ORDER);
    expect(prefs.collapsed).toBeNull();
  });

  test("valid values pass through", () => {
    const prefs = resolveAftPrefs({
      aft: {
        forceToTop: true,
        order: -500,
        startCollapsed: true,
        rememberCollapsed: false,
        collapsed: true,
        header: { label: "TOOLS", showVersion: false },
        sections: { searchIndex: false, compression: false },
      },
    });
    expect(prefs.forceToTop).toBe(true);
    expect(prefs.order).toBe(-500);
    expect(prefs.startCollapsed).toBe(true);
    expect(prefs.rememberCollapsed).toBe(false);
    expect(prefs.collapsed).toBe(true);
    expect(prefs.header).toEqual({ label: "TOOLS", showVersion: false });
    expect(prefs.sections).toEqual({
      searchIndex: false,
      semanticIndex: true,
      codeHealth: true,
      compression: false,
    });
  });

  test("numbers are clamped to their ranges", () => {
    const prefs = resolveAftPrefs({
      aft: { order: 99999999 },
    });
    expect(prefs.order).toBe(10000);
  });

  test("label is truncated to 20 chars and empty label falls back", () => {
    const long = resolveAftPrefs({
      aft: { header: { label: "X".repeat(50) } },
    });
    expect(long.header.label).toBe("X".repeat(20));
    const empty = resolveAftPrefs({
      aft: { header: { label: "" } },
    });
    expect(empty.header.label).toBe("AFT");
  });

  test("wrong types fall back per key", () => {
    const prefs = resolveAftPrefs({
      aft: {
        forceToTop: "yes",
        order: "high",
        collapsed: "maybe",
        header: "big",
        sections: { searchIndex: 1 },
      },
    });
    expect(prefs.forceToTop).toBe(false);
    expect(prefs.order).toBe(DEFAULT_SLOT_ORDER);
    expect(prefs.collapsed).toBeNull();
    expect(prefs.header).toEqual(DEFAULT_PREFS.header);
    expect(prefs.sections.searchIndex).toBe(true);
  });

  test("non-object plugin entry yields defaults", () => {
    expect(resolveAftPrefs({ aft: 42 })).toEqual(DEFAULT_PREFS);
  });

  test("partial object merges with defaults", () => {
    const prefs = resolveAftPrefs({
      aft: { sections: { semanticIndex: false } },
    });
    expect(prefs.sections.semanticIndex).toBe(false);
    expect(prefs.sections.searchIndex).toBe(true);
    expect(prefs.rememberCollapsed).toBe(true);
  });
});

describe("computeEffectiveOrder", () => {
  test("missing key returns default order", () => {
    expect(computeEffectiveOrder({}, "aft", DEFAULT_SLOT_ORDER)).toBe(DEFAULT_SLOT_ORDER);
  });

  test("explicit order knob is used and clamped", () => {
    expect(computeEffectiveOrder({ aft: { order: 42 } }, "aft", DEFAULT_SLOT_ORDER)).toBe(42);
    expect(computeEffectiveOrder({ aft: { order: -99999999 } }, "aft", DEFAULT_SLOT_ORDER)).toBe(
      -10000,
    );
  });

  test("forceToTop beats any explicit order", () => {
    expect(
      computeEffectiveOrder(
        { aft: { forceToTop: true, order: -10000 } },
        "aft",
        DEFAULT_SLOT_ORDER,
      ),
    ).toBe(-100000);
  });

  test("multiple forced plugins order by key position in file", () => {
    const root = {
      "plugin-a": { forceToTop: true },
      "plugin-b": { order: 5 },
      "plugin-c": { forceToTop: true },
    };
    expect(computeEffectiveOrder(root, "plugin-a", 0)).toBe(-100000);
    expect(computeEffectiveOrder(root, "plugin-c", 0)).toBe(-99998);
    expect(computeEffectiveOrder(root, "plugin-b", 0)).toBe(5);
  });

  test("non-boolean forceToTop is ignored", () => {
    expect(computeEffectiveOrder({ aft: { forceToTop: "yes" } }, "aft", DEFAULT_SLOT_ORDER)).toBe(
      DEFAULT_SLOT_ORDER,
    );
  });
});

describe("queueTuiPreferenceUpdate interop", () => {
  test("anthropic-auth values and comments survive aft-only collapsed update", async () => {
    const original = `// my shared notes
{
  // anthropic-auth plugin
  "anthropic-auth": {
    "pollMs": 2000, // tuned
    "collapsed": false
  },
  "aft": {
    "startCollapsed": false
  }
}
`;
    await writeFile(file, original, "utf8");
    await queueTuiPreferenceUpdate(PLUGIN_KEY, ["collapsed"], true);
    const text = await readFile(file, "utf8");
    const root = await readTuiPreferencesFile();
    expect(root["anthropic-auth"]).toEqual({ pollMs: 2000, collapsed: false });
    expect((root.aft as Record<string, unknown>).collapsed).toBe(true);
    expect(() => readTuiPreferencesFile()).not.toThrow();
    // comment-json round-trips sibling comments faithfully (verified): the
    // sibling key's values AND both its block and inline comments must survive
    // AFT writing only its own key.
    expect(text).toContain("anthropic-auth");
    expect(text).toContain("pollMs");
    expect(text).toContain("// my shared notes");
    expect(text).toContain("// anthropic-auth plugin");
    expect(text).toContain("// tuned");
  });
});
