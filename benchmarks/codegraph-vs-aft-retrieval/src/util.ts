import { existsSync, mkdirSync } from "node:fs";
import { dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SRC_DIR = dirname(fileURLToPath(import.meta.url));
export const HARNESS_DIR = resolve(SRC_DIR, "..");
export const REPO_ROOT = resolve(HARNESS_DIR, "../..");

export function round(value: number, digits = 6): number {
  return Number.isFinite(value) ? Number(value.toFixed(digits)) : 0;
}

export function percentile(values: number[], pct: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.max(0, Math.min(sorted.length - 1, Math.ceil((pct / 100) * sorted.length) - 1));
  return round(sorted[index] ?? 0, 3);
}

export function mean(values: number[]): number {
  if (values.length === 0) return 0;
  return round(values.reduce((sum, value) => sum + value, 0) / values.length);
}

export function normalizePath(raw: string | undefined, root: string): string | undefined {
  if (!raw) return undefined;
  const cleaned = raw.replace(/\\/g, "/");
  const absolute = cleaned.startsWith("/") ? cleaned : resolve(root, cleaned);
  return relative(root, absolute).replace(/\\/g, "/") || ".";
}

export function gitRev(path: string, short = false): string {
  const proc = Bun.spawnSync(["git", "-C", path, "rev-parse", short ? "--short" : "HEAD"], {
    stdout: "pipe",
    stderr: "pipe",
  });
  if (proc.exitCode !== 0) return "unknown";
  return new TextDecoder().decode(proc.stdout).trim() || "unknown";
}

export async function runCommand(
  command: string[],
  cwd: string,
  timeoutMs: number,
  env: Record<string, string | undefined> = {},
): Promise<{ stdout: string; stderr: string; exitCode: number; timedOut: boolean }> {
  const proc = Bun.spawn(command, {
    cwd,
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, ...env },
  });
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    proc.kill();
  }, timeoutMs);
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  clearTimeout(timer);
  return { stdout, stderr, exitCode, timedOut };
}

export async function ensureGitTarget(
  name: string,
  url: string,
  commit: string,
  cloneRoot = resolve(HARNESS_DIR, ".bench", "repos"),
): Promise<string> {
  mkdirSync(cloneRoot, { recursive: true });
  const repoPath = resolve(cloneRoot, name);
  if (!existsSync(resolve(repoPath, ".git"))) {
    const clone = await runCommand(["git", "clone", url, repoPath], HARNESS_DIR, 20 * 60_000);
    if (clone.exitCode !== 0) throw new Error(`git clone failed: ${clone.stderr || clone.stdout}`);
  }
  const fetch = await runCommand(["git", "fetch", "--all", "--tags", "--prune"], repoPath, 10 * 60_000);
  if (fetch.exitCode !== 0) throw new Error(`git fetch failed: ${fetch.stderr || fetch.stdout}`);
  const reset = await runCommand(["git", "reset", "--hard", commit], repoPath, 2 * 60_000);
  if (reset.exitCode !== 0) throw new Error(`git reset failed: ${reset.stderr || reset.stdout}`);
  await runCommand(["git", "clean", "-fdx"], repoPath, 2 * 60_000);
  return repoPath;
}
