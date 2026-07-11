import { existsSync, readdirSync, readlinkSync, realpathSync } from "node:fs";
import { basename, isAbsolute, join, resolve, win32 } from "node:path";

export const ONNX_RUNTIME_VERSION = "1.24.4";

export function getOnnxLibraryName(): string {
  if (process.platform === "darwin") return "libonnxruntime.dylib";
  if (process.platform === "win32") return "onnxruntime.dll";
  return "libonnxruntime.so";
}

export function getManualInstallHint(): string {
  const p = process.platform;
  const a = process.arch;
  if (p === "darwin") {
    if (a === "arm64") return "brew install onnxruntime (Apple Silicon)";
    return "Intel Mac requires manual install — see docs";
  }
  if (p === "linux") {
    if (a === "x64" || a === "arm64") {
      return "AFT auto-downloads ONNX Runtime on supported Linux (glibc)";
    }
    return "manual install required for this Linux arch";
  }
  if (p === "win32") {
    if (a === "x64" || a === "arm64") return "AFT auto-downloads ONNX Runtime on Windows";
    return "manual install required for this Windows arch";
  }
  return "ONNX Runtime must be installed manually for this platform";
}

function pathEnvValue(): string {
  return process.env.PATH ?? process.env.Path ?? process.env.path ?? "";
}

function pathEntriesForPlatform(): string[] {
  const delimiter = process.platform === "win32" ? ";" : ":";
  return pathEnvValue()
    .split(delimiter)
    .map((entry) => entry.trim().replace(/^"|"$/g, ""))
    .filter((entry) => {
      if (!entry || entry === "." || entry.includes("\0")) return false;
      return isAbsolute(entry) || win32.isAbsolute(entry);
    });
}

function isWindowsSystem32Directory(dir: string): boolean {
  if (process.platform !== "win32") return false;

  const normalizedDir = win32
    .resolve(dir)
    .replace(/[\\/]+$/, "")
    .toLowerCase();
  const windowsRoots = [process.env.SystemRoot, process.env.windir, "C:\\Windows"];
  return windowsRoots.some((root) => {
    if (!root) return false;
    return (
      normalizedDir ===
      win32
        .resolve(root, "System32")
        .replace(/[\\/]+$/, "")
        .toLowerCase()
    );
  });
}

function directoryContainsLibrary(dir: string, libName: string): boolean {
  try {
    const entries = readdirSync(dir);
    if (process.platform === "win32") {
      const expected = libName.toLowerCase();
      return entries.some((entry) => entry.toLowerCase() === expected);
    }
    return entries.includes(libName);
  } catch {
    return false;
  }
}

export function findIgnoredWindowsSystemOnnxRuntime(): string | null {
  if (process.platform !== "win32") return null;

  const windowsRoots = [process.env.SystemRoot, process.env.windir, "C:\\Windows"];
  const seen = new Set<string>();
  for (const root of windowsRoots) {
    if (!root) continue;
    const systemDir = join(root, "System32");
    const key = win32.resolve(systemDir).toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    if (directoryContainsLibrary(systemDir, getOnnxLibraryName())) return systemDir;
  }
  return null;
}

export function findSystemOnnxRuntime(): string | null {
  const libName = getOnnxLibraryName();
  const searchPaths: string[] = [];

  if (process.platform === "darwin") {
    searchPaths.push("/opt/homebrew/lib", "/usr/local/lib");
  } else if (process.platform === "linux") {
    searchPaths.push(
      "/usr/lib",
      "/usr/lib/x86_64-linux-gnu",
      "/usr/lib/aarch64-linux-gnu",
      "/usr/local/lib",
    );
  } else if (process.platform === "win32") {
    // Start with absolute PATH entries (via pathEntriesForPlatform) to
    // discover Scoop/manual-zip installs, then add common install paths.
    searchPaths.push(...pathEntriesForPlatform());
    const programFiles = process.env.ProgramFiles ?? "C:\\Program Files";
    const programFilesX86 = process.env["ProgramFiles(x86)"] ?? "C:\\Program Files (x86)";
    searchPaths.push(
      join(programFiles, "onnxruntime", "lib"),
      join(programFiles, "Microsoft ONNX Runtime", "lib"),
      join(programFiles, "Microsoft Machine Learning", "lib"),
      join(programFilesX86, "onnxruntime", "lib"),
      ...(() => {
        const nugetPaths: string[] = [];
        const userProfile = process.env.USERPROFILE ?? "";
        if (!userProfile) return nugetPaths;
        const nugetPackageDir = join(userProfile, ".nuget", "packages", "microsoft.ml.onnxruntime");
        if (!existsSync(nugetPackageDir)) return nugetPaths;
        try {
          for (const entry of readdirSync(nugetPackageDir, { withFileTypes: true })) {
            if (!entry.isDirectory()) continue;
            if (entry.name === "__globalPackagesFolder" || entry.name.startsWith(".")) continue;
            nugetPaths.push(
              join(nugetPackageDir, entry.name, "runtimes", "win-x64", "native"),
              join(nugetPackageDir, entry.name, "runtimes", "win-arm64", "native"),
            );
          }
        } catch {
          // Doctor probing is best-effort; ignore unreadable NuGet caches.
        }
        return nugetPaths;
      })(),
    );
  }
  // Deduplicate paths.
  // On case-insensitive filesystems (Windows, macOS) normalize casing for
  // comparison; on Linux the raw path casing is the authority.
  const normalizeCase = process.platform === "win32" || process.platform === "darwin";
  const seen = new Set<string>();
  const unknownVersionPaths: string[] = [];
  for (const dir of searchPaths) {
    let key = resolve(dir).replace(/[/\\]+$/, "");
    if (normalizeCase) key = key.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    if (!directoryContainsLibrary(dir, libName)) continue;

    const version = detectOrtVersion(dir);
    if (!version) {
      // Windows ships an unversioned ONNX Runtime in System32 on some releases.
      // The CLI cannot prove that OS copy meets AFT's minimum version, so report
      // it as unavailable and allow the bridge to install a managed runtime.
      if (isWindowsSystem32Directory(dir)) continue;
      unknownVersionPaths.push(dir);
      continue;
    }
    if (!isOrtVersionCompatible(version)) continue;
    return dir;
  }
  return unknownVersionPaths[0] ?? null;
}
export function findCachedOnnxRuntime(storageDir: string): string | null {
  const ortDir = join(storageDir, "onnxruntime", ONNX_RUNTIME_VERSION);
  const libName = getOnnxLibraryName();
  // Our own download flattens the library into the version root; a manual
  // install of Microsoft's archive leaves it under lib/. Accept either and
  // return the directory that actually contains the library (#71).
  if (existsSync(join(ortDir, libName))) return ortDir;
  const libSubdir = join(ortDir, "lib");
  if (existsSync(join(libSubdir, libName))) return libSubdir;
  return null;
}

const INVALID_ORT_VERSION = "<invalid>";

function parseOrtVersionFromPath(value: string): string | null {
  const name = basename(value);
  const semverish = name.match(
    /(?:^|[._-])(\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?)(?:\.(?:dylib|dll))?$/,
  );
  if (semverish) return semverish[1].split(/[-+]/, 1)[0];
  return /\d+\.\d+\.\d+/.test(name) ? INVALID_ORT_VERSION : null;
}

function parseOrtVersionFromDirectoryPath(value: string): string | null {
  const parts = value
    .split(/[\\/]+/)
    .filter(Boolean)
    .reverse();
  for (const part of parts) {
    const version = parseOrtVersionFromPath(part);
    if (version) return version;
  }
  return null;
}

/**
 * Detect an installed ONNX Runtime's advertised version by walking the
 * shared-library filename suffixes that Microsoft ships. Returns null when
 * the version can't be determined.
 */
export function detectOrtVersion(libDir: string): string | null {
  if (!existsSync(libDir)) return null;

  // Match libonnxruntime.so.1.24.4, libonnxruntime.1.24.4.dylib,
  // onnxruntime.1.24.4.dll, symlink targets, and Windows NuGet parent dirs.
  const libName = getOnnxLibraryName();
  try {
    const entries = readdirSync(libDir);
    const barePrefix = libName.replace(/\.(so|dylib|dll)$/, "");
    const expectedPrefix = process.platform === "win32" ? barePrefix.toLowerCase() : barePrefix;
    for (const entry of entries) {
      const comparable = process.platform === "win32" ? entry.toLowerCase() : entry;
      if (!comparable.startsWith(expectedPrefix)) continue;
      const version = parseOrtVersionFromPath(entry);
      if (version) return version;
    }

    // Fall back: libonnxruntime.so/.dylib or onnxruntime.dll → follow symlink.
    const base = join(libDir, libName);
    if (existsSync(base)) {
      try {
        const real = realpathSync(base);
        const version = parseOrtVersionFromPath(real) ?? parseOrtVersionFromDirectoryPath(real);
        if (version) return version;
      } catch {
        // ignore
      }
      try {
        const target = readlinkSync(base);
        const version = parseOrtVersionFromPath(target);
        if (version) return version;
      } catch {
        // not a symlink
      }
    }
    return parseOrtVersionFromDirectoryPath(libDir);
  } catch {
    // ignore
  }
  return null;
}

/** Minimum major.minor required by AFT's bundled ort crate. */
export const REQUIRED_ORT_MAJOR = 1;
export const REQUIRED_ORT_MIN_MINOR = 20;

export function isOrtVersionCompatible(version: string): boolean {
  const parts = version.split(".").map((p) => parseInt(p, 10));
  const [major, minor] = parts;
  if (!Number.isFinite(major) || !Number.isFinite(minor)) return false;
  if (major !== REQUIRED_ORT_MAJOR) return false;
  return minor >= REQUIRED_ORT_MIN_MINOR;
}
