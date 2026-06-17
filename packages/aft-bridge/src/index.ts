/**
 * @cortexkit/aft-bridge
 *
 * Shared transport, binary resolution, and ONNX runtime helpers for AFT
 * agent-host plugins. Public surface intentionally narrow — host policies
 * (config loading, permission UX, tool registration, notifications) stay in
 * each host plugin.
 */

// --- logger contract ---
export { setActiveLogger } from "./active-logger.js";
// --- bash output hints (shared by both plugin hosts) ---
export {
  appendPipeStripNote,
  formatForegroundResult,
  formatSeconds,
  isTerminalStatus,
  sleep,
} from "./bash-format.js";
export {
  commandInvokesCodeSearch,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
} from "./bash-hints.js";
export { resolveBashKillTimeout } from "./bash-timeout.js";
export type {
  BashCompletedPayload,
  BashLongRunningPayload,
  BridgeOptions,
  BridgeRequestOptions,
  ConfigureWarning,
  ConfigureWarningsContext,
  StatusSnapshot,
} from "./bridge.js";
// --- transport ---
export { BinaryBridge, compareSemver, tagStderrLine } from "./bridge.js";
// --- aft_callgraph flat formatter (shared by both plugin hosts) ---
export type { CallgraphTheme } from "./callgraph-format.js";
export { formatCallgraphSections, PLAIN_CALLGRAPH_THEME } from "./callgraph-format.js";
export { coerceOptionalInt, coerceStringArray, isEmptyParam } from "./coerce.js";
export { LONG_RUNNING_COMMAND_TIMEOUT_MS, timeoutForCommand } from "./command-timeouts.js";
// --- binary resolution ---
export {
  downloadBinary,
  ensureBinary,
  getBinaryName,
  getCacheDir,
  getCachedBinaryPath,
} from "./downloader.js";
export type { EditSummaryInput } from "./edit-summary.js";
export { formatEditSummary } from "./edit-summary.js";
// --- compact UI formatting ---
export { compressionSavingsPercent, formatTokenCount } from "./format.js";
// --- jsonc helpers ---
export { stripJsoncSymbols } from "./jsonc.js";
export type { Logger, LogMeta } from "./logger.js";
export type { MigrationHarness, MigrationOptions, MigrationStatus } from "./migration.js";
// --- storage migration ---
export {
  ensureStorageMigrated,
  getMigrationStatus,
  resolveCortexKitStorageRoot,
  resolveLegacyStorageRoot,
} from "./migration.js";
// --- npm resolution (PATH-stripped GUI launch fallback) ---
export type { ResolvedNpm } from "./npm-resolver.js";
export {
  isNpmAvailable,
  npmSpawnEnv,
  probeNpmVersion,
  resolveNpm,
} from "./npm-resolver.js";
// --- ONNX runtime ---
export {
  __test__ as __onnxTest__,
  cleanupOnnxRuntime,
  ensureOnnxRuntime,
  getManualInstallHint,
  isOrtAutoDownloadSupported,
} from "./onnx-runtime.js";
export {
  markAnnouncementSeen,
  repairRootScopedStorageFile,
  resolveHarnessStoragePath,
  shouldShowAnnouncement,
} from "./paths.js";
export type { PipeStripResult } from "./pipe-strip.js";
export { maybeStripCompressorPipe } from "./pipe-strip.js";
// --- platform helpers ---
export { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "./platform.js";
export type { PoolOptions } from "./pool.js";
export { BridgePool, HomeProjectRootError, isHomeDirectoryRoot } from "./pool.js";
// --- wire contract ---
export type {
  AftErrorResponse,
  AftPushFrame,
  AftRequestEnvelope,
  AftResponse,
  AftSuccessResponse,
  BashCompletedFrame,
  BgCompletion,
  ConfigureWarningFrame,
  PermissionAskFrame,
  ProgressFrame,
  StatusCompression,
  StatusCompressionAggregate,
  StatusResponse,
} from "./protocol.js";
export { findBinary, findBinarySync, isNativeExecutable, platformKey } from "./resolver.js";
// --- agent status bar (shared by both plugin hosts) ---
export type { StatusBarCounts, StatusBarEmitState } from "./status-bar.js";
export {
  createStatusBarEmitState,
  formatStatusBar,
  parseStatusBarCounts,
  STATUS_BAR_HEARTBEAT_CALLS,
  shouldEmitStatusBar,
  statusBarLine,
} from "./status-bar.js";
// --- shared agent-facing tool formatting ---
export type { ReadFooterOptions } from "./tool-format.js";
export { formatBridgeErrorMessage, formatReadFooter } from "./tool-format.js";
// --- aft_zoom plain-text formatter (shared by both plugin hosts) ---
export type {
  RustZoomBatchEntry,
  ZoomMultiTargetEntry,
  ZoomMultiTargetResult,
  ZoomMultiTargetSymbolResult,
  ZoomResponseLike,
} from "./zoom-format.js";
export {
  formatZoomMultiTargetResult,
  formatZoomText,
  isRustZoomBatchEnvelope,
  unwrapRustZoomBatchEnvelope,
} from "./zoom-format.js";
