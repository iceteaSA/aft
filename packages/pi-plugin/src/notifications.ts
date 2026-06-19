import {
  type BinaryBridge,
  markAnnouncementSeen,
  shouldShowAnnouncement,
} from "@cortexkit/aft-bridge";
import { formatConfigParseFailureMessage } from "./config.js";
import { log, sessionLog } from "./logger.js";

const WARNING_MARKER = "🔧 AFT: ⚠️";
const FEATURE_MARKER = "🔧 AFT: ✨";

export interface ConfigureWarning {
  kind:
    | "formatter_not_installed"
    | "checker_not_installed"
    | "lsp_binary_missing"
    | "config_parse_failed"
    | "config_key_dropped";
  language?: string;
  server?: string;
  tool?: string;
  binary?: string;
  hint: string;
}

export interface ConfigureWarningOptions {
  client: unknown;
  sessionId: string;
  bridge: Pick<BinaryBridge, "send">;
  storageDir: string;
  pluginVersion: string;
  projectRoot?: string;
}

type PiNotificationClient = {
  ui?: {
    notify?: (message: string, type?: "info" | "warning" | "error") => void;
  };
};

function sendIgnoredMessage(client: unknown, sessionId: string, text: string): boolean {
  const typedClient = client as PiNotificationClient;
  if (typeof typedClient.ui?.notify !== "function") return false;

  try {
    typedClient.ui.notify(text, "warning");
    return true;
  } catch (err) {
    sessionLog(
      sessionId,
      `[aft-pi] notification send failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return false;
  }
}

/**
 * Reads the persisted `warned_tools` dedup map.
 *
 * Returns `null` when the state could NOT be read (bridge not configured yet /
 * RPC error) — distinct from `{}` which means "read succeeded, nothing recorded
 * yet". The caller must treat `null` as "unknown" and NOT as "never warned":
 * conflating the two re-fired the same `lsp_binary_missing` warning on every
 * session, because a read that raced the not-configured window returned `{}`,
 * the gate read "never warned", and the warning was delivered again.
 *
 * A read that SUCCEEDS but returns a malformed/corrupt value is treated as a
 * recoverable empty `{}` (deliver once, then recordWarning overwrites the bad
 * value) — only a genuine read failure is `null`.
 */
async function readWarnedTools(
  bridge: Pick<BinaryBridge, "send">,
): Promise<Record<string, unknown> | null> {
  let resp: Awaited<ReturnType<Pick<BinaryBridge, "send">["send"]>>;
  try {
    resp = await bridge.send("db_get_state", { key: "warned_tools" });
  } catch {
    return null;
  }
  if (resp.success === false) return null;

  const value = (resp.data as { value?: unknown } | undefined)?.value;
  if (typeof value !== "string") return {};
  try {
    const parsed = JSON.parse(value) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as Record<string, unknown>;
  } catch {
    return {};
  }
}

/**
 * Tri-state dedup check:
 *   - "warned": key recorded — skip.
 *   - "fresh": state read OK, key absent — deliver + record.
 *   - "unknown": state unreadable — do NOT deliver (can't dedup); a later
 *     configured call delivers once.
 */
async function warnedStatus(
  bridge: Pick<BinaryBridge, "send">,
  key: string,
): Promise<"warned" | "fresh" | "unknown"> {
  const warned = await readWarnedTools(bridge);
  if (warned === null) return "unknown";
  return warned[key] === true || typeof warned[key] === "string" ? "warned" : "fresh";
}

async function recordWarning(bridge: Pick<BinaryBridge, "send">, key: string): Promise<void> {
  const warned = await readWarnedTools(bridge);
  if (warned === null) return;
  warned[key] = true;

  try {
    await bridge.send("db_set_state", {
      key: "warned_tools",
      value: JSON.stringify(warned),
    });
  } catch {
    // best-effort
  }
}

function warningKey(warning: ConfigureWarning, projectRoot?: string): string {
  const scope = warning.kind === "lsp_binary_missing" ? "_" : (projectRoot ?? "_");
  return [
    scope,
    warning.kind,
    warning.language ?? warning.server ?? "_",
    warning.tool ?? warning.binary ?? "_",
    warning.hint,
  ]
    .map((part) => encodeURIComponent(part))
    .join(":");
}

function warningTitle(warning: ConfigureWarning): string {
  switch (warning.kind) {
    case "formatter_not_installed":
      return "Formatter is not installed";
    case "checker_not_installed":
      return "Checker is not installed";
    case "lsp_binary_missing":
      return "LSP binary is missing";
    case "config_parse_failed":
      return "Config failed to parse";
    case "config_key_dropped":
      return "Config key ignored";
  }
}

function formatConfigureWarning(warning: ConfigureWarning): string {
  if (warning.kind === "config_key_dropped") return `${WARNING_MARKER} ${warning.hint}`;

  const details: string[] = [];
  if (warning.language) details.push(`language: ${warning.language}`);
  if (warning.server) details.push(`server: ${warning.server}`);
  if (warning.tool) details.push(`tool: ${warning.tool}`);
  if (warning.binary && warning.binary !== warning.tool) {
    details.push(`binary: ${warning.binary}`);
  }

  const suffix = details.length > 0 ? ` (${details.join(", ")})` : "";
  return `${WARNING_MARKER} ${warningTitle(warning)}${suffix}\n${warning.hint}`;
}

export async function deliverConfigureWarnings(
  opts: ConfigureWarningOptions,
  warnings: ConfigureWarning[],
): Promise<void> {
  if (warnings.length === 0) return;

  // `warned_tools` now persists through the bridge DB state API. This loses the
  // old file-lock read-modify-write mutex, so two same-process concurrent
  // recordWarning calls could race and drop one key. Configure warnings are
  // delivered sequentially in normal plugin flow; if this becomes observable,
  // add a bridge-side atomic update command rather than reviving file locks.
  for (const warning of warnings) {
    const key = warningKey(warning, opts.projectRoot);
    // "warned" → already shown once; "unknown" → dedup state unreadable
    // (bridge not configured yet), so do NOT deliver — delivering on unknown
    // is what re-fired the warning every session. Only "fresh" delivers.
    if ((await warnedStatus(opts.bridge, key)) !== "fresh") continue;

    if (!sendIgnoredMessage(opts.client, opts.sessionId, formatConfigureWarning(warning))) continue;

    await recordWarning(opts.bridge, key);
  }
}

export function notifyConfigParseFailures(
  client: unknown,
  sessionId: string,
  errors: readonly { path: string; message: string }[],
): void {
  for (const entry of errors) {
    const text = `${WARNING_MARKER} ${formatConfigParseFailureMessage(entry.path, entry.message)}`;
    if (!sendIgnoredMessage(client, sessionId, text)) {
      sessionLog(sessionId, `[aft-pi] ${text}`);
    }
  }
}

export function sendFeatureAnnouncement(
  version: string,
  features: string[],
  footer: string,
  storageDir: string,
): void {
  // shouldShowAnnouncement silently seeds the marker on first-install /
  // ephemeral-sandbox launches, so Docker/CI/disposable-VM users don't get
  // changelog bullets spammed on every boot (per magic-context#99). Real
  // upgrades from a persisted older version still surface here.
  if (!shouldShowAnnouncement(storageDir, "pi", version)) return;

  // Blank-line separator pins the persistent footer (Discord invite, etc.)
  // below the version-specific bullets so the footer reads as "always here"
  // rather than as one more changelog item.
  const sections: string[] = [
    `${FEATURE_MARKER} v${version}:`,
    ...features.map((feature) => `  • ${feature}`),
  ];
  if (typeof footer === "string" && footer.trim().length > 0) {
    sections.push("", footer);
  }
  log(sections.join("\n"));

  markAnnouncementSeen(storageDir, "pi", version);
}
