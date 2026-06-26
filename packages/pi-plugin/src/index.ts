/**
 * AFT (Agent File Tools) extension for Pi coding agent.
 *
 * Config is loaded from two levels (project overrides user):
 * - User:    ~/.config/cortexkit/aft.jsonc (XDG_CONFIG_HOME-aware)
 * - Project: <project>/.cortexkit/aft.jsonc
 *
 * Tools registered:
 *
 * Hoisting (replace Pi's built-in tools):
 *   - read   → AFT's indexed Rust reader
 *   - write  → AFT's atomic writer with backup + auto-format + LSP diagnostics
 *   - edit   → AFT's fuzzy-match edit with backup + diagnostics
 *   - grep   → AFT's trigram-indexed grep (falls back to ripgrep outside project root)
 *
 * AFT-specific:
 *   - aft_outline    Structural outline (symbols, headings) for files/URLs
 *   - aft_zoom       Symbol-level inspection with call-graph annotations
 *   - aft_search     Semantic search (when semantic_search=true)
 *   - aft_callgraph   Call-graph navigation (callers, call_tree, impact, trace_to, trace_to_symbol, trace_data)
 *   - aft_conflicts  One-call merge conflict inspection
 *   - aft_import     Language-aware import add/remove/organize
 *   - aft_safety     Per-file undo, checkpoints, restore
 *   - aft_delete     Delete file with backup
 *   - aft_move       Move/rename file
 *   - ast_grep_search / ast_grep_replace  AST-aware pattern search/rewrite
 *
 * Commands:
 *   - /aft-status    Status dialog (index states, LSP servers, storage dir)
 */

import { createRequire } from "node:module";
import {
  BridgePool,
  ensureBinary,
  ensureOnnxRuntime,
  ensureStorageMigrated,
  findBinary,
  formatDroppedKeyWarnings,
  getManualInstallHint,
  isHomeDirectoryRoot,
  resolveCortexKitStorageRoot,
  setActiveLogger,
} from "@cortexkit/aft-bridge";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import {
  appendToolResultBgCompletions,
  handlePushedBgCompletion,
  handlePushedBgLongRunning,
  handlePushedPatternMatch,
  handleTurnEndBgCompletions,
} from "./bg-notifications.js";
import { registerStatusCommand } from "./commands/aft-status.js";
import {
  type AftConfig,
  buildConfigTierConfigureParams,
  formatConfigParseFailureMessage,
  getConfigLoadErrors,
  loadAftConfig,
  migrateAftConfigLocations,
  resolveBashConfig,
  resolveBridgePoolTransportOptions,
} from "./config.js";
import { bridgeLogger, error, log, warn } from "./logger.js";
import { abortInFlightAutoInstalls, runAutoInstall } from "./lsp-auto-install.js";
import {
  abortInFlightGithubInstalls,
  discoverRelevantGithubServers,
  runGithubAutoInstall,
} from "./lsp-github-install.js";
import { GITHUB_LSP_TABLE } from "./lsp-github-table.js";
import { NPM_LSP_TABLE } from "./lsp-npm-table.js";
import {
  type ConfigureWarning,
  deliverConfigureWarnings,
  sendFeatureAnnouncement,
} from "./notifications.js";
import { statusBarBlockForSession } from "./status-bar-inject.js";

// Register our logger with @cortexkit/aft-bridge before any bridge code runs.
setActiveLogger(bridgeLogger);

import { disposeAllPtyTerminals } from "./shared/pty-cache.js";
import { registerShutdownCleanup } from "./shutdown-hooks.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";
import { resolveSessionId } from "./tools/_shared.js";
import { registerAstTools } from "./tools/ast.js";
import { registerBashTool } from "./tools/bash.js";
import { registerConflictsTool } from "./tools/conflicts.js";
import { registerFsTools } from "./tools/fs.js";
import { registerHoistedTools } from "./tools/hoisted.js";
import { registerImportTools } from "./tools/imports.js";
import { registerInspectTool } from "./tools/inspect.js";
import { registerNavigateTool } from "./tools/navigate.js";
import { registerReadingTools } from "./tools/reading.js";
import { registerRefactorTool } from "./tools/refactor.js";
import { registerSafetyTool } from "./tools/safety.js";
import { registerSemanticTool } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";
import { registerWorkflowHints } from "./workflow-hints.js";

type BashLongRunningPayload = {
  session_id: string;
  task_id: string;
  command: string;
  elapsed_ms: number;
  mode?: "pipes" | "pty" | string;
};

type BashPatternMatchPayload = {
  session_id: string;
  task_id: string;
  watch_id: string;
  match_text: string;
  match_offset: number;
  context: string;
  once: boolean;
  reason?: "pattern_match" | "task_exit";
};

type BridgePendingState = {
  hasPendingRequests(): boolean;
};

type VersionMismatchPool = {
  replaceBinary(path: string): Promise<string>;
};

function createVersionMismatchHandler(
  getPool: () => VersionMismatchPool | undefined,
  ensureCompatibleBinary: (version?: string) => Promise<string | null> = ensureBinary,
) {
  // Coordinate concurrent version mismatches so followers wait for the first
  // download/hot-swap for the target plugin version instead of failing while
  // the compatible binary is still in flight.
  const versionUpgradePromises = new Map<string, Promise<string | null>>();

  return async (binaryVersion: string, minVersion: string): Promise<string | null> => {
    const existing = versionUpgradePromises.get(minVersion);
    if (existing) {
      log(`Version ${binaryVersion} < ${minVersion}; awaiting in-flight compatible binary upgrade`);
      return existing;
    }

    const upgradePromise = (async () => {
      warn(
        `WARNING: aft binary v${binaryVersion} is older than plugin v${minVersion}. ` +
          "Some features may not work. Attempting to download a compatible binary...",
      );
      try {
        const path = await ensureCompatibleBinary(`v${minVersion}`);
        if (!path) {
          warn(`Could not find or download v${minVersion}. Continuing with v${binaryVersion}.`);
          return null;
        }
        const pool = getPool();
        if (!pool) {
          warn(`Found/downloaded compatible binary at ${path}, but bridge pool is not ready.`);
          return null;
        }
        log(`Found/downloaded compatible binary at ${path}. Replacing running bridges...`);
        const replaced = await pool.replaceBinary(path);
        log("Binary replaced successfully. New bridges will use the updated binary.");
        // Returning the new path triggers aft-bridge's coordinated retry of the
        // in-flight request against the replacement binary.
        return replaced;
      } catch (err) {
        error(
          `Auto-download failed: ${(err as Error).message}. Install manually: cargo install agent-file-tools@${minVersion}`,
        );
        return null;
      } finally {
        versionUpgradePromises.delete(minVersion);
      }
    })();
    versionUpgradePromises.set(minVersion, upgradePromise);
    return upgradePromise;
  };
}

/** Plugin version from package.json. */
const PLUGIN_VERSION: string = (() => {
  try {
    const req = createRequire(import.meta.url);
    return (req("../package.json") as { version: string }).version;
  } catch {
    return "0.0.0";
  }
})();

const ANNOUNCEMENT_VERSION = "0.42.0";
const ANNOUNCEMENT_FEATURES: string[] = [
  "Navigation and search tools now return leaner output: aft_zoom on a large class/struct returns a member menu instead of the whole body, aft_callgraph collapses standard-library noise, and aft_search expands only the top match within a budget — substantially less context per call, same answers.",
  "Backups behind aft_safety undo are now configurable (backup.enabled, max_depth, max_file_size), and edits are faster (one appended snapshot instead of rewriting the whole stack).",
  "Editing Rust code with &raw and files whose paths contain [brackets] or {braces} (e.g. Next.js dynamic routes) no longer fails.",
];

/**
 * Persistent footer rendered below the version-specific bullets in every
 * announcement. Stays in place across releases so users always see the Discord
 * invite without us needing to repeat it in `ANNOUNCEMENT_FEATURES` each time.
 *
 * Leave empty (`""`) to suppress.
 */
const ANNOUNCEMENT_FOOTER = "Join us on Discord: https://discord.gg/DSa65w8wuf";

const ALL_ONLY_TOOLS = new Set(["aft_callgraph", "aft_delete", "aft_move", "aft_refactor"]);

const pendingEagerWarnings = new Map<string, ConfigureWarning[]>();

function isConfigureWarning(value: unknown): value is ConfigureWarning {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const warning = value as Record<string, unknown>;
  return (
    (warning.kind === "formatter_not_installed" ||
      warning.kind === "checker_not_installed" ||
      warning.kind === "lsp_binary_missing" ||
      warning.kind === "config_parse_failed" ||
      warning.kind === "config_key_dropped") &&
    typeof warning.hint === "string"
  );
}

function coerceConfigureWarnings(warnings: unknown[]): ConfigureWarning[] {
  return warnings.filter(isConfigureWarning);
}

type DroppedConfigKey = { key: string; tier: string; reason: string };

function isDroppedConfigKey(value: unknown): value is DroppedConfigKey {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const dropped = value as Record<string, unknown>;
  return (
    typeof dropped.key === "string" &&
    typeof dropped.tier === "string" &&
    typeof dropped.reason === "string"
  );
}

function coerceDroppedKeyWarnings(droppedKeys: unknown): ConfigureWarning[] {
  if (!Array.isArray(droppedKeys)) return [];
  return formatDroppedKeyWarnings(droppedKeys.filter(isDroppedConfigKey)).map((hint) => ({
    kind: "config_key_dropped" as const,
    hint,
  }));
}

function drainPendingEagerWarnings(projectRoot: string): ConfigureWarning[] {
  const pending = pendingEagerWarnings.get(projectRoot) ?? [];
  pendingEagerWarnings.delete(projectRoot);
  return pending;
}

function enqueueConfigParseWarnings(
  projectRoot: string,
  errors: ReturnType<typeof getConfigLoadErrors>,
): void {
  if (!projectRoot || errors.length === 0) return;
  const pending = pendingEagerWarnings.get(projectRoot) ?? [];
  for (const entry of errors) {
    const hint = formatConfigParseFailureMessage(entry.path, entry.message);
    if (!pending.some((item) => item.kind === "config_parse_failed" && item.hint === hint)) {
      pending.push({ kind: "config_parse_failed", hint });
    }
  }
  pendingEagerWarnings.set(projectRoot, pending);
}

function shouldPrepareOnnxRuntime(
  config: Pick<AftConfig, "semantic_search" | "semantic">,
): boolean {
  const isFastembedSemanticBackend = (config.semantic?.backend ?? "fastembed") === "fastembed";
  return config.semantic_search === true && isFastembedSemanticBackend;
}

function bridgeDirectoryFromCallback(bridge: unknown, fallback: string): string {
  const cwd = (bridge as { cwd?: unknown } | undefined)?.cwd;
  return typeof cwd === "string" && cwd.length > 0 ? cwd : fallback;
}

// IMPORTANT: NOT exported as a named export — only via the __test__
// namespace at the bottom. Pi's extension loader is different from
// OpenCode's, but OpenCode's plugin loader walks every top-level
// function export and treats them all as plugin entrypoints, which
// crashed our OpenCode-side plugin. Keeping both packages' surface
// shape identical avoids cross-contamination if shared utilities ever
// move between them.
async function handleConfigureWarningsForSession(context: {
  projectRoot: string;
  sessionId?: string | null;
  client?: unknown;
  bridge: Pick<import("@cortexkit/aft-bridge").BinaryBridge, "send">;
  warnings: unknown[];
  configDroppedKeys?: unknown;
  storageDir: string;
  pluginVersion: string;
}): Promise<void> {
  const validWarnings = [
    ...coerceConfigureWarnings(context.warnings),
    ...coerceDroppedKeyWarnings(context.configDroppedKeys),
  ];

  if (!context.sessionId) {
    if (validWarnings.length === 0) return;
    const pending = pendingEagerWarnings.get(context.projectRoot) ?? [];
    pending.push(...validWarnings);
    pendingEagerWarnings.set(context.projectRoot, pending);
    warn(
      `[configure] deferred warnings for ${context.projectRoot} arrived without session_id; buffering until first session-bound call`,
    );
    return;
  }
  if (!context.client) {
    warn(
      `[configure] deferred warnings for session ${context.sessionId} arrived without notification client; skipping notification`,
    );
    return;
  }
  const pendingWarnings = drainPendingEagerWarnings(context.projectRoot);
  const combinedWarnings = [...pendingWarnings, ...validWarnings];
  if (combinedWarnings.length === 0) return;
  await deliverConfigureWarnings(
    {
      client: context.client,
      sessionId: context.sessionId,
      bridge: context.bridge,
      storageDir: context.storageDir,
      pluginVersion: context.pluginVersion,
      projectRoot: context.projectRoot,
    },
    combinedWarnings,
  );
}

/**
 * Tool surface mirrors opencode-plugin: navigate/delete/move/transform/refactor
 * are all-only. recommended exposes hoisted + read/safety/import/ast/lsp/conflicts
 * + experimental search/semantic when enabled.
 *
 * Returns the set of AFT tool names that should be registered given the
 * configured surface + disabled_tools filter. Pi's built-in tools are always
 * present; registering an AFT tool with the same name replaces them.
 */
function resolveToolSurface(config: ReturnType<typeof loadAftConfig>): {
  hoistBash: boolean;
  hoistRead: boolean;
  hoistWrite: boolean;
  hoistEdit: boolean;
  hoistGrep: boolean;
  restrictToProjectRoot: boolean;
  outline: boolean;
  zoom: boolean;
  semantic: boolean;
  inspect: boolean;
  navigate: boolean;
  conflicts: boolean;
  importTool: boolean;
  safety: boolean;
  delete: boolean;
  move: boolean;
  astSearch: boolean;
  astReplace: boolean;
  refactor: boolean;
} {
  const surface = config.tool_surface ?? "recommended";
  const disabled = new Set(config.disabled_tools ?? []);
  const ok = (name: string): boolean => !disabled.has(name);
  const allOnly = (name: string): boolean => ALL_ONLY_TOOLS.has(name) && ok(name);
  // Mirrors the Pi-side default in `configureOverrides` below: false means
  // "no plugin-side restriction; let Rust accept any path." Threaded into
  // ToolSurfaceFlags so hoisted tools can suppress the external_directory
  // prompt that Pi has no host-level allow-list to back.
  const restrictToProjectRoot = config.restrict_to_project_root ?? false;

  if (surface === "minimal") {
    return {
      hoistBash: ok("bash"),
      hoistRead: false,
      hoistWrite: false,
      hoistEdit: false,
      hoistGrep: false,
      restrictToProjectRoot,
      outline: ok("aft_outline"),
      zoom: ok("aft_zoom"),
      semantic: false,
      inspect: false,
      navigate: false,
      conflicts: false,
      importTool: false,
      safety: ok("aft_safety"),
      delete: false,
      move: false,
      astSearch: false,
      astReplace: false,
      refactor: false,
    };
  }

  // recommended + all
  const base = {
    hoistBash: ok("bash"),
    hoistRead: ok("read"),
    hoistWrite: ok("write"),
    hoistEdit: ok("edit"),
    hoistGrep: ok("grep") && config.search_index === true,
    restrictToProjectRoot,
    outline: ok("aft_outline"),
    zoom: ok("aft_zoom"),
    semantic: ok("aft_search") && config.semantic_search === true,
    inspect: ok("aft_inspect") && config.inspect?.enabled !== false,
    navigate: false,
    conflicts: ok("aft_conflicts"),
    importTool: ok("aft_import"),
    safety: ok("aft_safety"),
    delete: false,
    move: false,
    astSearch: ok("ast_grep_search"),
    astReplace: ok("ast_grep_replace"),
    refactor: false,
  };

  if (surface === "all") {
    return {
      ...base,
      navigate: allOnly("aft_callgraph"),
      delete: allOnly("aft_delete"),
      move: allOnly("aft_move"),
      refactor: allOnly("aft_refactor"),
    };
  }

  return base;
}

/**
 * Pi extension default export.
 *
 * Called once per session. Registers tools, commands, and session shutdown hooks.
 */
export default async function (pi: ExtensionAPI): Promise<void> {
  log(`AFT extension loading (plugin v${PLUGIN_VERSION})`);

  // Resolve AFT binary. On first run this downloads the platform binary to
  // ~/.cache/aft/bin/vX.Y.Z/aft. Failures bubble up as an error to Pi's loader.
  let binaryPath: string;
  try {
    binaryPath = await findBinary(PLUGIN_VERSION);
  } catch (err) {
    warn(
      `Failed to resolve AFT binary: ${err instanceof Error ? err.message : String(err)}. ` +
        "Tools will not be registered.",
    );
    return;
  }

  await ensureStorageMigrated({ harness: "pi", binaryPath, logger: bridgeLogger });

  const deliverConfigMigrationWarnings = (messages: readonly string[]) => {
    for (const message of messages) {
      const notify = (pi as { ui?: { notify?: (message: string, type?: "warning") => void } }).ui
        ?.notify;
      if (typeof notify === "function") {
        try {
          notify(message, "warning");
          continue;
        } catch (err) {
          warn(`[config] failed to deliver migration notification: ${err}`);
        }
      }
      try {
        process.stderr.write(`
[AFT] ${message}
`);
      } catch {
        // stderr may be unavailable in embedded hosts; warn() still records it.
      }
    }
  };
  deliverConfigMigrationWarnings(
    migrateAftConfigLocations(process.cwd(), bridgeLogger).flatMap((result) => result.warnings),
  );

  // Load config (user + project).
  const config = loadAftConfig(process.cwd());
  enqueueConfigParseWarnings(process.cwd(), getConfigLoadErrors());
  const storageDir = resolveCortexKitStorageRoot();

  // ONNX runtime for semantic search (optional, best-effort).
  //
  // We deliberately do NOT block plugin load on this. The ONNX runtime archive
  // is 60–80 MB and on a slow connection this can take 30–120 seconds.
  // Awaiting it inline used to make Pi appear to hang during plugin load, and
  // SIGKILL'ing the host mid-download left partial state on disk that the
  // next launch had to recover from.
  //
  // Instead: kick off the download as a background promise and patch
  // `_ort_dylib_dir` into the pool's configure overrides as soon as it
  // settles. Bridges spawned AFTER the download finishes pick it up
  // automatically. `ensureOnnxRuntime` returns null on unsupported platforms.
  let onnxRuntimePromise: Promise<string | null> | null = null;
  if (shouldPrepareOnnxRuntime(config)) {
    onnxRuntimePromise = ensureOnnxRuntime(storageDir).catch((err) => {
      warn(`Failed to prepare ONNX Runtime: ${err instanceof Error ? err.message : String(err)}`);
      return null;
    });
  }

  // Build configure-time params forwarded to every bridge on spawn.
  // Core-domain config flows only through raw tiers; Rust owns merge +
  // trust-boundary stripping. Flat params below are plugin-computed process
  // state and must not be derived from aft.jsonc.
  const configOverrides = buildConfigTierConfigureParams(process.cwd(), {
    storage_dir: storageDir,
  });
  // _ort_dylib_dir is patched in asynchronously below once ensureOnnxRuntime
  // settles. Bridges spawned before that resolution don't get ORT and
  // semantic search returns "still building" until they restart.

  // ─────────────────────────── LSP auto-install ───────────────────────────
  // Mirrors the OpenCode plugin: discover relevant LSPs, surface cached bin
  // dirs to Rust as `lsp_paths_extra`, kick off background installs for
  // anything missing. The 7-day grace defends against newly-published
  // malicious versions. Best-effort — failures never block plugin startup.
  try {
    const lspAutoInstall = config.lsp?.auto_install ?? true;
    const lspGraceDays = config.lsp?.grace_days ?? 7;
    const lspVersions = config.lsp?.versions ?? {};
    const lspDisabled = new Set(config.lsp?.disabled ?? []);
    const projectRoot = process.cwd();
    // When `lsp.auto_install: false`, leave the list empty so the Rust-side
    // `detect_missing_lsp_binaries` loop in configure.rs skips its built-in
    // server walk entirely. Without this gate, users who opted out of
    // auto-install still received `lsp_binary_missing` toasts/log warnings
    // on every configure. Explicit `lsp.servers` entries are unaffected.
    configOverrides.lsp_auto_install_binaries = lspAutoInstall
      ? [...new Set([...NPM_LSP_TABLE, ...GITHUB_LSP_TABLE].map((spec) => spec.binary))]
      : [];

    const npmResult = runAutoInstall(projectRoot, {
      autoInstall: lspAutoInstall,
      graceDays: lspGraceDays,
      versions: lspVersions,
      disabled: lspDisabled,
    });
    const relevantGithub = discoverRelevantGithubServers(projectRoot);
    const ghResult = runGithubAutoInstall(relevantGithub, {
      autoInstall: lspAutoInstall,
      graceDays: lspGraceDays,
      versions: lspVersions,
      disabled: lspDisabled,
    });
    const mergedBinDirs = [...npmResult.cachedBinDirs, ...ghResult.cachedBinDirs];
    if (mergedBinDirs.length > 0) {
      configOverrides.lsp_paths_extra = mergedBinDirs;
    }
    const lspInflightInstalls = [
      ...new Set([...npmResult.installingBinaries, ...ghResult.installingBinaries]),
    ];
    if (lspInflightInstalls.length > 0) {
      configOverrides.lsp_inflight_installs = lspInflightInstalls;
    }
    if (npmResult.installsStarted > 0 || ghResult.installsStarted > 0) {
      log(
        `[lsp] auto-install: ${npmResult.installsStarted} npm + ${ghResult.installsStarted} github install(s) running in background`,
      );
    }

    // ─── Surface install outcomes once installs settle ───
    //
    // Pi loads this extension once at startup, before any session exists, so
    // we can't send an ignored session message the way the OpenCode plugin
    // does. Instead we promote actionable skips from `log()` (verbose) to
    // `warn()` (visible at WARN level) so users running with default logging
    // see them. Routine skips (already-installed, not-relevant, disabled)
    // stay out of the warning summary.
    Promise.all([npmResult.installsComplete, ghResult.installsComplete])
      .then(() => {
        const actionable = [...npmResult.skipped, ...ghResult.skipped].filter((s) => {
          const r = s.reason.toLowerCase();
          if (r === "auto_install: false") return false;
          if (r === "disabled by config") return false;
          if (r === "not relevant to project") return false;
          if (r === "already installed") return false;
          if (r === "another install in progress") return false;
          return true;
        });
        if (actionable.length === 0) return;
        const lines = actionable.map((s) => `  • ${s.id}: ${s.reason}`).join("\n");
        warn(
          `[lsp] skipped or failed to install ${actionable.length} server(s):\n${lines}\n` +
            'Pin a working version with `lsp.versions: { "<package>": "<version>" }` if grace is blocking, ' +
            "or set `lsp.auto_install: false` to suppress.",
        );
      })
      .catch((err) => {
        warn(`[lsp] install-summary aggregation failed: ${err}`);
      });
  } catch (err) {
    warn(`[lsp] auto-install setup failed: ${err instanceof Error ? err.message : String(err)}`);
  }

  let pool: BridgePool;
  const poolOptions: import("@cortexkit/aft-bridge").PoolOptions & {
    onBashLongRunning: (reminder: BashLongRunningPayload, bridge: BridgePendingState) => void;
    onBashPatternMatch: (frame: BashPatternMatchPayload, bridge: BridgePendingState) => void;
  } = {
    ...resolveBridgePoolTransportOptions(config),
    errorPrefix: "[aft-pi]",
    minVersion: PLUGIN_VERSION,
    onVersionMismatch: createVersionMismatchHandler(() => pool),
    onConfigureWarnings: ({ projectRoot, sessionId, client, warnings, configDroppedKeys }) => {
      const bridge = pool.getActiveBridgeForRoot(projectRoot);
      if (!bridge) return;
      const pendingWarnings = sessionId ? drainPendingEagerWarnings(projectRoot) : [];
      // Avoid re-entering bridge.send() from the synchronous configure callback
      // before aft-bridge marks the lazy-spawned bridge configured.
      setTimeout(() => {
        void handleConfigureWarningsForSession({
          projectRoot,
          sessionId,
          client,
          bridge,
          warnings: [...pendingWarnings, ...warnings],
          configDroppedKeys,
          storageDir,
          pluginVersion: PLUGIN_VERSION,
        });
      }, 0);
    },
    onBashCompletion: (completion, bridge) => {
      const directory = bridgeDirectoryFromCallback(bridge, process.cwd());
      void handlePushedBgCompletion(
        {
          ctx,
          directory,
          sessionID: completion.session_id,
          runtime: pi,
        },
        completion,
      );
    },
    onBashLongRunning: (reminder, bridge) => {
      const directory = bridgeDirectoryFromCallback(bridge, process.cwd());
      void handlePushedBgLongRunning(
        {
          ctx,
          directory,
          sessionID: reminder.session_id,
          runtime: pi,
        },
        reminder,
      );
    },
    onBashPatternMatch: (frame, bridge) => {
      const directory = bridgeDirectoryFromCallback(bridge, process.cwd());
      void handlePushedPatternMatch(
        {
          ctx,
          directory,
          sessionID: frame.session_id,
          runtime: pi,
        },
        frame,
      );
    },
  };
  pool = new BridgePool(binaryPath, poolOptions, configOverrides);
  pool.setConfigureOverride("harness", "pi");
  // Tell Rust whether `aft_search` is registered for this surface so the
  // grep-rewrite footer steers there (vs the grep tool). Set before the eager
  // warmup spawn below so even the first bridge configures with the flag.
  // `resolveToolSurface` is pure; `.semantic` is the same predicate the tool
  // registration uses (ok("aft_search") && semantic_search === true).
  pool.setConfigureOverride("aft_search_registered", resolveToolSurface(config).semantic);
  const ctx: PluginContext = { pool, config, storageDir };

  // Settle the ONNX runtime download promise (started above) and patch the
  // resolved path into the pool's configure overrides. Bridges spawned AFTER
  // this resolves will pass `_ort_dylib_dir` through configure and pick up
  // the runtime; bridges already running at resolution time keep going
  // without ORT (we don't restart them — that would discard warm
  // trigram/semantic/LSP state). Result: semantic search becomes available
  // for new sessions automatically once the download completes.
  if (onnxRuntimePromise) {
    onnxRuntimePromise.then(
      (ortDylibDir) => {
        if (ortDylibDir) {
          pool.setConfigureOverride("_ort_dylib_dir", ortDylibDir);
          log(`ONNX Runtime ready at ${ortDylibDir}; new bridges will load semantic backend.`);
        } else {
          warn(
            `ONNX Runtime unavailable. Semantic search will be disabled. Install manually: ${getManualInstallHint()}`,
          );
        }
      },
      (err) => {
        warn(`ONNX Runtime resolution rejected unexpectedly: ${err}`);
      },
    );
  }

  // Eager async configure: warm the bridge for `process.cwd()` so the first
  // tool call doesn't pay the spawn + configure latency. Errors are swallowed —
  // the next real tool call will surface a proper error.
  //
  // AUDIT NOTE (intentional — do not flag as a bug): unlike the OpenCode
  // plugin, Pi keeps eager warmup ON PURPOSE. Pi runs one bridge per process
  // (one session per process) and has no OpenCode-Desktop-style sidebar that
  // multiplies plugin instances across many projects/worktrees. The reason
  // OpenCode went lazy (avoid spawning N bridges for N sidebar projects at
  // startup) does not apply here, so eager warmup is the correct trade for Pi:
  // it removes first-tool-call latency without the bridge-storm downside.
  // The $HOME guard below is the only case we skip. See the home-dir note.
  void (async () => {
    try {
      // Note #65: skip eager configure when Pi was launched from the user's
      // home directory. Configuring on `$HOME` walks the entire user home
      // tree (100k–10M files), times out the 30s configure budget, gets
      // killed, then silently retries on every reload. The first real tool
      // call from a session will still warm the correct project bridge.
      const cwd = process.cwd();
      if (isHomeDirectoryRoot(cwd)) {
        log(
          `Eager configure skipped: cwd=${cwd} is the user home directory. ` +
            `The first real tool call will warm the correct project bridge.`,
        );
        return;
      }
      // Await ONNX runtime resolution BEFORE spawning the bridge — otherwise
      // the bridge starts without _ort_dylib_dir on its configure overrides,
      // and Rust falls back to a system-path dlopen("libonnxruntime.dylib")
      // that almost always fails on macOS / Windows (the user has the runtime
      // managed in `<storage_dir>/onnxruntime/`, not on the system loader
      // path). The race symptom in the wild: log shows
      //   Spawning binary: ...
      //   ONNX Runtime ready at ...   <- 4ms later
      //   failed to build semantic index: ONNX Runtime not found
      // because the bridge spawn at t=0 has no _ort_dylib_dir yet, and once
      // it's set on the pool only NEW bridges pick it up. Mirror the
      // OpenCode plugin: cap at 60s so a slow/broken download doesn't block
      // the warmup permanently; the bridge still spawns without ORT after
      // the cap and semantic just fails honestly.
      if (onnxRuntimePromise) {
        await Promise.race([
          onnxRuntimePromise,
          new Promise<null>((resolve) => setTimeout(() => resolve(null), 60_000)),
        ]);
      }
      const bridge = pool.getBridge(cwd);
      // No session_id: runs before any user session exists; configure
      // threads spawned by this warmup will log with no [ses_xxx] prefix.
      const response = await bridge.send("status", {});
      // Seed the plugin-side cache so the /aft-status overlay's first poll
      // after spawn finds a warm snapshot instead of racing into bridge.send
      // and hitting the client timeout while the bridge dispatch loop is
      // still finishing configure. Push frames will overwrite this with
      // fresh data on every state transition (1s debounce).
      if (response.success !== false) {
        bridge.cacheStatusSnapshot(response as Parameters<typeof bridge.cacheStatusSnapshot>[0]);
      }
    } catch (err) {
      log(`eager configure failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  })();

  if (ANNOUNCEMENT_VERSION && ANNOUNCEMENT_FEATURES.length > 0) {
    sendFeatureAnnouncement(
      ANNOUNCEMENT_VERSION,
      ANNOUNCEMENT_FEATURES,
      ANNOUNCEMENT_FOOTER,
      storageDir,
    );
  }

  const surface = resolveToolSurface(config);

  // Hoisted tool overrides (replace Pi's built-in bash/read/write/edit/grep with AFT versions).
  // Bash hoisting is gated by the single resolved bash config — see
  // `resolveBashConfig` in config.ts for the precedence rules. When
  // `surface.hoistBash` is false (e.g. `tool_surface: "minimal"`), AFT bash
  // never registers regardless of resolved config. registerBashTool registers
  // `bash` whenever enabled and gates the background control tools on
  // `bash.background`.
  if (surface.hoistBash && resolveBashConfig(config).enabled) {
    registerBashTool(pi, ctx, surface.semantic);
  }
  registerHoistedTools(pi, ctx, surface);

  // AFT-specific tools
  if (surface.outline || surface.zoom) {
    registerReadingTools(pi, ctx, surface);
  }
  if (surface.semantic) {
    registerSemanticTool(pi, ctx);
  }
  if (surface.inspect) {
    registerInspectTool(pi, ctx);
  }
  if (surface.navigate) {
    registerNavigateTool(pi, ctx);
  }
  if (surface.conflicts) {
    registerConflictsTool(pi, ctx);
  }
  if (surface.importTool) {
    registerImportTools(pi, ctx);
  }
  if (surface.safety && config.backup?.enabled !== false) {
    registerSafetyTool(pi, ctx);
  }
  if (surface.astSearch || surface.astReplace) {
    registerAstTools(pi, ctx, surface);
  }
  if (surface.delete || surface.move) {
    registerFsTools(pi, ctx, surface);
  }
  if (surface.refactor) {
    registerRefactorTool(pi, ctx);
  }

  // Workflow hints: short system-prompt block teaching token-efficient
  // AFT workflows. Hooked into Pi's `before_agent_start` event with
  // systemPrompt extension. Always-on; conditional on the registered
  // tool surface so absent tools aren't advertised.
  registerWorkflowHints(pi, config, surface);

  // Slash command: /aft-status
  registerStatusCommand(pi, ctx);

  (
    pi.on as (
      event: "tool_result",
      handler: (
        event: {
          content: Array<
            { type: "text"; text: string } | { type: "image"; data: string; mimeType: string }
          >;
          details: unknown;
          isError: boolean;
        },
        ctx: Parameters<typeof resolveSessionId>[0] & { cwd: string },
      ) => unknown,
    ) => void
  )("tool_result", async (event, extCtx) => {
    const sessionID = resolveSessionId(extCtx);
    const bgContent = await appendToolResultBgCompletions(
      { ctx, directory: extCtx.cwd, sessionID },
      event.content,
    );
    // Start from the bg-completion-augmented content if present, else original.
    let content = bgContent ?? event.content;
    // Agent status bar — IDE-style health glance, appended on emit-on-change.
    // Read from the active bridge (no spawn); the Rust side keeps counts current.
    // Force it on aft_inspect (the bar IS that call's summary line). The Pi
    // tool_result event doesn't carry the tool name, so detect inspect by its
    // distinctive top-level `scanner_state` key on the response (passed as
    // `details`) — only inspect emits it.
    const isInspect =
      typeof event.details === "object" &&
      event.details !== null &&
      "scanner_state" in (event.details as Record<string, unknown>);
    const activeBridge = pool.getActiveBridgeForRoot(extCtx.cwd);
    const bar = statusBarBlockForSession(sessionID, activeBridge?.getStatusBar(), isInspect);
    if (bar) content = [...content, { type: "text", text: bar }];
    // Nothing to add → leave the tool result untouched.
    if (content === event.content) return undefined;
    return { content, details: event.details, isError: event.isError };
  });

  (
    pi.on as (
      event: "turn_end",
      handler: (
        event: unknown,
        ctx: Parameters<typeof resolveSessionId>[0] & { cwd: string },
      ) => unknown,
    ) => void
  )("turn_end", async (_event, extCtx) => {
    const sessionID = resolveSessionId(extCtx);
    await handleTurnEndBgCompletions({
      ctx,
      directory: extCtx.cwd,
      sessionID,
      runtime: pi,
    });
    const bridge = pool.getActiveBridgeForRoot(extCtx.cwd);
    if (bridge && sessionID) {
      void handleConfigureWarningsForSession({
        projectRoot: extCtx.cwd,
        sessionId: sessionID,
        client: pi,
        bridge,
        warnings: [],
        storageDir,
        pluginVersion: PLUGIN_VERSION,
      });
    }
  });

  // User-message abort: when the user sends a message while the agent is
  // blocked in a sync bash_watch wait, signal the wait to abort and convert
  // to an async watch so the user isn't locked out.
  (
    pi.on as (
      event: "input",
      handler: (
        event: { type: string; text: string; source: string },
        ctx: Parameters<typeof resolveSessionId>[0] & { cwd: string },
      ) => unknown,
    ) => void
  )("input", (_event, extCtx) => {
    signalSyncWatchAbort(resolveSessionId(extCtx));
  });

  // Also register process-level signal handlers so children get an orderly
  // shutdown when Pi's host Node process is killed directly (terminal close,
  // Ctrl+C, OS shutdown) rather than through the session_shutdown lifecycle.
  const unregisterShutdownCleanup = registerShutdownCleanup(async () => {
    try {
      await Promise.allSettled([abortInFlightAutoInstalls(), abortInFlightGithubInstalls()]);
      await disposeAllPtyTerminals();
      await pool.shutdown();
    } catch (err) {
      warn(`Error during process shutdown: ${err instanceof Error ? err.message : String(err)}`);
    }
  });

  // Clean up bridges on session shutdown.
  pi.on("session_shutdown", async () => {
    try {
      await Promise.allSettled([abortInFlightAutoInstalls(), abortInFlightGithubInstalls()]);
      await disposeAllPtyTerminals();
      await pool.shutdown();
      log("Bridge pool shut down");
    } catch (err) {
      warn(`Error during bridge shutdown: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      unregisterShutdownCleanup();
    }
  });

  log(`AFT extension ready (surface=${config.tool_surface ?? "recommended"})`);
}

export const __test__ = {
  enqueueConfigParseWarnings,
  bridgeDirectoryFromCallback,
  resolveToolSurface,
  handleConfigureWarningsForSession,
  shouldPrepareOnnxRuntime,
  createVersionMismatchHandler,
};
