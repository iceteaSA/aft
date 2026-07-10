import { homedir } from "node:os";
import { join } from "node:path";

/** `~/.cache/aft/bin/` (or the platform equivalent) — same as plugin's `getCacheDir`. */
export function getAftBinaryCacheDir(): string {
  if (process.env.AFT_CACHE_DIR) {
    return join(process.env.AFT_CACHE_DIR, "bin");
  }
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "bin");
  }
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "bin");
}

export function getAftBinaryName(): string {
  return process.platform === "win32" ? "aft.exe" : "aft";
}

/**
 * Root of the LSP package cache populated by the OpenCode/Pi plugin.
 *
 * `~/.cache/aft/lsp-packages/<urlencoded-pkg>/node_modules/.bin/<binary>` for
 * npm-distributed servers (typescript-language-server, pyright, etc.).
 */
export function getAftLspPackagesDir(): string {
  if (process.env.AFT_CACHE_DIR) {
    return join(process.env.AFT_CACHE_DIR, "lsp-packages");
  }
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "lsp-packages");
  }
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "lsp-packages");
}

/**
 * Root of the LSP binary cache populated by the OpenCode/Pi plugin.
 *
 * `~/.cache/aft/lsp-binaries/<id>/bin/<binary>` for GitHub-distributed
 * servers (clangd, lua-ls, zls, tinymist, texlab).
 */
export function getAftLspBinariesDir(): string {
  if (process.env.AFT_CACHE_DIR) {
    return join(process.env.AFT_CACHE_DIR, "lsp-binaries");
  }
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "lsp-binaries");
  }
  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "lsp-binaries");
}

function homeDir(): string {
  if (process.platform === "win32") return process.env.USERPROFILE || process.env.HOME || homedir();
  return process.env.HOME || homedir();
}

function dataHome(): string {
  if (process.env.XDG_DATA_HOME) return process.env.XDG_DATA_HOME;
  if (process.platform === "win32") {
    return process.env.LOCALAPPDATA || process.env.APPDATA || join(homeDir(), "AppData", "Local");
  }
  return join(homeDir(), ".local", "share");
}

export function getCortexKitStorageRoot(): string {
  if (process.env.AFT_CACHE_DIR) return join(process.env.AFT_CACHE_DIR, "aft");
  return join(dataHome(), "cortexkit", "aft");
}
