import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SRC_DIR = dirname(fileURLToPath(import.meta.url));
export const HARNESS_DIR = resolve(SRC_DIR, "..");
export function resetDir(path: string): void {
  rmSync(path, { recursive: true, force: true });
  mkdirSync(path, { recursive: true });
}

export function copyFixture(source: string, destination: string): void {
  resetDir(destination);
  cpSync(source, destination, { recursive: true, dereference: true, filter: (src) => !src.includes("node_modules") });
  const init = Bun.spawnSync(["git", "init"], { cwd: destination, stdout: "pipe", stderr: "pipe" });
  if (init.exitCode === 0) {
    Bun.spawnSync(["git", "config", "user.email", "bench@example.invalid"], { cwd: destination });
    Bun.spawnSync(["git", "config", "user.name", "Benchmark"], { cwd: destination });
    Bun.spawnSync(["git", "add", "-A"], { cwd: destination });
    Bun.spawnSync(["git", "commit", "-m", "fixture"], { cwd: destination, stdout: "pipe", stderr: "pipe" });
  }
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

export function readOpencodeAuthKey(provider: string): string | null {
  try {
    const home = process.env.HOME;
    if (!home) return null;
    const raw = readFileSync(`${home}/.local/share/opencode/auth.json`, "utf8");
    const parsed = JSON.parse(raw) as Record<string, { type?: string; key?: string }>;
    const entry = parsed[provider];
    if (!entry || entry.type !== "api" || typeof entry.key !== "string") return null;
    return entry.key;
  } catch {
    return null;
  }
}

export function writeJson(path: string, value: unknown): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

export function ensureExists(path: string, label: string): void {
  if (!existsSync(path)) throw new Error(`${label} not found: ${path}`);
}

export function percentile(values: number[], pct: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.max(0, Math.min(sorted.length - 1, Math.ceil((pct / 100) * sorted.length) - 1));
  return round(sorted[index] ?? 0, 3);
}

export function round(value: number, digits = 3): number {
  return Number.isFinite(value) ? Number(value.toFixed(digits)) : 0;
}
