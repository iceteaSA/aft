/**
 * Shared helpers used by every Pi tool wrapper.
 */

import type { BinaryBridge, BridgeRequestOptions } from "@cortexkit/aft-bridge";
import { formatBridgeErrorMessage, timeoutForCommand } from "@cortexkit/aft-bridge";
import type { AgentToolResult, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { ingestBgCompletions } from "../bg-notifications.js";
import type { PluginContext } from "../types.js";

export const optionalInt = (_min: number, _max: number) =>
  Type.Optional(Type.Any({ description: "(integer)" }));

export function coerceOptionalInt(
  v: unknown,
  paramName: string,
  min: number,
  max: number,
): number | undefined {
  if (v === undefined || v === null || v === "") return undefined;
  // 0 is an empty-param sentinel ONLY when 0 is out of bounds anyway. For
  // 0-indexed params (edit's `occurrence`, min=0) it is the most common legal
  // value — dropping it sent agents into an ambiguous_match loop that told
  // them to pass the param they had just passed.
  if (typeof v === "number" && (!Number.isFinite(v) || (v === 0 && min > 0))) return undefined;
  const n = typeof v === "string" ? Number(v) : v;
  if (typeof n !== "number" || !Number.isInteger(n)) {
    throw new Error(`${paramName} must be an integer between ${min} and ${max}`);
  }
  if (n < min || n > max) {
    throw new Error(`${paramName} must be between ${min} and ${max}`);
  }
  return n;
}

/**
 * True when a value represents "agent did not provide this param".
 *
 * GPT-family models send empty strings / empty arrays / null instead of
 * omitting optional params entirely. Use this BEFORE mutual-exclusion
 * checks so an empty `targets: []` or `url: ""` doesn't get counted as
 * present and trigger a misleading "X is mutually exclusive with Y" error.
 *
 * Treats undefined / null / "" / [] / {} as empty. Booleans and numbers
 * (including 0 and false) are NOT empty by themselves — only string and
 * collection sentinels qualify.
 */
export function isEmptyParam(value: unknown): boolean {
  if (value === undefined || value === null) return true;
  if (typeof value === "string") return value.length === 0;
  if (Array.isArray(value)) return value.length === 0;
  if (typeof value === "object") return Object.keys(value as object).length === 0;
  return false;
}

// Re-exported from @cortexkit/aft-bridge — the table lives next to the
// bridge's semantic-timeout clamp so the two can never drift apart.
export {
  formatBridgeErrorMessage,
  LONG_RUNNING_COMMAND_TIMEOUT_MS,
  timeoutForCommand,
} from "@cortexkit/aft-bridge";

/** Get the session bridge for the current working directory. */
export function bridgeFor(ctx: PluginContext, cwd: string): BinaryBridge {
  return ctx.pool.getBridge(cwd);
}

/**
 * Resolve Pi's native session ID from the tool execution context so that
 * `/new`, `/fork`, and `/resume` each scope their own undo/checkpoint
 * namespace in AFT instead of sharing one extension-wide UUID.
 *
 * `sessionManager` is on every `ExtensionContext`; we read it defensively
 * because Pi's public type surface is still evolving and we don't want a
 * missing field at runtime to wedge tool execution.
 */
export function resolveSessionId(extCtx: ExtensionContext): string | undefined {
  const manager = (extCtx as unknown as { sessionManager?: { getSessionId?: () => string } })
    .sessionManager;
  const id = manager?.getSessionId?.();
  return typeof id === "string" && id.length > 0 ? id : undefined;
}

/**
 * Error thrown by callBridge on a `success: false` response. Carries the Rust
 * error `code` so callers can distinguish soft negatives (e.g. symbol_not_found)
 * from genuine errors without re-parsing the message.
 */
export class BridgeError extends Error {
  readonly code: string;
  constructor(message: string, code: string) {
    super(message);
    this.name = "BridgeError";
    this.code = code;
  }
}

/**
 * Call a bridge command and throw a BridgeError on failure.
 * Every tool handler should guard with `if (response.success === false)`
 * before accessing success-only fields — this helper does it uniformly.
 *
 * `extCtx` is used to derive Pi's current session ID per call so Rust
 * scopes backups/undo per Pi session rather than per extension instance.
 */
export async function callBridge(
  bridge: BinaryBridge,
  command: string,
  params: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const timeoutMs = timeoutForCommand(command);
  const merged: Record<string, unknown> = { ...params };
  const sessionId = extCtx ? resolveSessionId(extCtx) : undefined;
  if (sessionId) {
    merged.session_id = sessionId;
  }
  const sendOptions = {
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    configureWarningClient: extCtx,
    ...options,
  };
  const response = await bridge.send(
    command,
    merged,
    Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
  );
  if (response.success === false) {
    throw new BridgeError(
      formatBridgeErrorMessage(command, response, merged),
      typeof response.code === "string" ? response.code : "",
    );
  }
  ingestBgCompletions(sessionId, response.bg_completions);
  return response;
}

/**
 * Build a text-only AgentToolResult.
 * This is the standard result shape for most AFT tools.
 */
export function textResult<TDetails = unknown>(
  text: string,
  details?: TDetails,
): AgentToolResult<TDetails> {
  return {
    content: [{ type: "text", text }],
    details: details as TDetails,
  };
}

/**
 * Convert a bridge response into a pretty JSON string for the model.
 * Strips undefined/null fields that just clutter the output.
 */
export function jsonTextResult<TDetails = unknown>(
  response: Record<string, unknown>,
  details?: TDetails,
): AgentToolResult<TDetails> {
  return textResult(JSON.stringify(response, null, 2), details);
}

/** Strip top-level success field before JSON stringifying. */
export function stripSuccess(response: Record<string, unknown>): Record<string, unknown> {
  const { success: _success, ...rest } = response;
  return rest;
}
