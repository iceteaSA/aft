/**
 * Shared helpers used by every Pi tool wrapper.
 */

import type { BinaryBridge, BridgeRequestOptions } from "@cortexkit/aft-bridge";
import { timeoutForCommand } from "@cortexkit/aft-bridge";
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
  if (typeof v === "number" && (v === 0 || !Number.isFinite(v))) return undefined;
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
export { LONG_RUNNING_COMMAND_TIMEOUT_MS, timeoutForCommand } from "@cortexkit/aft-bridge";

function asPlainObject(value: unknown): Record<string, unknown> | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

function candidateLocation(candidate: Record<string, unknown>): string | undefined {
  const file =
    typeof candidate.file === "string" && candidate.file.length > 0 ? candidate.file : undefined;
  if (!file) return undefined;
  const line =
    typeof candidate.line === "number" && Number.isFinite(candidate.line)
      ? candidate.line
      : undefined;
  return line === undefined ? file : `${file}:${line}`;
}

function stringifyData(data: unknown): string | undefined {
  if (data === undefined) return undefined;
  try {
    return JSON.stringify(data, null, 2);
  } catch {
    return String(data);
  }
}

/** Format bridge failure envelopes without dropping structured error data. */
export function formatBridgeErrorMessage(
  command: string,
  response: Record<string, unknown>,
  params: Record<string, unknown> = {},
): string {
  const code =
    typeof response.code === "string" && response.code.length > 0 ? response.code : undefined;
  const message =
    typeof response.message === "string" && response.message.length > 0
      ? response.message
      : `${command} failed`;
  // Rust merges error_with_data() extras into the top-level response, NOT under
  // a nested `data` field. Read structured fields at top-level first; fall back
  // to `response.data` for forward-compat with any handler that uses nesting.
  const data = asPlainObject(response.data);
  const rawCandidates = Array.isArray(response.candidates)
    ? response.candidates
    : Array.isArray(data?.candidates)
      ? data.candidates
      : undefined;
  const rawSymbol =
    typeof response.symbol === "string" && response.symbol.length > 0
      ? response.symbol
      : typeof data?.symbol === "string" && data.symbol.length > 0
        ? data.symbol
        : undefined;

  if (code === "ambiguous_target" || code === "target_symbol_not_in_file") {
    const candidates = (rawCandidates ?? [])
      .map(asPlainObject)
      .filter((candidate): candidate is Record<string, unknown> => candidate !== undefined)
      .map(candidateLocation)
      .filter((candidate): candidate is string => candidate !== undefined);

    if (candidates.length > 0) {
      const symbol =
        typeof params.toSymbol === "string" && params.toSymbol.length > 0
          ? params.toSymbol
          : rawSymbol;
      const target = symbol ? `multiple symbols named "${symbol}"` : message.replace(/[.!?]+$/, "");
      const action =
        code === "ambiguous_target"
          ? "Pass toFile to disambiguate"
          : "Try one of these files for toFile";
      return `${command}: ${code} — ${target}. ${action}:\n${candidates
        .map((candidate) => `  - ${candidate}`)
        .join("\n")}`;
    }
  }

  if (!code) return message;

  const lines = [`${command}: ${code} — ${message}`];
  // For unhandled structured error codes, surface any extra fields beyond
  // code/message/success/id so agents see the full context (not just data.*).
  const extras = collectStructuredExtras(response);
  if (extras) lines.push(`data: ${extras}`);
  return lines.join("\n");
}

/**
 * Capture any structured fields a Rust error_with_data() merged into the top-level
 * response, excluding the well-known envelope keys (id/success/code/message) and
 * already-shown nested `data` (handled separately when present).
 */
function collectStructuredExtras(response: Record<string, unknown>): string | undefined {
  const reserved = new Set(["id", "success", "code", "message", "data"]);
  const extras: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(response)) {
    if (reserved.has(key)) continue;
    extras[key] = value;
  }
  if (Object.keys(extras).length === 0) {
    return stringifyData(response.data);
  }
  // Prefer top-level extras; fold any nested data fields beneath.
  if (response.data !== undefined) extras.data = response.data;
  return stringifyData(extras);
}

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
