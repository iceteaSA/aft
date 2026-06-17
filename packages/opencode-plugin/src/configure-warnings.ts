/**
 * Configure-warning delivery helper.
 *
 * IMPORTANT — DO NOT MOVE BACK INTO `index.ts`.
 *
 * OpenCode's plugin loader (`getLegacyPlugins` in
 * `~/Work/OSS/opencode/packages/opencode/src/plugin/index.ts`) walks
 * `Object.values(mod)` of the plugin's main module and treats every
 * top-level export as either a server plugin function or an object
 * with a `.server` plugin function. Anything else throws
 * `TypeError: Plugin export is not a function` and the plugin fails to
 * load. Function exports get called as plugins, their (often `void`)
 * return value gets pushed into the hooks array, and the next iteration
 * over hooks crashes the host with
 * `undefined is not an object (evaluating 'z.config')` (and a sibling
 * `S.provider` for other hook iterations).
 *
 * Putting this helper in its own module keeps `index.ts` to exactly one
 * default export — the plugin function itself — and lets tests import
 * from this file directly.
 */

import type { BinaryBridge } from "@cortexkit/aft-bridge";
import {
  type ConfigLoadError,
  type ConfigureWarningsDelivery,
  formatConfigParseFailureMessage,
} from "./config.js";
import { warn } from "./logger.js";
import { type ConfigureWarning, deliverConfigureWarnings } from "./notifications.js";

const pendingEagerWarnings = new Map<string, ConfigureWarning[]>();
const pendingConfigParseWarnings = new Map<string, ConfigureWarning[]>();

type PendingSessionWarnings = {
  warnings: ConfigureWarning[];
  client: unknown;
  bridge: Pick<BinaryBridge, "send">;
  storageDir: string;
  pluginVersion: string;
  projectRoot: string;
  serverUrl?: string;
  delivery: ConfigureWarningsDelivery;
};

const pendingBySession = new Map<string, PendingSessionWarnings>();

function isConfigureWarning(value: unknown): value is ConfigureWarning {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const warning = value as Record<string, unknown>;
  return (
    (warning.kind === "formatter_not_installed" ||
      warning.kind === "checker_not_installed" ||
      warning.kind === "lsp_binary_missing" ||
      warning.kind === "config_parse_failed") &&
    typeof warning.hint === "string"
  );
}

function configParseWarningsFromErrors(errors: readonly ConfigLoadError[]): ConfigureWarning[] {
  return errors.map((entry) => ({
    kind: "config_parse_failed" as const,
    hint: formatConfigParseFailureMessage(entry.path, entry.message),
  }));
}

/** Buffer config syntax failures until a session-bound configure warning flush (deduped by hint). */
export function enqueueConfigParseWarnings(
  projectRoot: string,
  errors: readonly ConfigLoadError[],
): void {
  if (!projectRoot || errors.length === 0) return;
  const incoming = configParseWarningsFromErrors(errors);
  const existing = pendingConfigParseWarnings.get(projectRoot) ?? [];
  for (const warning of incoming) {
    if (!existing.some((item) => item.hint === warning.hint)) {
      existing.push(warning);
    }
  }
  pendingConfigParseWarnings.set(projectRoot, existing);
}

export function drainPendingConfigParseWarnings(projectRoot: string): ConfigureWarning[] {
  const pending = pendingConfigParseWarnings.get(projectRoot) ?? [];
  pendingConfigParseWarnings.delete(projectRoot);
  return pending;
}

function coerceConfigureWarnings(warnings: unknown[]): ConfigureWarning[] {
  return warnings.filter(isConfigureWarning);
}

export function drainPendingEagerWarnings(projectRoot: string): ConfigureWarning[] {
  const pending = pendingEagerWarnings.get(projectRoot) ?? [];
  pendingEagerWarnings.delete(projectRoot);
  return pending;
}

/** Test-only reset for queued configure warnings. */
export function __resetConfigureWarningQueuesForTests(): void {
  pendingEagerWarnings.clear();
  pendingConfigParseWarnings.clear();
  pendingBySession.clear();
}

export function enqueueConfigureWarningsForSession(context: {
  projectRoot: string;
  sessionId?: string | null;
  client?: unknown;
  bridge: Pick<BinaryBridge, "send">;
  warnings: unknown[];
  fallbackClient: unknown;
  storageDir: string;
  pluginVersion: string;
  serverUrl?: string;
  delivery?: ConfigureWarningsDelivery;
}): void {
  const validWarnings = [
    ...drainPendingConfigParseWarnings(context.projectRoot),
    ...coerceConfigureWarnings(context.warnings),
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

  const pendingWarnings = drainPendingEagerWarnings(context.projectRoot);
  const combinedWarnings = [...pendingWarnings, ...validWarnings];
  if (combinedWarnings.length === 0) return;

  const existing = pendingBySession.get(context.sessionId);
  if (existing) {
    existing.warnings.push(...combinedWarnings);
    return;
  }

  pendingBySession.set(context.sessionId, {
    warnings: combinedWarnings,
    client: context.client ?? context.fallbackClient,
    bridge: context.bridge,
    storageDir: context.storageDir,
    pluginVersion: context.pluginVersion,
    projectRoot: context.projectRoot,
    serverUrl: context.serverUrl,
    delivery: context.delivery ?? "toast",
  });
}

/** Deliver queued configure warnings after the session goes idle (avoids mid-turn prompt side effects). */
export async function flushConfigureWarningsOnIdle(sessionId: string): Promise<void> {
  const pending = pendingBySession.get(sessionId);
  if (!pending) return;
  pendingBySession.delete(sessionId);

  await deliverConfigureWarnings(
    {
      client: pending.client,
      sessionId,
      bridge: pending.bridge,
      storageDir: pending.storageDir,
      pluginVersion: pending.pluginVersion,
      projectRoot: pending.projectRoot,
      serverUrl: pending.serverUrl,
      delivery: pending.delivery,
    },
    pending.warnings,
  );
}

/** @deprecated Use {@link enqueueConfigureWarningsForSession} + {@link flushConfigureWarningsOnIdle}. */
export async function handleConfigureWarningsForSession(context: {
  projectRoot: string;
  sessionId?: string | null;
  client?: unknown;
  bridge: Pick<BinaryBridge, "send">;
  warnings: unknown[];
  fallbackClient: unknown;
  storageDir: string;
  pluginVersion: string;
  serverUrl?: string;
  delivery?: ConfigureWarningsDelivery;
}): Promise<void> {
  enqueueConfigureWarningsForSession(context);
  if (context.sessionId) {
    await flushConfigureWarningsOnIdle(context.sessionId);
  }
}
