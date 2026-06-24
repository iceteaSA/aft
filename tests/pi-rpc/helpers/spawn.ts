import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { createRpcClient, type RpcClient } from "./rpc-client";

const REPO_ROOT = resolve(import.meta.dir, "../../..");
const require_ = createRequire(import.meta.url);

function compareSemver(a: string, b: string): number {
  const left = a.split(".").map((part) => Number(part));
  const right = b.split(".").map((part) => Number(part));
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const diff = (left[index] ?? 0) - (right[index] ?? 0);
    if (diff !== 0) return diff;
  }
  return 0;
}

function resolvePiPackageJson(): string {
  try {
    return require_.resolve("@earendil-works/pi-coding-agent/package.json");
  } catch {
    const bunModules = join(REPO_ROOT, "node_modules/.bun");
    const prefix = "@earendil-works+pi-coding-agent@";
    const candidates = readdirSync(bunModules, { withFileTypes: true })
      .filter((entry) => entry.isDirectory() && entry.name.startsWith(prefix))
      .map((entry) => {
        const version = entry.name.slice(prefix.length).split("+")[0] ?? "0.0.0";
        return { name: entry.name, version };
      })
      .sort((a, b) => compareSemver(b.version, a.version));
    const best = candidates[0];
    if (best === undefined) {
      throw new Error(`Could not locate @earendil-works/pi-coding-agent under ${bunModules}`);
    }
    return join(bunModules, best.name, "node_modules/@earendil-works/pi-coding-agent/package.json");
  }
}

export function resolvePiCli(): string {
  return join(dirname(resolvePiPackageJson()), "dist/cli.js");
}

export function resolvePiPluginDir(): string {
  return join(REPO_ROOT, "packages/pi-plugin");
}

export interface PiSpawnOptions {
  mockProviderURL: string;
  aftPluginDir: string;
  configDir: string;
  workdir: string;
  extraArgs?: string[];
  /**
   * Force `restrict_to_project_root: true` in the generated AFT config so
   * tests that exercise the `ui.confirm` external-directory prompt actually
   * trigger it. Pi defaults this to false ("no restriction"), in which case
   * the plugin defers to Rust without ever showing the prompt.
   */
  restrictToProjectRoot?: boolean;
}

function childEnv(configDir: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined || key === "NODE_ENV") continue;
    result[key] = value;
  }
  result.HOME = configDir;
  result.PI_CODING_AGENT_DIR = join(configDir, ".pi", "agent");
  result.XDG_CONFIG_HOME = join(configDir, "config");
  result.XDG_DATA_HOME = join(configDir, "data");
  result.XDG_CACHE_HOME = join(configDir, "cache");
  result.OPENAI_API_KEY = "sk-mock";
  result.PI_OFFLINE = "1";
  result.PI_SKIP_VERSION_CHECK = "1";
  // Prepend BOTH target/release and target/debug to PATH so the Pi RPC
  // tests find the aft binary regardless of which build the surrounding
  // CI job produced:
  //   - dedicated `pi-rpc-e2e` job:        cargo build --release  → target/release/aft
  //   - workspace `Test` / `Test (macOS)`: cargo test --workspace → target/debug/aft
  //
  // Locally either may exist; release takes precedence to match the
  // dedicated CI job's behavior. Mirrors the same fallback pattern used by
  // packages/opencode-plugin/src/__tests__/e2e/helpers.ts and
  // packages/pi-plugin/src/__tests__/e2e/helpers.ts (TARGET_DEBUG_BINARY).
  const sep = process.platform === "win32" ? ";" : ":";
  result.PATH = [
    join(REPO_ROOT, "target", "release"),
    join(REPO_ROOT, "target", "debug"),
    result.PATH ?? "",
  ].join(sep);
  // Store-backed callgraph ops (e.g. trace_to_symbol) build the persisted store
  // in the background and return `callgraph_building` until it lands. Tests
  // assert the real JSON result, so block the op until the store is ready
  // instead of racing the background build. Matches the determinism knob the
  // other e2e harnesses use; see AppContext::callgraph_build_wait_window.
  result.AFT_CALLGRAPH_BUILD_WAIT_MS = "15000";
  return result;
}

/**
 * Seed the freshly-built aft binary into the versioned cache the spawned Pi
 * process will read, so the resolver uses the code UNDER TEST — not a published
 * artifact.
 *
 * The Pi RPC harness spawns the real Pi CLI, which loads the AFT Pi plugin and
 * resolves the binary via the aft-bridge resolver. That resolver checks the
 * versioned cache (`XDG_CACHE_HOME/aft/bin/v<ver>/aft`) at step 1, BEFORE the
 * npm platform package at step 2. During the pre-release window the lockfile
 * pins a PUBLISHED `@cortexkit/aft-<platform>` at the SAME version as this HEAD
 * build (e.g. both `0.39.4` until v0.40.0 is cut), so `bun install` materializes
 * a stale published binary in node_modules and the resolver's same-version guard
 * (it only rejects MISMATCHED versions) happily uses it at step 2 — running code
 * that predates HEAD and breaking the very behavior under test. Seeding step 1
 * makes the build under test win, exactly as a local `dev-rebuild` does (which is
 * why this never reproduced locally). Production is unaffected: real installs
 * have no source build, so the npm platform package is the correct source there.
 */
function seedBinaryCache(configDir: string): void {
  const ext = process.platform === "win32" ? ".exe" : "";
  const builtBinary = [
    join(REPO_ROOT, "target", "release", `aft${ext}`),
    join(REPO_ROOT, "target", "debug", `aft${ext}`),
  ].find((candidate) => existsSync(candidate));
  if (builtBinary === undefined) {
    throw new Error(
      `No built aft binary at target/release/aft${ext} or target/debug/aft${ext}. ` +
        "Run: cargo build --release -p agent-file-tools",
    );
  }
  // The cache lookup keys on the version the binary reports (the resolver then
  // verifies the cached binary reports the expected version), so derive the tag
  // straight from the binary rather than guessing the workspace version.
  const reported = execFileSync(builtBinary, ["--version"], { encoding: "utf8" })
    .trim()
    .replace(/^aft\s+/, "");
  const cacheBinDir = join(configDir, "cache", "aft", "bin", `v${reported}`);
  mkdirSync(cacheBinDir, { recursive: true });
  const dest = join(cacheBinDir, `aft${ext}`);
  copyFileSync(builtBinary, dest);
  chmodSync(dest, 0o755);
  // The copy is only linker-signed; macOS Sequoia SIGKILLs such binaries on
  // exec. Ad-hoc re-sign (best-effort) so local macOS runs don't die — the
  // Linux CI job doesn't need it. Mirrors scripts/dev-rebuild.sh.
  if (process.platform === "darwin") {
    try {
      execFileSync("codesign", ["--force", "--sign", "-", dest], { stdio: "ignore" });
    } catch {
      // codesign unavailable / failed — leave the copy as-is.
    }
  }
}

function writeConfigs(opts: PiSpawnOptions): string {
  const agentDir = join(opts.configDir, ".pi", "agent");
  const extensionsDir = join(agentDir, "extensions");
  mkdirSync(extensionsDir, { recursive: true });
  mkdirSync(join(opts.configDir, "config"), { recursive: true });
  mkdirSync(join(opts.configDir, "data"), { recursive: true });
  mkdirSync(join(opts.configDir, "cache"), { recursive: true });
  // Resolve to the build under test, not the lockfile's published platform pkg.
  seedBinaryCache(opts.configDir);

  const distEntry = join(opts.aftPluginDir, "dist", "index.js");
  if (!existsSync(distEntry)) {
    throw new Error(`${distEntry} is missing. Run: cd packages/pi-plugin && bun run build`);
  }

  const installedPluginDir = join(extensionsDir, "aft-pi");
  if (!existsSync(installedPluginDir)) symlinkSync(opts.aftPluginDir, installedPluginDir, "dir");

  const template = readFileSync(join(import.meta.dir, "../fixtures/models.json.tmpl"), "utf8");
  writeFileSync(
    join(agentDir, "models.json"),
    template.replace("${MOCK_URL}", opts.mockProviderURL),
  );
  writeFileSync(
    join(agentDir, "settings.json"),
    JSON.stringify(
      {
        packages: [`file:${installedPluginDir}`],
        defaultProvider: "mock",
        defaultModel: "mock-model",
        enabledModels: ["mock/mock-model"],
        compaction: { enabled: false },
        retry: { enabled: false },
        quietStartup: true,
        enableInstallTelemetry: false,
      },
      null,
      2,
    ),
  );
  const baseConfig = readFileSync(join(import.meta.dir, "../fixtures/aft-pi-config.jsonc"), "utf8");
  // Tests that exercise the `ui.confirm` external-directory prompt opt into
  // strict mode by passing `restrictToProjectRoot: true`. Without this, the
  // plugin defers to Rust (Pi default behavior) and the prompt never fires.
  const aftConfig = opts.restrictToProjectRoot
    ? baseConfig.replace(/\}\s*$/, ',\n  "restrict_to_project_root": true\n}\n')
    : baseConfig;
  writeFileSync(join(agentDir, "aft.jsonc"), aftConfig);
  return agentDir;
}

export function spawnPiRpc(opts: PiSpawnOptions): { child: ChildProcess; client: RpcClient } {
  const agentDir = writeConfigs(opts);
  const child = spawn(
    "node",
    [
      resolvePiCli(),
      "--mode",
      "rpc",
      "--provider",
      "mock",
      "--model",
      "mock/mock-model",
      "--no-session",
      "--session-dir",
      join(opts.configDir, "sessions"),
      ...(opts.extraArgs ?? []),
    ],
    {
      cwd: opts.workdir,
      env: { ...childEnv(opts.configDir), PI_CODING_AGENT_DIR: agentDir },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );

  let stderr = "";
  child.stderr?.on("data", (chunk) => {
    stderr += String(chunk);
  });
  child.once("exit", (code, signal) => {
    if (code !== 0 && signal !== "SIGTERM" && stderr.length > 0) {
      process.stderr.write(`Pi RPC stderr:\n${stderr}\n`);
    }
  });

  return { child, client: createRpcClient(child) };
}
