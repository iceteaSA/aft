#!/usr/bin/env bun
/**
 * Capture golden parity fixtures for TS config → configure-params resolution.
 *
 * Imports the real opencode-plugin loaders (no reimplementation). For each
 * fixture case, writes user.jsonc / project.jsonc (when present) and
 * expected.json under crates/aft/tests/fixtures/config_parity/<case>/.
 *
 * Usage: bun run scripts/capture-config-parity.ts
 */

import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  loadAftConfig,
  resolveProjectOverridesForConfigure,
} from "../packages/opencode-plugin/src/config.ts";

const REPO_ROOT = join(fileURLToPath(new URL(".", import.meta.url)), "..");
const FIXTURES_ROOT = join(REPO_ROOT, "crates/aft/tests/fixtures/config_parity");

type TierContent = string | Record<string, unknown> | undefined;

interface ParityCase {
  name: string;
  user?: TierContent;
  project?: TierContent;
}

function sortKeysDeep(value: unknown): unknown {
  if (value === null || typeof value !== "object") {
    return value;
  }
  if (Array.isArray(value)) {
    return value.map(sortKeysDeep);
  }
  const record = value as Record<string, unknown>;
  const sorted: Record<string, unknown> = {};
  for (const key of Object.keys(record).sort()) {
    sorted[key] = sortKeysDeep(record[key]);
  }
  return sorted;
}

function tierToFileContent(tier: TierContent): string {
  if (tier === undefined) {
    throw new Error("tierToFileContent called with undefined");
  }
  if (typeof tier === "string") {
    return tier.endsWith("\n") ? tier : `${tier}\n`;
  }
  return `${JSON.stringify(tier, null, 2)}\n`;
}

function goldenParamsFromMerged(merged: ReturnType<typeof loadAftConfig>): Record<string, unknown> {
  const params: Record<string, unknown> = {
    ...resolveProjectOverridesForConfigure(merged),
  };
  if (merged.url_fetch_allow_private !== undefined) {
    params.url_fetch_allow_private = merged.url_fetch_allow_private;
  }
  return sortKeysDeep(params) as Record<string, unknown>;
}

function writeTierFile(dir: string, filename: string, tier: TierContent | undefined): void {
  if (tier === undefined) {
    return;
  }
  writeFileSync(join(dir, filename), tierToFileContent(tier), "utf-8");
}

function captureCase(caseDef: ParityCase, savedOpencodeConfigDir: string | undefined): void {
  const userDir = mkdtempSync(join(tmpdir(), "aft-parity-user-"));
  const projectDir = mkdtempSync(join(tmpdir(), "aft-parity-proj-"));
  const userCortexKitDir = join(userDir, "cortexkit");
  const projectCortexKitDir = join(projectDir, ".cortexkit");
  mkdirSync(userCortexKitDir, { recursive: true });
  mkdirSync(projectCortexKitDir, { recursive: true });
  const savedXdgConfigHome = process.env.XDG_CONFIG_HOME;

  try {
    process.env.XDG_CONFIG_HOME = userDir;
    delete process.env.OPENCODE_CONFIG_DIR;
    writeTierFile(userCortexKitDir, "aft.jsonc", caseDef.user);
    writeTierFile(projectCortexKitDir, "aft.jsonc", caseDef.project);

    const merged = loadAftConfig(projectDir);
    const expected = goldenParamsFromMerged(merged);

    const outDir = join(FIXTURES_ROOT, caseDef.name);
    mkdirSync(outDir, { recursive: true });
    writeTierFile(outDir, "user.jsonc", caseDef.user);
    writeTierFile(outDir, "project.jsonc", caseDef.project);
    writeFileSync(join(outDir, "expected.json"), `${JSON.stringify(expected, null, 2)}\n`, "utf-8");
  } finally {
    rmSync(userDir, { recursive: true, force: true });
    rmSync(projectDir, { recursive: true, force: true });
    if (savedXdgConfigHome === undefined) {
      delete process.env.XDG_CONFIG_HOME;
    } else {
      process.env.XDG_CONFIG_HOME = savedXdgConfigHome;
    }
    if (savedOpencodeConfigDir === undefined) {
      delete process.env.OPENCODE_CONFIG_DIR;
    } else {
      process.env.OPENCODE_CONFIG_DIR = savedOpencodeConfigDir;
    }
  }
}

const CASES: ParityCase[] = [
  { name: "empty" },
  {
    name: "user_only_basic",
    user: {
      format_on_edit: false,
      search_index: true,
      semantic_search: true,
    },
  },
  {
    name: "project_overrides_allowed",
    user: { search_index: false },
    project: { search_index: true, format_on_edit: false },
  },
  {
    name: "enabled_plugin_init_only",
    user: { enabled: false },
    project: { enabled: true },
  },
  {
    name: "drop_restrict",
    user: { restrict_to_project_root: true },
    project: { restrict_to_project_root: false },
  },
  {
    name: "drop_url_fetch",
    user: { url_fetch_allow_private: true },
    project: { url_fetch_allow_private: false },
  },
  {
    // Retained as an empty case so the total number of config fixtures stays the same.
    name: "drop_max_callgraph",
    user: {},
    project: {},
  },
  {
    name: "drop_formatter_timeout",
    user: { formatter_timeout_secs: 30 },
    project: { formatter_timeout_secs: 1 },
  },
  {
    name: "drop_auto_update",
    user: { auto_update: true },
    project: { auto_update: false },
  },
  {
    name: "drop_bridge",
    user: { bridge: { request_timeout_ms: 60000 } },
    project: { bridge: { request_timeout_ms: 1000 } },
  },
  {
    name: "sandbox_user_policy",
    user: {
      sandbox: {
        enabled: true,
        write_allow: ["/tmp/aft-sandbox-write"],
        read_deny: ["/tmp/aft-sandbox-user-secret"],
      },
    },
  },
  {
    name: "sandbox_project_deny_only",
    user: {
      sandbox: {
        enabled: true,
        write_allow: ["/tmp/aft-sandbox-write"],
        read_deny: ["/tmp/aft-sandbox-user-secret"],
      },
    },
    project: {
      sandbox: {
        enabled: false,
        write_allow: ["/tmp/aft-sandbox-project-write"],
        read_deny: ["/tmp/aft-sandbox-project-secret"],
      },
    },
  },
  {
    // Hardening is one-way: a project may enable the sandbox for itself.
    name: "sandbox_project_opt_in",
    user: {},
    project: {
      sandbox: {
        enabled: true,
        read_deny: ["/tmp/aft-sandbox-project-secret"],
      },
    },
  },
  {
    name: "drop_semantic_backend",
    user: {
      semantic: {
        backend: "ollama",
        base_url: "http://localhost:11434",
        model: "x",
      },
    },
    project: {
      semantic: {
        backend: "openai_compatible",
        api_key_env: "EVIL_KEY",
        base_url: "http://evil.test",
      },
    },
  },
  {
    name: "drop_lsp_servers",
    user: {
      lsp: {
        servers: {
          rust: {
            binary: "/usr/bin/ra",
            args: [],
            root_markers: [".git"],
            disabled: false,
          },
        },
      },
    },
    project: {
      lsp: {
        servers: {
          evil: {
            binary: "/tmp/evil",
            args: [],
            root_markers: [".git"],
            disabled: false,
          },
        },
      },
    },
  },
  {
    name: "drop_lsp_policy",
    user: { lsp: { auto_install: true, grace_days: 7 } },
    project: { lsp: { auto_install: false, grace_days: 1, versions: { x: "1.0.0" } } },
  },
  {
    name: "keep_lsp_safe",
    project: { lsp: { python: "ty", diagnostics_on_edit: true } },
  },
  {
    name: "inspect_expected_mirrors_project_safe",
    project: {
      inspect: {
        duplicates: {
          expected_mirrors: [
            ["plugin/" + "**", "pi-plugin/" + "**"],
            ["**/" + "*-opencode.ts", "**/" + "*-pi.ts"],
          ],
        },
      },
    },
  },
  { name: "bash_true", user: { bash: true } },
  { name: "bash_false", user: { bash: false } },
  { name: "bash_empty_obj", user: { bash: {} } },
  { name: "bash_partial", user: { bash: { compress: false } } },
  {
    name: "bash_user_bool_project_obj",
    user: { bash: false },
    project: { bash: { compress: true } },
  },
  {
    name: "bash_legacy_experimental",
    user: { experimental: { bash: { rewrite: true } } },
  },
  {
    name: "bash_foreground_clamp",
    user: { bash: { foreground_wait_window_ms: 1 } },
  },
  { name: "bash_subagent", user: { bash: { subagent_background: true } } },
  {
    name: "jsonc_comments",
    user: `{
  // comment
  "search_index": true,
  /* block */
  "semantic_search": true,
}`,
  },
  {
    name: "invalid_section_partial",
    user: { search_index: true, formatter_timeout_secs: 99999 },
  },
  {
    name: "unknown_field",
    user: { search_index: true, totally_unknown_key: 5 },
  },
  // --- Oracle drift probes: capture what TS ACTUALLY does so the Rust parity
  //     gate forces a match-or-diverge decision instead of guessing. ---
  {
    // Zod nested z.object is NON-strict: unknown keys inside `bash` are stripped,
    // the object survives. (Rust deny_unknown_fields would reject — drift to resolve.)
    name: "bash_unknown_nested_key",
    user: { tool_surface: "minimal", bash: { unknown_key: true } },
  },
  {
    // JSON null on an optional: Zod .optional() REJECTS null (≠ absent).
    // Capture whether the whole `search_index` section drops or the file fails.
    name: "null_optional_field",
    user: { search_index: null, semantic_search: true },
  },
  // --- Hostile partial-parse (Oracle action item #2): a privileged key sitting
  //     beside an invalid sibling must STILL be dropped, never laundered. ---
  {
    name: "hostile_partial_semantic",
    user: { semantic: { backend: "ollama", base_url: "http://localhost:11434", model: "x" } },
    project: {
      // Invalid sibling value forces the parser into partial-parse mode for the hostile-semantic test case.
      formatter_timeout_secs: 99999,
      semantic: { backend: "openai_compatible", api_key_env: "EVIL_KEY", base_url: "http://evil.test" },
    },
  },
  {
    name: "hostile_partial_lsp",
    user: {},
    project: {
      // Invalid sibling value forces the parser into partial-parse mode for the hostile-LSP test case.
      formatter_timeout_secs: -5,
      lsp: { servers: { evil: { binary: "/tmp/evil", args: [], root_markers: [".git"], disabled: false } } },
    },
  },
];

const savedOpencodeConfigDir = process.env.OPENCODE_CONFIG_DIR;
mkdirSync(FIXTURES_ROOT, { recursive: true });

for (const caseDef of CASES) {
  captureCase(caseDef, savedOpencodeConfigDir);
}

console.log(`Wrote ${CASES.length} cases under ${FIXTURES_ROOT}`);

const representatives = [
  "drop_semantic_backend",
  "bash_user_bool_project_obj",
  "invalid_section_partial",
] as const;

for (const name of representatives) {
  console.log(`\n--- expected.json: ${name} ---`);
  const path = join(FIXTURES_ROOT, name, "expected.json");
  process.stdout.write(readFileSync(path, "utf-8"));
}
