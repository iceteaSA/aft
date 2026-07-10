import { appendFile, mkdir, rename, rm, stat } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export const DEFAULT_LOG_BYTES = 20 * 1024 * 1024;
export const DEFAULT_LOG_GENERATIONS = 5;

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

/** Resolve the same storage root used by the Rust module. */
export function resolveAftStorageRoot(configuredRoot?: string): string {
  if (configuredRoot) return configuredRoot;
  if (process.env.AFT_CACHE_DIR) return join(process.env.AFT_CACHE_DIR, "aft");
  return join(dataHome(), "cortexkit", "aft");
}

export function resolveAftLogPath(filename: string, configuredRoot?: string): string {
  return join(resolveAftStorageRoot(configuredRoot), "logs", filename);
}

export interface RotatingLogOptions {
  maxBytes?: number;
  generations?: number;
}

/**
 * Asynchronous append-only sink with bounded size rotation.
 *
 * Callers only enqueue strings; directory creation, stat, append, and rename
 * operations run on a serialized promise chain outside the logging hot path.
 */
export class RotatingLogSink {
  readonly path: string;
  private readonly maxBytes: number;
  private readonly generations: number;
  private estimatedBytes: number | null = null;
  private queue: Promise<void> = Promise.resolve();
  private disabled = false;
  private failureReported = false;

  constructor(path: string, options: RotatingLogOptions = {}) {
    this.path = path;
    this.maxBytes = options.maxBytes ?? DEFAULT_LOG_BYTES;
    this.generations = options.generations ?? DEFAULT_LOG_GENERATIONS;
  }

  append(data: string): void {
    if (this.disabled || data.length === 0) return;
    this.queue = this.queue
      .then(() => this.write(data))
      .catch((error: unknown) => {
        this.disabled = true;
        if (!this.failureReported) {
          this.failureReported = true;
          try {
            process.stderr.write(
              `[aft-plugin] durable log disabled for ${this.path}: ${error instanceof Error ? error.message : String(error)}\n`,
            );
          } catch {
            // Logging failures must never escape into the host process.
          }
        }
      });
  }

  /** Wait for queued writes. Intended for shutdown hooks and tests. */
  async drain(): Promise<void> {
    await this.queue;
  }

  private async write(data: string): Promise<void> {
    await mkdir(dirname(this.path), { recursive: true });
    if (this.estimatedBytes === null) {
      try {
        this.estimatedBytes = (await stat(this.path)).size;
      } catch (error: unknown) {
        if (!isMissing(error)) throw error;
        this.estimatedBytes = 0;
      }
    }

    const bytes = Buffer.byteLength(data);
    if (this.estimatedBytes > 0 && this.estimatedBytes + bytes > this.maxBytes) {
      await this.rotate();
    }
    await appendFile(this.path, data, "utf8");
    this.estimatedBytes = (this.estimatedBytes ?? 0) + bytes;
  }

  private async rotate(): Promise<void> {
    if (this.generations <= 0) {
      await removeIfPresent(this.path);
      this.estimatedBytes = 0;
      return;
    }
    await removeIfPresent(`${this.path}.${this.generations}`);
    for (let generation = this.generations - 1; generation >= 1; generation -= 1) {
      await renameIfPresent(`${this.path}.${generation}`, `${this.path}.${generation + 1}`);
    }
    await renameIfPresent(this.path, `${this.path}.1`);
    this.estimatedBytes = 0;
  }
}

function isMissing(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

async function removeIfPresent(path: string): Promise<void> {
  try {
    await rm(path, { force: true });
  } catch (error: unknown) {
    if (!isMissing(error)) throw error;
  }
}

async function renameIfPresent(from: string, to: string): Promise<void> {
  try {
    await rename(from, to);
  } catch (error: unknown) {
    if (!isMissing(error)) throw error;
  }
}
