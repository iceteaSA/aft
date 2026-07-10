import { execSync } from "node:child_process";
import { existsSync, readFileSync, rmSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, parse, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { resolveAftLogPath, resolveCortexKitUserConfigPath } from "@cortexkit/aft-bridge";

import { dirSize } from "../lib/fs-util.js";
import { detectJsoncFile, readJsoncFile, writeJsoncFile } from "../lib/jsonc.js";
import { getCortexKitStorageRoot } from "../lib/paths.js";
import { getSelfVersion } from "../lib/self-version.js";
import type {
  HarnessAdapter,
  HarnessConfigPaths,
  PluginCacheInfo,
  PluginEntryResult,
} from "./types.js";

const PLUGIN_NAME = "@cortexkit/aft-opencode";
const PLUGIN_ENTRY = `${PLUGIN_NAME}@latest`;

function getOpenCodeConfigDir(): string {
  const envDir = process.env.OPENCODE_CONFIG_DIR?.trim();
  if (envDir) return resolve(envDir);
  const xdg = process.env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(xdg, "opencode");
}

function getOpenCodeCacheDir(): string {
  const xdg = process.env.XDG_CACHE_HOME;
  if (xdg) return join(xdg, "opencode");
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local");
    return join(localAppData, "opencode");
  }
  return join(homedir(), ".cache", "opencode");
}

/** True when the `opencode` CLI is runnable on PATH. */
function hasOpenCodeCli(): boolean {
  try {
    // timeout: a misbehaving/booting host must never hang the probe (and a hung
    // probe under PATH self-resolution is what enabled a fork bomb).
    execSync("opencode --version", { stdio: "ignore", timeout: 5000 });
    return true;
  } catch {
    return false;
  }
}

/**
 * True when an OpenCode Desktop app bundle exists in a known install location.
 * Used only as a last-resort signal when the config dir hasn't been created yet
 * (e.g. a freshly installed Desktop app that hasn't been launched).
 */
function openCodeDesktopAppExists(): boolean {
  const candidates: string[] = [];
  if (process.platform === "darwin") {
    candidates.push(
      "/Applications/OpenCode.app",
      "/Applications/OpenCode Beta.app",
      join(homedir(), "Applications", "OpenCode.app"),
      join(homedir(), "Applications", "OpenCode Beta.app"),
    );
  } else if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA ?? join(homedir(), "AppData", "Local");
    candidates.push(join(localAppData, "Programs", "opencode"), join(localAppData, "opencode"));
  } else {
    // Linux: common AppImage / package install hints.
    candidates.push(
      "/opt/OpenCode",
      "/usr/lib/opencode",
      join(homedir(), ".local", "share", "applications", "opencode.desktop"),
    );
  }
  return candidates.some((p) => {
    try {
      return existsSync(p);
    } catch {
      return false;
    }
  });
}

/**
 * Convert a plugin entry string to a filesystem path if it represents one.
 *
 * Plugin entries may be:
 * - npm package names: `@cortexkit/aft-opencode` (returns null)
 * - npm package@version: `@cortexkit/aft-opencode@latest` (returns null)
 * - file URLs: `file:///path/to/dir` (returns the resolved path)
 * - absolute Unix paths: `/Users/x/work/aft` (returns as-is)
 * - absolute Windows paths: `F:\path\to\plugin` or `C:/path/to/plugin` (returns as-is)
 */
function pathFromEntry(entry: string): string | null {
  if (entry.startsWith("file://")) {
    try {
      return fileURLToPath(entry);
    } catch {
      return null;
    }
  }
  if (entry.startsWith("/") || /^[A-Za-z]:[/\\]/.test(entry)) return entry;
  return null;
}

/**
 * Verify a path entry resolves to our actual plugin package by reading its
 * package.json and checking the name field. Required because the previous
 * substring-based heuristic (`includes("/opencode-plugin")`) produced false
 * positives for unrelated third-party plugins whose paths happened to contain
 * "opencode-plugin" — for example a user with
 * `file:///F:/hackingtool-plugin/opencode-plugin` in their config would have
 * AFT report itself as registered when it wasn't.
 */
function pathPointsToOurPlugin(entry: string): boolean {
  const fsPath = pathFromEntry(entry);
  if (!fsPath) return false;
  try {
    if (!existsSync(fsPath)) return false;
    let searchDir = statSync(fsPath).isDirectory() ? fsPath : dirname(fsPath);
    let pkgJsonPath: string | null = null;
    while (true) {
      const candidate = join(searchDir, "package.json");
      if (existsSync(candidate)) {
        pkgJsonPath = candidate;
        break;
      }
      const parent = dirname(searchDir);
      if (parent === searchDir || searchDir === parse(searchDir).root) break;
      searchDir = parent;
    }
    if (!pkgJsonPath) return false;
    const parsed = JSON.parse(readFileSync(pkgJsonPath, "utf-8")) as { name?: unknown };
    return parsed.name === PLUGIN_NAME;
  } catch {
    return false;
  }
}

function matchesPluginEntry(entry: string): boolean {
  if (entry === PLUGIN_NAME) return true;
  if (entry.startsWith(`${PLUGIN_NAME}@`)) return true;
  return pathPointsToOurPlugin(entry);
}

export class OpenCodeAdapter implements HarnessAdapter {
  readonly kind = "opencode" as const;
  readonly displayName = "OpenCode";
  readonly pluginPackageName = PLUGIN_NAME;
  readonly pluginEntryWithVersion = PLUGIN_ENTRY;

  isInstalled(): boolean {
    // OpenCode ships two ways: the `opencode` CLI (on PATH) and OpenCode
    // Desktop (an Electron app that does NOT put `opencode` on PATH). Both read
    // the same `~/.config/opencode` config and load the same plugin entry, so
    // "installed" must mean "OpenCode is present", not just "CLI is runnable" —
    // otherwise `aft setup` bails for Desktop-only users (issue: setup finds no
    // harness). getHostVersion() stays CLI-only and reports null for Desktop.
    //
    // Check cheap filesystem signals BEFORE shelling out: the config dir is
    // created by Desktop and the CLI alike and is exactly where we write the
    // plugin entry, so its presence both proves OpenCode is present and makes
    // setup meaningful — without booting `opencode --version` (slow, and the
    // probe that, under PATH self-resolution, enabled a fork bomb).
    if (existsSync(getOpenCodeConfigDir())) return true;
    // App bundle exists but config dir not yet created (freshly installed,
    // never launched).
    if (openCodeDesktopAppExists()) return true;
    // Last resort: the CLI is on PATH but hasn't created a config dir yet.
    return hasOpenCodeCli();
  }

  getHostVersion(): string | null {
    try {
      return execSync("opencode --version", {
        encoding: "utf-8",
        stdio: "pipe",
        timeout: 5000,
      }).trim();
    } catch {
      return null;
    }
  }

  detectConfigPaths(): HarnessConfigPaths {
    const configDir = getOpenCodeConfigDir();
    const harness = detectJsoncFile(configDir, "opencode");
    // AFT config lives in the shared CortexKit location since the v0.40.0
    // consolidation, not the per-harness opencode config dir. Use the bridge's
    // canonical path so the CLI and the plugin agree byte-for-byte (and so a
    // fresh `setup` creates aft.jsonc, the only name the plugin reads).
    const aftConfigPath = resolveCortexKitUserConfigPath();
    const aftConfigExists = existsSync(aftConfigPath);
    const tui = detectJsoncFile(configDir, "tui");
    return {
      configDir,
      harnessConfig: harness.path,
      harnessConfigFormat: harness.format,
      aftConfig: aftConfigPath,
      aftConfigFormat: aftConfigExists ? "jsonc" : "none",
      tuiConfig: tui.path,
      tuiConfigFormat: tui.format,
    };
  }

  hasPluginEntry(): boolean {
    const paths = this.detectConfigPaths();
    const { value } = readJsoncFile(paths.harnessConfig);
    const plugins = Array.isArray(value?.plugin) ? value.plugin : [];
    return plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
  }

  async ensurePluginEntry(): Promise<PluginEntryResult> {
    const paths = this.detectConfigPaths();
    const configPath = paths.harnessConfig;

    if (paths.harnessConfigFormat === "none") {
      // No existing file — create one with the plugin entry.
      const initial = { plugin: [PLUGIN_ENTRY] };
      writeJsoncFile(configPath, initial, "json");
      return {
        ok: true,
        action: "added",
        message: `Created ${configPath} and added ${PLUGIN_ENTRY}`,
        configPath,
      };
    }

    const { value, error } = readJsoncFile(configPath);
    if (error || !value) {
      return {
        ok: false,
        action: "error",
        message: `Could not parse ${configPath}: ${error ?? "unknown error"}`,
        configPath,
      };
    }

    const plugins = Array.isArray(value.plugin) ? value.plugin : [];
    const already = plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
    if (already) {
      return {
        ok: true,
        action: "already_present",
        message: `${PLUGIN_NAME} is already registered in ${configPath}`,
        configPath,
      };
    }

    plugins.push(PLUGIN_ENTRY);
    // Mutate in place so comment-json keeps symbol-keyed comment metadata on
    // the parsed object. Spreading into a fresh literal drops JSONC comments.
    value.plugin = plugins;
    writeJsoncFile(configPath, value, paths.harnessConfigFormat);
    return {
      ok: true,
      action: "added",
      message: `Added ${PLUGIN_ENTRY} to ${configPath}`,
      configPath,
    };
  }

  hasTuiPluginEntry(): boolean {
    const paths = this.detectConfigPaths();
    if (!paths.tuiConfig) return false;
    const { value } = readJsoncFile(paths.tuiConfig);
    const plugins = Array.isArray(value?.plugin) ? value.plugin : [];
    return plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
  }

  /**
   * Register the TUI sidebar plugin in tui.json(c). Only setup/doctor call
   * this: the plugin itself must never auto-inject the entry at load time,
   * because that silently undoes a user's deliberate removal on every launch.
   */
  async ensureTuiPluginEntry(): Promise<PluginEntryResult> {
    const paths = this.detectConfigPaths();
    const configPath = paths.tuiConfig;
    if (!configPath) {
      return {
        ok: false,
        action: "error",
        message: "No TUI config path detected",
        configPath: paths.configDir,
      };
    }

    if (paths.tuiConfigFormat === "none") {
      writeJsoncFile(configPath, { plugin: [PLUGIN_ENTRY] }, "json");
      return {
        ok: true,
        action: "added",
        message: `Created ${configPath} and added ${PLUGIN_ENTRY} (TUI sidebar)`,
        configPath,
      };
    }

    const { value, error } = readJsoncFile(configPath);
    if (error || !value) {
      return {
        ok: false,
        action: "error",
        message: `Could not parse ${configPath}: ${error ?? "unknown error"}`,
        configPath,
      };
    }

    const plugins = Array.isArray(value.plugin) ? value.plugin : [];
    const already = plugins.some((entry) => typeof entry === "string" && matchesPluginEntry(entry));
    if (already) {
      return {
        ok: true,
        action: "already_present",
        message: `${PLUGIN_NAME} is already registered in ${configPath}`,
        configPath,
      };
    }

    plugins.push(PLUGIN_ENTRY);
    // Mutate in place so comment-json keeps symbol-keyed comment metadata on
    // the parsed object. Spreading into a fresh literal drops JSONC comments.
    value.plugin = plugins;
    writeJsoncFile(configPath, value, paths.tuiConfigFormat);
    return {
      ok: true,
      action: "added",
      message: `Added ${PLUGIN_ENTRY} to ${configPath} (TUI sidebar)`,
      configPath,
    };
  }

  getPluginCacheInfo(): PluginCacheInfo {
    const path = join(getOpenCodeCacheDir(), "packages", PLUGIN_ENTRY);
    let cached: string | undefined;
    try {
      const installedPkgPath = join(
        path,
        "node_modules",
        "@cortexkit",
        "aft-opencode",
        "package.json",
      );
      if (existsSync(installedPkgPath)) {
        const pkg = JSON.parse(readFileSync(installedPkgPath, "utf-8")) as { version?: unknown };
        cached = typeof pkg.version === "string" ? pkg.version : undefined;
      }
    } catch {
      cached = undefined;
    }
    return {
      path,
      cached,
      latest: getSelfVersion(),
      exists: existsSync(path),
    };
  }

  getStorageDir(): string {
    return getCortexKitStorageRoot();
  }

  getLogFile(): string {
    return resolveAftLogPath("aft-plugin.log");
  }

  getInstallHint(): string {
    return "Install OpenCode: https://opencode.ai/docs/install";
  }

  async clearPluginCache(force: boolean): Promise<{
    action: "cleared" | "up_to_date" | "not_found" | "not_applicable" | "error";
    path: string;
    cached?: string;
    latest?: string;
    error?: string;
  }> {
    const info = this.getPluginCacheInfo();
    if (!info.exists) {
      return { action: "not_found", path: info.path };
    }
    if (!force && info.cached && info.cached === info.latest) {
      return {
        action: "up_to_date",
        path: info.path,
        cached: info.cached,
        latest: info.latest,
      };
    }
    try {
      rmSync(info.path, { recursive: true, force: true });
      return {
        action: "cleared",
        path: info.path,
        cached: info.cached,
        latest: info.latest,
      };
    } catch (error) {
      return {
        action: "error",
        path: info.path,
        cached: info.cached,
        latest: info.latest,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /** Exposed for diagnostic reporting — harness-specific side data. */
  getOpenCodeCacheDir(): string {
    return getOpenCodeCacheDir();
  }

  /** For doctor: directory size helpers for each storage subtree. */
  describeStorageSubtrees(): Record<string, number> {
    const storage = this.getStorageDir();
    return {
      index: dirSize(join(storage, "index")),
      semantic: dirSize(join(storage, "semantic")),
      backups: dirSize(join(storage, "backups")),
      url_cache: dirSize(join(storage, "url_cache")),
      onnxruntime: dirSize(join(storage, "onnxruntime")),
    };
  }
}
