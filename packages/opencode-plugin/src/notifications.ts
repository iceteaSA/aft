/**
 * User-visible notifications for AFT plugin.
 *
 * Two delivery paths based on the OpenCode client:
 *   - Desktop: Sends ignored messages to the active session (appears in chat, hidden from LLM)
 *   - TUI: Uses client.tui.showToast() for transient toast notifications
 *
 * Use cases:
 *   - Feature announcements (new version, new experimental features)
 *   - Warnings (ONNX Runtime not found, stale binary)
 *
 * Messages are identified by markers and cleaned up on subsequent startups
 * when no longer relevant (Desktop only — TUI toasts are inherently transient).
 */

import { existsSync, readFileSync } from "node:fs";
import { homedir, platform } from "node:os";
import { join } from "node:path";
import {
  type AftProjectTransport,
  markAnnouncementSeen,
  shouldShowAnnouncement,
} from "@cortexkit/aft-bridge";
import type { ConfigureWarningsDelivery } from "./config.js";
import { sessionLog, warn } from "./logger.js";
import { resolvePromptContext } from "./shared/last-assistant-model.js";

// --- TUI toast helper ---

type TuiClient = {
  tui?: {
    showToast?: (input: {
      body: {
        title: string;
        message: string;
        variant?: "info" | "warning" | "error" | "success";
        duration?: number;
      };
    }) => Promise<unknown>;
  };
};

function isTuiMode(): boolean {
  return process.env.OPENCODE_CLIENT === "cli";
}

async function showTuiToast(
  client: unknown,
  title: string,
  message: string,
  variant: "info" | "warning" | "error" | "success" = "info",
  duration = 8000,
): Promise<boolean> {
  const c = client as TuiClient;
  if (typeof c.tui?.showToast !== "function") return false;
  try {
    await c.tui.showToast({ body: { title, message, variant, duration } });
    return true;
  } catch {
    return false;
  }
}

// --- Markers for message identification ---

/** Prefix for all AFT notification messages */
const AFT_MARKER = "🔧 AFT:";

/** Marker for feature announcements */
const FEATURE_MARKER = `${AFT_MARKER} New in`;

/** Marker for warnings (ONNX missing, etc.) */
const WARNING_MARKER = `${AFT_MARKER} ⚠️`;

// --- Desktop state file resolution ---

function getDesktopStatePath(): string | null {
  const os = platform();
  const home = homedir();

  if (os === "darwin") {
    return join(
      home,
      "Library",
      "Application Support",
      "ai.opencode.desktop",
      "opencode.global.dat",
    );
  }
  if (os === "linux") {
    const xdgConfig = process.env.XDG_CONFIG_HOME || join(home, ".config");
    return join(xdgConfig, "ai.opencode.desktop", "opencode.global.dat");
  }
  if (os === "win32") {
    const appData = process.env.APPDATA || join(home, "AppData", "Roaming");
    return join(appData, "ai.opencode.desktop", "opencode.global.dat");
  }

  return null;
}

interface DesktopState {
  serverUrl: string | null;
}

function readDesktopState(): DesktopState {
  const statePath = getDesktopStatePath();
  if (!statePath || !existsSync(statePath)) {
    return { serverUrl: null };
  }

  try {
    const raw = readFileSync(statePath, "utf-8");
    const state = JSON.parse(raw) as Record<string, unknown>;

    // Extract sidecar URL from server state. We intentionally do NOT read
    // layout.page.lastProjectSession here: that value is global per directory,
    // not per Desktop window, and can route startup notifications to the wrong
    // window when two windows have the same repo open. Desktop notifications
    // require an explicit sessionId; session-less startup notices are queued
    // until a caller has concrete session/window context.
    let serverUrl: string | null = null;
    const serverStr = state.server;
    if (typeof serverStr === "string") {
      try {
        const serverState = JSON.parse(serverStr) as Record<string, unknown>;
        if (typeof serverState.currentSidecarUrl === "string") {
          serverUrl = serverState.currentSidecarUrl;
        }
      } catch {
        // ignore
      }
    }

    return { serverUrl };
  } catch {
    return { serverUrl: null };
  }
}

// --- Desktop notification queue ---

type PendingDesktopNotification = {
  key: string;
  text: string;
  shouldSkip?: () => boolean;
  onDelivered?: () => void;
};

const MAX_PENDING_DESKTOP_NOTIFICATIONS = 20;
const pendingDesktopNotifications = new Map<string, PendingDesktopNotification[]>();

function getExplicitSessionId(opts: Pick<NotificationOptions, "sessionId">): string | null {
  const sessionId = opts.sessionId?.trim();
  return sessionId ? sessionId : null;
}

function enqueuePendingDesktopNotification(
  directory: string,
  notification: PendingDesktopNotification,
): void {
  if (!directory) return;
  const pending = pendingDesktopNotifications.get(directory) ?? [];
  if (pending.some((item) => item.key === notification.key)) return;

  pending.push(notification);
  if (pending.length > MAX_PENDING_DESKTOP_NOTIFICATIONS) {
    pending.splice(0, pending.length - MAX_PENDING_DESKTOP_NOTIFICATIONS);
  }
  pendingDesktopNotifications.set(directory, pending);
}

async function flushPendingDesktopNotifications(
  opts: Pick<NotificationOptions, "client" | "directory">,
  sessionId: string,
): Promise<void> {
  const pending = pendingDesktopNotifications.get(opts.directory);
  if (!pending?.length) return;

  pendingDesktopNotifications.delete(opts.directory);
  for (const notification of pending) {
    if (notification.shouldSkip?.()) continue;
    const delivered = await sendIgnoredMessage(opts.client, sessionId, notification.text);
    if (delivered) {
      notification.onDelivered?.();
    } else {
      enqueuePendingDesktopNotification(opts.directory, notification);
    }
  }
}

export function __resetNotificationStateForTests(): void {
  pendingDesktopNotifications.clear();
}

// --- SDK message helpers ---

type SdkMessage = {
  info?: { id?: string; role?: string };
  parts?: Array<{ type?: string; text?: string; ignored?: boolean }>;
};

function getServerAuth(): string | undefined {
  const password = process.env.OPENCODE_SERVER_PASSWORD;
  if (!password) return undefined;
  const username = process.env.OPENCODE_SERVER_USERNAME ?? "opencode";
  return `Basic ${Buffer.from(`${username}:${password}`, "utf8").toString("base64")}`;
}

// The warning cleanup path scans from the END of the array and breaks on the
// first non-AFT user message. It only needs a handful of recent messages, so
// 50 is plenty — typical AFT warning chains are 1-5 consecutive messages at
// the tail.
//
// Bounding is required: without `query.limit`, OpenCode's legacy
// `/session/{id}/message` endpoint hydrates the ENTIRE session. Sessions
// with 30k+ messages and 100k+ parts blow the host's memory.
//
// Future v2 migration: once `@opencode-ai/sdk` exposes
// `client.v2.session.messages` with projected shapes, prefer that with
// `{ limit: 50, order: "desc" }` — but note v2's projected message shape
// strips `parts[]`, which the cleanup logic below relies on for marker
// detection. So v2 may not be drop-in for this caller; we would need v2 to
// expose part content or accept legacy as the only path here.
export const SESSION_MESSAGES_LIMIT = 50;

/**
 * @internal — exported only so tests can pin the bounded-call contract.
 * Production callers go through `cleanupWarnings`, which gates on a real
 * `readDesktopState()` before reaching this helper.
 */
export async function getSessionMessages(
  client: unknown,
  sessionId: string,
): Promise<SdkMessage[]> {
  try {
    const c = client as {
      session?: {
        messages?: (input: {
          path: { id: string };
          query?: { limit?: number };
        }) => Promise<{ data?: SdkMessage[] }>;
      };
    };
    if (typeof c.session?.messages === "function") {
      const result = await c.session.messages({
        path: { id: sessionId },
        query: { limit: SESSION_MESSAGES_LIMIT },
      });
      return result?.data ?? [];
    }
  } catch {
    // silent
  }
  return [];
}

async function sendIgnoredMessage(
  client: unknown,
  sessionId: string,
  text: string,
  options?: { includeAgent?: boolean },
): Promise<boolean> {
  try {
    const c = client as {
      session?: {
        prompt?: (input: unknown) => unknown;
        promptAsync?: (input: unknown) => unknown;
      };
    };

    // `noReply: true` means OpenCode appends this as a synthetic user
    // message and does NOT trigger an assistant turn — no LLM call happens
    // now. But OpenCode's `createUserMessage` still RECORDS prompt context
    // on the appended message, and that recorded context becomes the
    // session's active model/agent for the NEXT real turn.
    //
    // Pin agent AND model/variant from the previous assistant turn for
    // announcements/warnings so the next real turn keeps the previous
    // `{ includeAgent: false }` to skip all context pinning and avoid
    // ModelSwitched / AgentSwitched on the first tool turn.
    const body: Record<string, unknown> = {
      noReply: true,
      parts: [{ type: "text", text, ignored: true }],
    };
    if (options?.includeAgent !== false) {
      const promptContext = await resolvePromptContext(
        c as Parameters<typeof resolvePromptContext>[0],
        sessionId,
      );
      if (promptContext?.agent) body.agent = promptContext.agent;
      if (promptContext?.model) {
        body.model = {
          providerID: promptContext.model.providerID,
          modelID: promptContext.model.modelID,
        };
      }
      if (promptContext?.variant) body.variant = promptContext.variant;
    }

    const promptInput = {
      path: { id: sessionId },
      body,
    };

    if (typeof c.session?.prompt === "function") {
      await Promise.resolve(c.session.prompt(promptInput));
      return true;
    }
    if (typeof c.session?.promptAsync === "function") {
      await c.session.promptAsync(promptInput);
      return true;
    }
  } catch (err) {
    sessionLog(
      sessionId,
      `[aft-plugin] notification send failed: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  return false;
}

async function showToastViaHttp(
  serverUrl: string,
  title: string,
  message: string,
  variant: "info" | "warning" | "error" | "success",
  duration: number,
): Promise<boolean> {
  const auth = getServerAuth();
  const url = `${serverUrl.replace(/\/$/, "")}/tui/show-toast`;
  try {
    const response = await fetch(url, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        ...(auth ? { Authorization: auth } : {}),
      },
      body: JSON.stringify({ title, message, variant, duration }),
      signal: AbortSignal.timeout(10_000),
    });
    return response.ok;
  } catch {
    return false;
  }
}

async function deleteMessage(
  serverUrl: string,
  sessionId: string,
  messageId: string,
): Promise<boolean> {
  const auth = getServerAuth();
  const url = `${serverUrl}/session/${encodeURIComponent(sessionId)}/message/${encodeURIComponent(messageId)}`;

  try {
    const response = await fetch(url, {
      method: "DELETE",
      headers: auth ? { Authorization: auth } : {},
      signal: AbortSignal.timeout(10_000),
    });
    return response.ok;
  } catch {
    return false;
  }
}

// --- Public API ---

export interface NotificationOptions {
  /** The OpenCode SDK client */
  client: unknown;
  /** Project directory used as the queue key for delayed Desktop notices. */
  directory: string;
  /** Concrete OpenCode session/window to receive Desktop notifications. */
  sessionId?: string;
  /** Server URL for message deletion (optional — from ctx.serverUrl) */
  serverUrl?: string;
}

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
  bridge: Pick<AftProjectTransport, "send">;
  storageDir: string;
  pluginVersion: string;
  projectRoot?: string;
  serverUrl?: string;
  delivery?: ConfigureWarningsDelivery;
}

/**
 * Send a persistent warning notification.
 * Desktop: ignored message when sessionId is explicit; otherwise queued.
 * TUI: toast with warning variant (inherently transient).
 */
export async function sendWarning(opts: NotificationOptions, message: string): Promise<void> {
  // Try TUI toast first, fall back to Desktop ignored message
  const toastSent = await showTuiToast(opts.client, "AFT Warning", message, "warning", 10000);
  if (toastSent) return;

  const text = `${WARNING_MARKER} ${message}`;
  const sessionId = getExplicitSessionId(opts);
  if (!sessionId) {
    enqueuePendingDesktopNotification(opts.directory, { key: `warning:${message}`, text });
    return;
  }

  await flushPendingDesktopNotifications(opts, sessionId);
  sessionLog(sessionId, `[aft-plugin] sending warning to session ${sessionId}`);
  await sendIgnoredMessage(opts.client, sessionId, text);
}

/**
 * Send a feature announcement for a new version.
 * Tracked via a version file in storageDir — only fires once per version across all sessions.
 * Desktop: ignored message when sessionId is explicit; otherwise queued.
 * TUI: toast with info variant.
 */
export async function sendFeatureAnnouncement(
  opts: NotificationOptions,
  version: string,
  features: string[],
  footer: string,
  storageDir?: string,
): Promise<void> {
  // Check if we already announced this version (persisted across sessions).
  if (hasAnnouncedVersion(storageDir, version)) return;

  // Blank-line separator pins the persistent footer (Discord invite, etc.)
  // below the version-specific bullets so the footer reads as "always here"
  // rather than as one more changelog item.
  const hasFooter = typeof footer === "string" && footer.trim().length > 0;
  const featureText = hasFooter
    ? [features.map((f) => `• ${f}`).join("\n"), "", footer].join("\n")
    : features.map((f) => `• ${f}`).join("\n");

  // Try TUI toast first (works when client exposes tui.showToast),
  // fall back to Desktop ignored message
  const toastSent = await showTuiToast(opts.client, `AFT v${version}`, featureText, "info", 12000);
  if (toastSent) {
    persistAnnouncedVersion(storageDir, version);
    return;
  }

  const sections: string[] = [`${FEATURE_MARKER} v${version}:`, ...features.map((f) => `  • ${f}`)];
  if (hasFooter) sections.push("", footer);
  const text = sections.join("\n");
  const pending = {
    key: `feature:${version}`,
    text,
    shouldSkip: () => hasAnnouncedVersion(storageDir, version),
    onDelivered: () => persistAnnouncedVersion(storageDir, version),
  };

  const sessionId = getExplicitSessionId(opts);
  if (!sessionId) {
    enqueuePendingDesktopNotification(opts.directory, pending);
    return;
  }

  await flushPendingDesktopNotifications(opts, sessionId);
  if (hasAnnouncedVersion(storageDir, version)) return;

  sessionLog(sessionId, `[aft-plugin] sending feature announcement for v${version}`);
  if (await sendIgnoredMessage(opts.client, sessionId, text)) {
    persistAnnouncedVersion(storageDir, version);
  }
}

/**
 * Returns true when the announcement should be suppressed for any reason:
 *   - storageDir isn't configured (can't persist seen state),
 *   - the marker file already records this version, or
 *   - this is a fresh install / ephemeral sandbox (no marker file yet),
 *     in which case shouldShowAnnouncement silently seeds the marker so the
 *     next launch stays quiet (per magic-context#99).
 *
 * Note the name is retained from the pre-shared-helper version of this
 * module to minimize call-site churn; semantically it's now "should suppress
 * announcement" (fresh-install case included).
 */
function hasAnnouncedVersion(storageDir: string | undefined, version: string): boolean {
  if (!storageDir) return true;
  return !shouldShowAnnouncement(storageDir, "opencode", version);
}

function persistAnnouncedVersion(storageDir: string | undefined, version: string): void {
  if (!storageDir) return;
  markAnnouncementSeen(storageDir, "opencode", version);
}

/**
 * Reads the persisted `warned_tools` dedup map.
 *
 * Returns `null` when the state could NOT be read (bridge not configured yet,
 * RPC error, or a throw) — distinct from `{}` which means "read succeeded, no
 * warnings recorded yet". The caller MUST treat `null` as "unknown" and NOT as
 * "never warned": conflating the two is what caused the same `lsp_binary_missing`
 * warning to re-fire on every session. The dedup row persists fine (record runs
 * once the bridge is configured), but a read that raced the not-configured
 * window returned `{}`, the gate read "never warned", and the warning was
 * re-delivered every time.
 */
async function readWarnedTools(
  bridge: Pick<AftProjectTransport, "send">,
): Promise<Record<string, unknown> | null> {
  let resp: Awaited<ReturnType<Pick<AftProjectTransport, "send">["send"]>>;
  try {
    resp = await bridge.send("db_get_state", { key: "warned_tools" });
  } catch {
    // The RPC itself failed (bridge not ready / transport error). State is
    // UNKNOWN — caller must not treat this as "never warned".
    return null;
  }
  // success:false means the bridge couldn't serve the read (e.g. not
  // configured yet). UNKNOWN — same as a throw.
  if (resp.success === false) return null;

  // From here the read SUCCEEDED. Any malformed/absent/corrupt value is a
  // genuine empty `{}` (recoverable): we deliver once and recordWarning then
  // overwrites the bad value with a fresh valid map. Returning null here would
  // suppress the warning forever AND never repair the corruption.
  const value = (resp.data as { value?: unknown } | undefined)?.value;
  if (typeof value !== "string") return {};
  try {
    const parsed = JSON.parse(value) as unknown;
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as Record<string, unknown>;
  } catch {
    // Corrupt JSON, but the read succeeded — treat as empty/recoverable.
    return {};
  }
}

/**
 * Tri-state dedup check:
 *   - "warned": the key is recorded — skip delivery.
 *   - "fresh": state read OK, key absent — deliver + record.
 *   - "unknown": state could not be read — do NOT deliver (can't dedup, so
 *     delivering would risk spamming). The next configured call retries.
 */
async function warnedStatus(
  bridge: Pick<AftProjectTransport, "send">,
  key: string,
): Promise<"warned" | "fresh" | "unknown"> {
  const warned = await readWarnedTools(bridge);
  if (warned === null) return "unknown";
  return warned[key] === true || typeof warned[key] === "string" ? "warned" : "fresh";
}

async function recordWarning(
  bridge: Pick<AftProjectTransport, "send">,
  key: string,
): Promise<void> {
  // Read-modify-write. If the read failed (null), do NOT write — a blind
  // `{}` write would clobber previously-recorded keys and re-open the
  // re-fire window. We only reach here after a "fresh" status, which means
  // the read succeeded, so null is not expected; guard anyway.
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

function formatConfigureWarningLine(warning: ConfigureWarning): string {
  if (warning.kind === "config_key_dropped") return `• ${warning.hint}`;

  const details: string[] = [];
  if (warning.language) details.push(`language: ${warning.language}`);
  if (warning.server) details.push(`server: ${warning.server}`);
  if (warning.tool) details.push(`tool: ${warning.tool}`);
  if (warning.binary && warning.binary !== warning.tool) {
    details.push(`binary: ${warning.binary}`);
  }

  const suffix = details.length > 0 ? ` (${details.join(", ")})` : "";
  return `• ${warningTitle(warning)}${suffix}\n  ${warning.hint}`;
}

function formatConfigureWarningChat(warning: ConfigureWarning): string {
  if (warning.kind === "config_key_dropped") return `${WARNING_MARKER} ${warning.hint}`;
  return `${WARNING_MARKER} ${formatConfigureWarningLine(warning).replace(/^• /, "")}`;
}

function formatConfigureWarningsBatch(warnings: ConfigureWarning[]): string {
  return warnings.map(formatConfigureWarningLine).join("\n\n");
}

async function deliverConfigureWarningBatch(
  opts: ConfigureWarningOptions,
  warnings: ConfigureWarning[],
): Promise<boolean> {
  if (warnings.length === 0) return false;
  const delivery = opts.delivery ?? "toast";
  const message = formatConfigureWarningsBatch(warnings);
  const title = warnings.length === 1 ? `AFT: ${warningTitle(warnings[0])}` : "AFT: Missing tools";
  if (delivery === "log") {
    warn(`[aft-plugin] configure warnings:\n${message}`);
    sessionLog(opts.sessionId, `[aft-plugin] configure warnings:\n${message}`);
    return true;
  }

  const toastSent = await showTuiToast(opts.client, title, message, "warning", 10_000);
  if (toastSent) return true;

  const effectiveServerUrl = opts.serverUrl || readDesktopState().serverUrl;
  if (effectiveServerUrl) {
    const httpToast = await showToastViaHttp(effectiveServerUrl, title, message, "warning", 10_000);
    if (httpToast) return true;
  }

  warn(`[aft-plugin] configure warnings (toast unavailable):\n${message}`);
  sessionLog(opts.sessionId, `[aft-plugin] configure warnings:\n${message}`);
  return true;
}

export async function deliverConfigureWarnings(
  opts: ConfigureWarningOptions,
  warnings: ConfigureWarning[],
): Promise<void> {
  if (opts.projectRoot) {
    await flushPendingDesktopNotifications(
      { client: opts.client, directory: opts.projectRoot },
      opts.sessionId,
    );
  }
  if (warnings.length === 0) return;

  const pending: ConfigureWarning[] = [];
  for (const warning of warnings) {
    const key = warningKey(warning, opts.projectRoot);
    const status = await warnedStatus(opts.bridge, key);
    // "warned": already delivered once — skip.
    // "unknown": dedup state couldn't be read (bridge not configured yet /
    //   RPC error). Do NOT deliver — delivering here is exactly what caused
    //   the warning to re-fire on every session. The next configured call
    //   reads real state and delivers once.
    if (status !== "fresh") continue;
    pending.push(warning);
  }
  if (pending.length === 0) return;

  const delivery = opts.delivery ?? "toast";
  if (delivery === "chat") {
    for (const warning of pending) {
      const ok = await sendIgnoredMessage(
        opts.client,
        opts.sessionId,
        formatConfigureWarningChat(warning),
        { includeAgent: false },
      );
      if (ok) {
        await recordWarning(opts.bridge, warningKey(warning, opts.projectRoot));
      }
    }
    return;
  }

  const delivered = await deliverConfigureWarningBatch(opts, pending);
  if (!delivered) return;

  for (const warning of pending) {
    await recordWarning(opts.bridge, warningKey(warning, opts.projectRoot));
  }
}

/**
 * Clean up stale AFT warning messages from previous runs.
 * Desktop only — TUI toasts are inherently transient and don't need cleanup.
 */
export async function cleanupWarnings(opts: NotificationOptions): Promise<void> {
  if (isTuiMode()) return; // TUI toasts don't persist

  const sessionId = getExplicitSessionId(opts);
  if (!sessionId) return;

  const effectiveServerUrl = opts.serverUrl || readDesktopState().serverUrl;
  if (!effectiveServerUrl) return;

  const messages = await getSessionMessages(opts.client, sessionId);
  if (messages.length === 0) return;

  // Scan from end for consecutive AFT warning messages
  const warningIds: string[] = [];
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i];
    const msgId = msg.info?.id;
    if (!msgId || msg.info?.role !== "user") break;

    const isAftWarning =
      msg.parts?.length &&
      msg.parts.every(
        (p) =>
          p.ignored === true &&
          p.type === "text" &&
          typeof p.text === "string" &&
          p.text.startsWith(WARNING_MARKER),
      );

    if (isAftWarning) {
      warningIds.push(msgId);
    } else {
      break;
    }
  }

  if (warningIds.length === 0) return;

  sessionLog(sessionId, `[aft-plugin] cleaning up ${warningIds.length} stale warning(s)`);
  for (const id of warningIds) {
    await deleteMessage(effectiveServerUrl, sessionId, id);
  }
}
