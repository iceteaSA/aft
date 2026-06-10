/**
 * Shared helpers for plugin tool handlers.
 *
 * Every tool that talks to the Rust binary should use `callBridge()` instead
 * of calling `ctx.pool.getBridge(...).send(...)` directly. The helper:
 *
 *   1. Resolves the project root from `context.worktree ?? context.directory`
 *      (canonical path), so two tool calls in the same project always reach
 *      the same bridge even if the agent's cwd momentarily differs.
 *   2. Injects `session_id` from `context.sessionID` into every request so the
 *      Rust side can partition undo/checkpoint state per OpenCode session
 *      (issue #14 — one shared bridge per project, N sessions per bridge).
 *
 * Tools that specifically need the raw `BinaryBridge` (for example to call
 * `bridge.send()` multiple times with shared state) should use `bridgeFor()`
 * and still pass `session_id` explicitly.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { BinaryBridge, BridgeRequestOptions } from "@cortexkit/aft-bridge";
import { timeoutForCommand } from "@cortexkit/aft-bridge";
import { tool } from "@opencode-ai/plugin";
import { ingestBgCompletions } from "../bg-notifications.js";
import {
  getSessionDirectory,
  getSessionDirectoryCached,
  warmSessionDirectory,
} from "../shared/session-directory.js";
import { markBridgeEnd, markBridgeStart } from "../tool-perf.js";
import type { PluginContext } from "../types.js";

const z = tool.schema;

/**
 * Optional integer with bounds.
 *
 * MUST be JSON-Schema-representable for OpenCode tool registration to
 * succeed: OpenCode wraps plugin args in a host `z.object()` and runs
 * `z.toJSONSchema(args, { io: "input" })` at session start. Any node
 * the host's Zod can't convert (e.g. `.transform()`, `.preprocess()`)
 * throws "Transforms cannot be represented in JSON Schema" and the
 * entire plugin fails to load. Keep this a plain schema — no
 * transforms. Empty-sentinel coercion (null/""/0 → undefined) belongs
 * in tool handlers via `coerceOptionalInt`, not in the schema.
 *
 * Regression guard: `tool-schemas-json-convertible.test.ts` runs
 * `z.toJSONSchema(z.object(args), { io: "input" })` on every plugin
 * tool. If anyone reintroduces a `.transform()` here it fails before
 * shipping.
 *
 * Return type is `any` to suppress TS2742 — Zod's inferred type leaks
 * `.bun/zod@...` paths that aren't portable across the host SDK and
 * our zod version. The type annotation has no runtime effect; the
 * contract test is the real invariant.
 */
// biome-ignore lint/suspicious/noExplicitAny: tool.schema's bounded-int return type isn't portably nameable; contract test enforces the actual invariant.
export const optionalInt = (min: number, max: number): any =>
  z.number().int().min(min).max(max).optional();

/**
 * Runtime coercion for agent-friendly sentinel handling.
 *
 * Some agents emit null / "" / 0 when they mean "param not provided".
 * Use this inside tool handlers BEFORE relying on the value. Returns
 * `undefined` for all empty sentinels; rejects out-of-bounds with a
 * clear message.
 *
 * Tool handlers that want sentinel tolerance must pass args through
 * this AFTER Zod validation has accepted the value (or for fields
 * declared as `z.unknown().optional()` that bypass type validation).
 * With `optionalInt`'s bounded `z.number().int()` schema, Zod already
 * rejects the sentinels — call this for defense in depth or for fields
 * declared more permissively.
 */
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

// Bridge transport budget for bash-family calls. Rust bash returns `running`
// promptly and plugin-side polling handles task lifetime, so transport only
// covers spawn/control RPC round-trips. Keep this centralized so every
// bash-family RPC also keeps the shared bridge alive on transport timeout.
export const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

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

/**
 * Minimum shape of the per-tool-call context provided by the OpenCode SDK.
 *
 * We only depend on a few fields so any similar context (including the Pi
 * plugin's `ExtensionContext`) can be passed through the same helpers once
 * they adopt session-aware calls.
 */
export interface ToolRuntime {
  /** Worktree root (preferred); falls back to `directory` when absent. */
  worktree?: string;
  /** Agent's working directory for this tool call. */
  directory: string;
  /** Opaque OpenCode session identifier. Missing in CLI tests / some hosts. */
  sessionID?: string;
}

/**
 * Canonicalize a directory path: strip trailing separators, resolve symlinks
 * via `realpath`, fall back to lexical resolution if the path doesn't exist.
 *
 * Used both for the canonical project-root key and for verifying the
 * session-stored directory before we use it for routing.
 */
function canonicalizeDirectory(dir: string): string {
  const trimmed = dir.replace(/[/\\]+$/, "");
  try {
    return fs.realpathSync(trimmed);
  } catch {
    return path.resolve(trimmed);
  }
}

/**
 * Resolve the canonical project root for a runtime.
 *
 * Prefers `worktree` because that stays stable across OpenCode sessions in
 * the same project; falls back to `directory` when unavailable (standalone
 * CLI use, older hosts). Normalizes trailing slashes and resolves symlinks
 * so `/repo` and `/repo/` and `/Users/.../repo -> /Volumes/...` collapse to
 * the same key.
 *
 * NOTE: When the runtime carries a `sessionID` and we have a cached
 * session-stored directory for it (see `shared/session-directory.ts`), the
 * stored directory wins. This is the workaround for OpenCode's bug where
 * `ctx.directory` is set to `process.cwd()` rather than the resumed
 * session's actual project directory.
 */
export function projectRootFor(runtime: ToolRuntime): string {
  // Workaround: if OpenCode handed us a session ID and the session has a
  // resolved directory in our cache, use that. This survives `opencode -s`
  // launched from the wrong cwd.
  const cached = getSessionDirectoryCached(runtime.sessionID);
  if (typeof cached === "string" && cached.length > 0) {
    return canonicalizeDirectory(cached);
  }

  const raw = runtime.worktree ?? runtime.directory;
  return canonicalizeDirectory(raw);
}

/**
 * Warm the session-directory cache, then return the same project root that
 * callBridge()/bridgeFor() will use for this tool call. Permission checks must
 * resolve paths through this helper before dispatch so the path the user
 * approves is byte-for-byte the path the Rust bridge acts on.
 */
export async function resolveProjectRoot(
  ctx: PluginContext,
  runtime: ToolRuntime,
): Promise<string> {
  if (runtime.sessionID && getSessionDirectoryCached(runtime.sessionID) === undefined) {
    await getSessionDirectory(ctx.client, runtime.sessionID, runtime.directory);
  }
  return projectRootFor(runtime);
}

/**
 * Expand a leading `~` to the user's home directory. Node's `path.resolve`
 * treats `~` as a literal segment, so `~/foo` would otherwise resolve to
 * `<projectRoot>/~/foo`. Applied before any absolute/relative decision so all
 * file tools (read/write/edit/outline/zoom/delete/refactor/imports/safety)
 * accept `~/...` the same way the search tools already do.
 */
export function expandTilde(input: string): string {
  if (!input || !input.startsWith("~")) return input;
  if (input === "~") return os.homedir();
  if (input.startsWith("~/") || input.startsWith(`~${path.sep}`)) {
    return path.resolve(os.homedir(), input.slice(2));
  }
  // Leave `~user` forms untouched — we don't resolve other users' homes.
  return input;
}

/** Resolve a user path exactly as a bridge request will: `~` expands to home,
 * absolute paths are preserved; relative paths are rooted at the
 * session/project root. */
export function resolvePathFromProjectRoot(projectRoot: string, target: string): string {
  const expanded = expandTilde(target);
  return path.isAbsolute(expanded) ? expanded : path.resolve(projectRoot, expanded);
}

export async function resolvePathArg(
  ctx: PluginContext,
  runtime: ToolRuntime,
  target: string,
): Promise<string> {
  return resolvePathFromProjectRoot(await resolveProjectRoot(ctx, runtime), target);
}

/**
 * Get the BinaryBridge for the runtime's project root.
 *
 * Prefer `callBridge()` unless you need to send multiple requests yourself.
 *
 * This is synchronous and uses only the cached session directory. If the
 * cache is cold, it falls back to `runtime.directory` — `callBridge()`
 * eagerly warms the cache before calling this so the cache is hot for
 * subsequent calls in the same session.
 */
export function bridgeFor(ctx: PluginContext, runtime: ToolRuntime): BinaryBridge {
  return ctx.pool.getBridge(projectRootFor(runtime));
}

/**
 * Send a single command to the Rust binary with `session_id` injected.
 *
 * This is the canonical way for a tool handler to call AFT: the helper picks
 * the right bridge (project-keyed), attaches the session namespace from
 * `context.sessionID`, and returns whatever the binary responds.
 *
 * Before routing, it ensures the session-directory cache is warm so the
 * very first tool call on a resumed-from-wrong-cwd session still reaches
 * the correct project bridge. Subsequent calls hit the cache synchronously.
 *
 * The Rust side falls back to a shared default namespace when `session_id`
 * is absent (see `RawRequest::session()`), so hosts that don't expose a
 * session identifier still work — they just share undo/checkpoint state.
 */
export async function callBridge(
  ctx: PluginContext,
  runtime: ToolRuntime,
  command: string,
  params: Record<string, unknown> = {},
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  // Resolve the session's stored project directory once on first call —
  // OpenCode sets `runtime.directory = process.cwd()` even for resumed
  // sessions, so we can't trust it as the workspace root. Subsequent
  // calls in the same session hit the cache and skip the lookup.
  if (runtime.sessionID && getSessionDirectoryCached(runtime.sessionID) === undefined) {
    await getSessionDirectory(ctx.client, runtime.sessionID, runtime.directory);
  }

  const merged: Record<string, unknown> = { ...params };
  if (runtime.sessionID) {
    merged.session_id = runtime.sessionID;
  }
  const timeoutMs = timeoutForCommand(command);
  const sendOptions = {
    ...(timeoutMs !== undefined ? { timeoutMs } : {}),
    configureWarningClient: ctx.client,
    ...options,
  };
  markBridgeStart();
  let response: Awaited<ReturnType<BinaryBridge["send"]>>;
  try {
    response = await bridgeFor(ctx, runtime).send(
      command,
      merged,
      Object.keys(sendOptions).length > 0 ? sendOptions : undefined,
    );
  } finally {
    markBridgeEnd();
  }
  ingestBgCompletions(runtime.sessionID, response.bg_completions);
  return response;
}

/**
 * Send a bash-family command without restarting the shared bridge on transport
 * timeout. Bash has its own child-process timeout/watchdog handling; a late
 * transport response must not sacrifice the warm shared bridge and reject
 * unrelated sibling requests.
 */
export async function callBashBridge(
  ctx: PluginContext,
  runtime: ToolRuntime,
  command: string,
  params: Record<string, unknown> = {},
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  return await callBridge(ctx, runtime, command, params, {
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
    ...options,
    keepBridgeOnTimeout: true,
  });
}

/**
 * Eagerly warm the session-directory cache for a runtime. Safe to call from
 * synchronous code — the lookup runs in the background and failures are
 * logged. Useful in plugin lifecycle hooks (`chat.message`, etc.) where we
 * want the cache filled before any tool call arrives.
 */
export function warmSessionDirectoryFromRuntime(ctx: PluginContext, runtime: ToolRuntime): void {
  warmSessionDirectory(ctx.client, runtime.sessionID, runtime.directory);
}
