/**
 * Shared helpers used by every Pi tool wrapper.
 */

import { existsSync } from "node:fs";
import type {
  AftProjectTransport,
  BridgeRequestOptions,
  ToolCallOptions,
  ToolCallResult,
} from "@cortexkit/aft-bridge";
import { formatBridgeErrorMessage, timeoutForCommand } from "@cortexkit/aft-bridge";
import type { AgentToolResult, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { ingestBgCompletions } from "../bg-notifications.js";
import type { PluginContext } from "../types.js";

type TextContent = { type: "text"; text: string; textSignature?: string };
type ImageContent = { type: "image"; data: string; mimeType: string };
type ContentBlock = TextContent | ImageContent;

/**
 * Optional integer field schema for Pi tool parameters.
 *
 * Pi validates tool arguments against this schema BEFORE our handler runs, and
 * some models send stringified integers like "42". A strict `Type.Integer()`
 * would reject those calls before `coerceOptionalInt()` can normalize them, so
 * keep the schema permissive at the field level while still documenting the
 * real integer contract for models and host UIs.
 */
export const optionalInt = (min: number, max: number, description = "(integer)") =>
  Type.Optional(
    Type.Union([Type.Integer({ minimum: min, maximum: max }), Type.String()], {
      description,
    }),
  );

// Re-exported from @cortexkit/aft-bridge — shared runtime coercion,
// formatting, and timeout tables live in the host-neutral bridge package.
export {
  coerceOptionalInt,
  formatBridgeErrorMessage,
  isEmptyParam,
  LONG_RUNNING_COMMAND_TIMEOUT_MS,
  timeoutForCommand,
} from "@cortexkit/aft-bridge";

/** Get the session bridge for the current working directory. */
export function bridgeFor(ctx: PluginContext, cwd: string): AftProjectTransport {
  // A restored session can point at a reclaimed mason worktree (or any
  // deleted directory). Binding it would make the module configure, lease,
  // and warm indexes for a dead root — refuse before any transport work.
  if (!existsSync(cwd)) {
    throw new Error(`project directory no longer exists: ${cwd} (stale restored session?)`);
  }
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
  readonly response?: Record<string, unknown>;
  constructor(message: string, code: string, response?: Record<string, unknown>) {
    super(message);
    this.name = "BridgeError";
    this.code = code;
    this.response = response;
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
  bridge: AftProjectTransport,
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
      response,
    );
  }
  ingestBgCompletions(sessionId, response.bg_completions);
  return response;
}

/**
 * Wrapper that calls a tool on the Pi agent. It supplies the session ID and
 * timeout, forwards warnings, gathers any follow-up data, and returns the raw
 * response plus the text summary the model will receive.
 */
export async function callToolCall(
  bridge: AftProjectTransport,
  name: string,
  rawArgs: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: ToolCallOptions,
): Promise<ToolCallResult> {
  const timeoutMs = timeoutForCommand(name);
  const sessionId = extCtx ? resolveSessionId(extCtx) : undefined;
  const sendOptions = {
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    configureWarningClient: extCtx,
    ...options,
  };
  const response = await bridge.toolCall(
    sessionId,
    name,
    rawArgs,
    Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
  );
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
  return contentResult([{ type: "text", text }], details);
}

/** Build an AgentToolResult that can include image content blocks. */
export function contentResult<TDetails = unknown>(
  content: ContentBlock[],
  details?: TDetails,
): AgentToolResult<TDetails> {
  return {
    content,
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
