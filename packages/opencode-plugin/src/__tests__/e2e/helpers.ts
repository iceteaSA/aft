import { spawn } from "node:child_process";
import { constants, type Dirent } from "node:fs";
import { access, cp, mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import {
  BinaryBridge,
  type BridgeOptions,
  inlineUserConfigTier,
  setActiveLogger,
} from "@cortexkit/aft-bridge";
import { bridgeLogger } from "../../logger.js";

// Route aft-bridge log calls (including forwarded Rust child stderr lines like
// "[aft] invalidated 7 files") into $TMPDIR/aft-plugin-test.log instead of
// console.error. Without this, every "invalidated N files" / "watcher started"
// line emitted by the Rust child during e2e tests leaks onto test stdout and
// pollutes the bash background-completion output preview.
setActiveLogger(bridgeLogger);

// Remove a temp dir, tolerating the Windows `EBUSY: resource busy or locked`
// race: a detached background-bash child (or the bridge's own handles) can keep
// the temp directory open for a brief window after shutdown, so a single `rm`
// in a test's teardown throws and fails an otherwise-passing test. Cleanup
// failures must never fail a test — retry a few times, then give up silently
// (the OS reaps the temp dir, and a leaked temp dir is harmless in CI).
async function safeRemoveDir(dir: string): Promise<void> {
  for (let attempt = 0; attempt < 5; attempt++) {
    try {
      await rm(dir, { recursive: true, force: true });
      return;
    } catch {
      // Most commonly EBUSY on Windows while a detached child still holds a
      // handle. Back off briefly and retry; ignore if it never frees up.
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
}

// Windows cargo produces `aft.exe`; Unix produces `aft`. Resolve the
// platform-correct name so CI's fail-loud "binary must be present" guard does
// not trip on a name mismatch (Windows previously silent-skipped into a false
// green before the guard landed).
const AFT_BINARY_NAME = process.platform === "win32" ? "aft.exe" : "aft";
const TARGET_DEBUG_BINARY = resolve(
  import.meta.dir,
  `../../../../../target/debug/${AFT_BINARY_NAME}`,
);
const FALLBACK_BINARY = resolve(homedir(), ".cargo/bin", AFT_BINARY_NAME);
const PROJECT_ROOT = resolve(import.meta.dir, "../../../../../");
const FIXTURES_DIR = resolve(import.meta.dir, "./fixtures");
const DEFAULT_TIMEOUT_MS = 15_000;
const OUTLINE_EXTENSIONS = new Set([
  ".ts",
  ".tsx",
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".rs",
  ".go",
  ".py",
  ".rb",
  ".c",
  ".cpp",
  ".h",
  ".hpp",
  ".cs",
  ".java",
  ".kt",
  ".scala",
  ".swift",
  ".lua",
  ".ex",
  ".exs",
  ".hs",
  ".sol",
  ".nix",
  ".md",
  ".mdx",
  ".css",
  ".html",
  ".json",
  ".yaml",
  ".yml",
  ".sh",
  ".bash",
]);

const GROUP_A_CONFIGURE_KEYS = new Set([
  "storage_dir",
  "harness",
  "project_root",
  "bash_permissions",
  "lsp_paths_extra",
  "lsp_auto_install_binaries",
  "lsp_inflight_installs",
  "max_background_bash_tasks",
  "aft_search_registered",
  "_ort_dylib_dir",
  "_bypass_size_limits",
]);

const LEGACY_CONFIG_KEYS = new Set([
  "experimental_bash_rewrite",
  "experimental_bash_compress",
  "experimental_bash_background",
  "bash_long_running_reminder_enabled",
  "bash_long_running_reminder_interval_ms",
  "experimental_lsp_ty",
  "lsp_servers",
  "disabled_lsp",
]);

function objectDocValue(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? { ...(value as Record<string, unknown>) }
    : {};
}

/**
 * E2E migration shim: keep process-state configure params flat, but wrap legacy
 * flat aft.jsonc config overrides in an inline user config tier like the plugins do.
 */
export function configureParamsFromLegacyOverrides(
  overrides: Record<string, unknown>,
): Record<string, unknown> {
  const params: Record<string, unknown> = {};
  const doc: Record<string, unknown> = {};

  for (const [key, value] of Object.entries(overrides)) {
    if (GROUP_A_CONFIGURE_KEYS.has(key)) {
      params[key] = value;
    } else if (!LEGACY_CONFIG_KEYS.has(key)) {
      doc[key] = value;
    }
  }

  const hasBashLegacy =
    Object.hasOwn(overrides, "experimental_bash_rewrite") ||
    Object.hasOwn(overrides, "experimental_bash_compress") ||
    Object.hasOwn(overrides, "experimental_bash_background") ||
    Object.hasOwn(overrides, "bash_long_running_reminder_enabled") ||
    Object.hasOwn(overrides, "bash_long_running_reminder_interval_ms");
  if (hasBashLegacy) {
    const bash = objectDocValue(doc.bash);
    bash.rewrite = Object.hasOwn(overrides, "experimental_bash_rewrite")
      ? overrides.experimental_bash_rewrite
      : false;
    bash.compress = Object.hasOwn(overrides, "experimental_bash_compress")
      ? overrides.experimental_bash_compress
      : false;
    bash.background = Object.hasOwn(overrides, "experimental_bash_background")
      ? overrides.experimental_bash_background
      : false;
    if (Object.hasOwn(overrides, "bash_long_running_reminder_enabled")) {
      bash.long_running_reminder_enabled = overrides.bash_long_running_reminder_enabled;
    }
    if (Object.hasOwn(overrides, "bash_long_running_reminder_interval_ms")) {
      bash.long_running_reminder_interval_ms = overrides.bash_long_running_reminder_interval_ms;
    }
    doc.bash = bash;
  }

  if (Object.hasOwn(overrides, "experimental_lsp_ty")) {
    const experimental = objectDocValue(doc.experimental);
    experimental.lsp_ty = overrides.experimental_lsp_ty;
    doc.experimental = experimental;
  }

  if (Object.hasOwn(overrides, "lsp_servers") || Object.hasOwn(overrides, "disabled_lsp")) {
    const lsp = objectDocValue(doc.lsp);
    if (Object.hasOwn(overrides, "lsp_servers")) lsp.servers = overrides.lsp_servers;
    if (Object.hasOwn(overrides, "disabled_lsp")) lsp.disabled = overrides.disabled_lsp;
    doc.lsp = lsp;
  }

  if (Object.keys(doc).length > 0) {
    params.config = inlineUserConfigTier(doc);
  }

  return params;
}

const SKIP_DIRS = new Set([
  "node_modules",
  ".git",
  "dist",
  "build",
  "out",
  ".next",
  ".nuxt",
  "target",
  "__pycache__",
  ".venv",
  "venv",
  "vendor",
  ".turbo",
  "coverage",
  ".nyc_output",
  ".cache",
]);

export interface PreparedBinary {
  binaryPath: string | null;
  skipReason?: string;
  source: "target" | "fallback" | null;
  buildAttempted: boolean;
}

export interface E2EHarness {
  readonly binaryPath: string;
  readonly bridge: BinaryBridge;
  readonly tempDir: string;
  path(...segments: string[]): string;
  relativePath(...segments: string[]): string;
  cleanup(): Promise<void>;
}

export interface ReadLikePluginOptions {
  startLine?: number;
  endLine?: number;
  offset?: number;
  limit?: number;
}

let preparedBinaryPromise: Promise<PreparedBinary> | null = null;

export function prepareBinary(): Promise<PreparedBinary> {
  preparedBinaryPromise ??= prepareBinaryOnce();
  return preparedBinaryPromise;
}

export async function createHarness(
  preparedBinary: PreparedBinary,
  options?: {
    fixtureNames?: string[];
    timeoutMs?: number;
    tempPrefix?: string;
    bridgeOptions?: BridgeOptions;
  },
): Promise<E2EHarness> {
  if (!preparedBinary.binaryPath) {
    throw new Error(preparedBinary.skipReason ?? "aft binary unavailable");
  }

  // Keep e2e projects outside both the repository and OS temp directories so
  // external-directory tests cover ordinary out-of-project paths.
  const tempDir = await mkdtemp(
    join(dirname(PROJECT_ROOT), `.${options?.tempPrefix ?? "aft-plugin-e2e-"}`),
  );

  let bridge: BinaryBridge | undefined;
  try {
    await copyFixturesToTempDir(tempDir, options?.fixtureNames);

    // Redirect the search index cache to a temp dir so tests don't pollute the
    // user's ~/.cache/aft/index/. Pass AFT_CACHE_DIR via the bridge's per-child
    // env instead of mutating process.env: the child spawns lazily on the first
    // send(), so a process.env mutation scoped to construction would be restored
    // before the child ever inherits it — and process.env is process-global, so
    // concurrent harnesses would race. childEnv is applied at spawn time, scoped
    // to this child only.
    bridge = new BinaryBridge(
      preparedBinary.binaryPath,
      tempDir,
      {
        timeoutMs: options?.timeoutMs ?? DEFAULT_TIMEOUT_MS,
        childEnv: { AFT_CACHE_DIR: join(tempDir, ".aft-cache") },
        ...(options?.bridgeOptions ?? {}),
      },
      { harness: "opencode" },
    );
  } catch (err) {
    await safeRemoveDir(tempDir);
    throw err;
  }

  return {
    binaryPath: preparedBinary.binaryPath,
    bridge,
    tempDir,
    path: (...segments: string[]) => resolve(tempDir, ...segments),
    relativePath: (...segments: string[]) => segments.join("/"),
    cleanup: async () => {
      try {
        await bridge.shutdown();
      } catch {
        // ignore cleanup errors
      } finally {
        await safeRemoveDir(tempDir);
      }
    },
  };
}

export async function cleanupHarnesses(harnesses: E2EHarness[]): Promise<void> {
  await Promise.all(
    harnesses.splice(0, harnesses.length).map(async (harness) => {
      await harness.cleanup();
    }),
  );
}

export async function copyFixturesToTempDir(
  tempDir: string,
  fixtureNames?: string[],
): Promise<void> {
  const entries = fixtureNames ?? (await readdir(FIXTURES_DIR));
  await Promise.all(
    entries.map(async (entry) => {
      const source = resolve(FIXTURES_DIR, entry);
      const destination = resolve(tempDir, entry);
      await cp(source, destination, { recursive: true, force: true });
    }),
  );
}

export async function readTextFile(filePath: string): Promise<string> {
  return readFile(filePath, "utf8");
}

export function lineNumberText(text: string): string {
  return lineNumberRangeText(text, 1);
}

export function lineNumberRangeText(text: string, startLine: number, endLine?: number): string {
  const normalized = text.replace(/\r\n/g, "\n");
  const lines = normalized.split("\n");
  if (lines.at(-1) === "") {
    lines.pop();
  }

  const actualEnd = Math.min(endLine ?? lines.length, lines.length);
  const slice = lines.slice(Math.max(startLine - 1, 0), actualEnd);
  const width = String(Math.max(actualEnd, 1)).length;
  return slice
    .map((line, index) => `${String(startLine + index).padStart(width, " ")}: ${line}\n`)
    .join("");
}

export async function sendReadLikePlugin(
  bridge: BinaryBridge,
  filePath: string,
  options: ReadLikePluginOptions = {},
): Promise<Record<string, unknown>> {
  let startLine = options.startLine;
  let endLine = options.endLine;

  if (startLine === undefined && options.offset !== undefined) {
    startLine = options.offset;
    if (options.limit !== undefined) {
      endLine = options.offset + options.limit - 1;
    }
  }

  const params: Record<string, unknown> = { file: filePath };
  if (startLine !== undefined) params.start_line = startLine;
  if (endLine !== undefined) params.end_line = endLine;
  if (options.limit !== undefined && options.offset === undefined) {
    params.limit = options.limit;
  }

  return bridge.send("read", params);
}

export async function sendOutlineDirectoryLikePlugin(
  bridge: BinaryBridge,
  directory: string,
): Promise<Record<string, unknown>> {
  const files = await discoverOutlineFiles(directory);
  return bridge.send("outline", { files });
}

export async function sendZoomMultiSymbolLikePlugin(
  bridge: BinaryBridge,
  filePath: string,
  symbols: string[],
  contextLines?: number,
): Promise<Array<Record<string, unknown>>> {
  return Promise.all(
    symbols.map((symbol) => {
      const params: Record<string, unknown> = { file: filePath, symbol };
      if (contextLines !== undefined) {
        params.context_lines = contextLines;
      }
      return bridge.send("zoom", params);
    }),
  );
}

export async function discoverOutlineFiles(directory: string, maxFiles = 200): Promise<string[]> {
  const files: string[] = [];

  async function walk(current: string): Promise<void> {
    if (files.length >= maxFiles) return;

    let entries: Dirent<string>[];
    try {
      entries = await readdir(current, { withFileTypes: true, encoding: "utf8" });
    } catch {
      return;
    }

    for (const entry of entries) {
      if (files.length >= maxFiles) return;

      if (entry.isDirectory()) {
        if (!SKIP_DIRS.has(entry.name) && !entry.name.startsWith(".")) {
          await walk(resolve(current, entry.name));
        }
      } else if (entry.isFile()) {
        const ext = entry.name.slice(entry.name.lastIndexOf(".")).toLowerCase();
        if (OUTLINE_EXTENSIONS.has(ext)) {
          files.push(resolve(current, entry.name));
        }
      }
    }
  }

  await walk(directory);
  files.sort();
  return files;
}

async function resolveAftBinaryPath(candidates: string[]): Promise<string | undefined> {
  for (const candidate of candidates) {
    if (await isExecutable(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function debugBinaryCandidates(): string[] {
  return [TARGET_DEBUG_BINARY];
}

function fallbackBinaryCandidates(): string[] {
  return [FALLBACK_BINARY];
}

async function prepareBinaryOnce(): Promise<PreparedBinary> {
  const existing = await resolveAftBinaryPath(debugBinaryCandidates());
  if (existing) {
    return {
      binaryPath: existing,
      source: "target",
      buildAttempted: false,
    };
  }

  const build = await runCargoBuild();
  const built = await resolveAftBinaryPath(debugBinaryCandidates());
  if (built) {
    return {
      binaryPath: built,
      source: "target",
      buildAttempted: true,
    };
  }

  const fallback = await resolveAftBinaryPath(fallbackBinaryCandidates());
  if (fallback) {
    return {
      binaryPath: fallback,
      source: "fallback",
      buildAttempted: true,
    };
  }

  const searched = [...debugBinaryCandidates(), ...fallbackBinaryCandidates()]
    .map((path) => relative(PROJECT_ROOT, path))
    .join(" or ");
  const skipReason = build.ok
    ? `aft binary not found at ${searched}`
    : `cargo build failed and no fallback aft binary was found\n${build.output}`;

  // In CI the aft binary is always built before the Bun suites run, so a missing
  // binary there means the build/setup broke — fail loud instead of letting 25+
  // e2e files silently `describe.skipIf(!binaryPath)` into a false green. Locally
  // (CI unset) keep the quiet-skip behavior so contributors without a built
  // binary can still run the non-e2e suites.
  if (process.env.CI === "true") {
    throw new Error(
      `e2e setup failed: ${skipReason}\n` +
        "The aft binary must be present in CI (built before Bun tests run). " +
        "Refusing to silently skip e2e coverage.",
    );
  }

  return {
    binaryPath: null,
    source: null,
    buildAttempted: true,
    skipReason,
  };
}

async function isExecutable(filePath: string): Promise<boolean> {
  try {
    // Windows has no Unix execute bit; existence is enough for .exe discovery.
    const mode = process.platform === "win32" ? constants.F_OK : constants.X_OK;
    await access(filePath, mode);
    return true;
  } catch {
    return false;
  }
}

async function runCargoBuild(): Promise<{ ok: boolean; output: string }> {
  return new Promise((resolveBuild) => {
    const child = spawn("cargo", ["build"], {
      cwd: PROJECT_ROOT,
      stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";

    child.stdout?.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });

    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });

    child.on("error", (error) => {
      resolveBuild({ ok: false, output: error.message });
    });

    child.on("close", (code) => {
      const output = `${stdout}${stderr}`.trim();
      resolveBuild({ ok: code === 0, output });
    });
  });
}
