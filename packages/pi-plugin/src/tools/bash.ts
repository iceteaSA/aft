import * as fs from "node:fs/promises";
import {
  type BinaryBridge,
  type BridgeRequestOptions,
  coerceBoolean,
  isBridgeTransportTimeout,
  isTerminalStatus,
  maybeAppendConflictsHint,
  maybeAppendGrepSearchHint,
  resolveBashKillTimeout,
  sleep,
} from "@cortexkit/aft-bridge";
import type {
  AgentToolResult,
  ExtensionAPI,
  ExtensionContext,
  Theme,
} from "@earendil-works/pi-coding-agent";
import { Container, Spacer, Text } from "@earendil-works/pi-tui";
import { type Static, Type } from "typebox";
import {
  consumeBgCompletion,
  markBgCompletionDelivered,
  markExplicitControl,
  markTaskWaiting,
  trackBgTask,
  unmarkExplicitControl,
  unmarkTaskWaiting,
} from "../bg-notifications.js";
import { resolveBashConfig } from "../config.js";
import { clearSyncWatchAbort, isSyncWatchAborted } from "../sync-watch-abort.js";
import type { PluginContext } from "../types.js";
import {
  BridgeError,
  bridgeFor,
  callBridge,
  coerceOptionalInt,
  optionalInt,
  resolveSessionId,
  textResult,
} from "./_shared.js";

const BASH_WAIT_POLL_INTERVAL_MS = 100;
const DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS = 30_000;
const MAX_BASH_STATUS_WAIT_TIMEOUT_MS = 30 * 60 * 1000;
const REGEX_WAIT_SCAN_WINDOW_BYTES = 64 * 1024;

// Test-only override for the foreground wait window. Production resolves the
// window from config (floored at 5000ms), but bun caps each test at 5000ms, so
// promotion tests need a sub-floor window to exercise the promote path
// deterministically. Mirrors the Rust `AFT_CALLGRAPH_BUILD_WAIT_MS` test seam.
// Never set outside tests.
function resolveForegroundWaitMs(configured: number): number {
  const override = process.env.AFT_TEST_FOREGROUND_WAIT_MS;
  if (override !== undefined) {
    const parsed = Number(override);
    if (Number.isFinite(parsed) && parsed >= 0) return parsed;
  }
  return configured;
}
// Baseline bridge transport budget for bash-family control calls. The main
// orchestrated bash tool overrides this per request because Rust may hold the
// final response until the foreground wait window or hard-kill cap elapses.
const BASH_TRANSPORT_TIMEOUT_MS = 30_000;
const DEFAULT_HARD_TIMEOUT_MS = 30 * 60 * 1000;
// The margin gives Rust time to promote or finalize the task and deliver the
// final response after the server's foreground wait window or hard kill timeout.
const BASH_TRANSPORT_MARGIN_MS = 10_000;

function orchestratedTransportTimeoutMs(
  blockToCompletion: boolean,
  effectiveTimeout: number | undefined,
  foregroundWaitMs: number,
): number {
  const waitBudget = blockToCompletion
    ? (effectiveTimeout ?? DEFAULT_HARD_TIMEOUT_MS)
    : foregroundWaitMs;
  return waitBudget + BASH_TRANSPORT_MARGIN_MS;
}

// Background task completion metadata shape (from Track D)
interface BgCompletion {
  task_id: string;
  status: "completed" | "failed" | "cancelled";
  exit_code?: number;
  command?: string;
}

// BashSpawnHook type — Pi's extension point for modifying bash execution
interface BashSpawnContext {
  command: string;
  cwd?: string;
  env?: Record<string, string>;
}

type BashSpawnHook = (ctx: BashSpawnContext) => BashSpawnContext | Promise<BashSpawnContext>;

const BashBaseParams = {
  command: Type.String({
    description: "Shell command to execute. Supports pipes, redirections, and shell syntax.",
  }),
  timeout: optionalInt(1, Number.MAX_SAFE_INTEGER),
  workdir: Type.Optional(
    Type.String({
      description:
        "Working directory for command execution. Relative paths resolve against the project root. Defaults to the current session's working directory.",
    }),
  ),
  description: Type.Optional(
    Type.String({
      description:
        "Human-readable description shown in UI logs. Helps users understand what the command does without reading shell syntax.",
    }),
  ),
};

const BashBackgroundFlagParam = {
  background: Type.Optional(
    Type.Boolean({
      description:
        "Spawn command in background and return immediately with a task_id. Use bash_watch to wait for completion or output patterns; bash_status for a one-shot snapshot only. Use bash_kill to terminate. Ideal for long-running tasks like builds or dev servers.",
    }),
  ),
};

const BashCompressionParam = {
  compressed: Type.Optional(
    Type.Boolean({
      description:
        "Compress output by removing ANSI codes, carriage returns, and excessive blank lines. Default: true. Set to false for raw terminal output including color codes.",
    }),
  ),
};

const BashPtyParams = {
  pty: Type.Optional(
    Type.Boolean({
      description:
        'Spawn the command in a real PTY for interactive programs. Implies background: true automatically. Inspect with bash_status({ task_id, output_mode: "screen" }) and send input with bash_write.',
    }),
  ),
  ptyRows: optionalInt(1, 60),
  ptyCols: optionalInt(1, 140),
};

const BashParams = Type.Object({
  ...BashBaseParams,
  ...BashBackgroundFlagParam,
  ...BashCompressionParam,
  ...BashPtyParams,
});
const BashForegroundOnlyParams = Type.Object({
  ...BashBaseParams,
  ...BashCompressionParam,
});

function bashParamsForConfig(backgroundEnabled: boolean): typeof BashParams {
  return (backgroundEnabled ? BashParams : BashForegroundOnlyParams) as typeof BashParams;
}

const BashTaskParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
});

const BashStatusParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
  output_mode: Type.Optional(
    Type.Union([Type.Literal("screen"), Type.Literal("raw"), Type.Literal("both")], {
      description:
        "PTY output rendering mode. Defaults to screen for PTY tasks and preserves existing behavior for piped tasks when omitted.",
    }),
  ),
});

const BashWatchParams = Type.Object({
  task_id: Type.String({
    description: "Background bash task id returned by bash({ background: true }).",
  }),
  pattern: Type.Optional(Type.Union([Type.String(), Type.Object({ regex: Type.String() })])),
  background: Type.Optional(Type.Boolean()),
  timeout_ms: optionalInt(1, MAX_BASH_STATUS_WAIT_TIMEOUT_MS),
  once: Type.Optional(Type.Boolean()),
});

const BashWriteParams = Type.Object({
  task_id: Type.String({
    description: "Background PTY task id returned by bash({ pty: true, background: true }).",
  }),
  // input accepts either a plain string (verbatim bytes) or a sequence array
  // mixing strings (text) with { key: "<name>" } objects (named control keys).
  // Rust validates each item; unknown key names return invalid_request.
  input: Type.Union(
    [
      Type.String(),
      Type.Array(
        Type.Union([
          Type.String(),
          Type.Object({
            key: Type.String({
              description:
                "Named control key, e.g. 'esc', 'enter', 'up', 'ctrl-c'. Case-insensitive.",
            }),
          }),
        ]),
      ),
    ],
    {
      description:
        "Either a string of verbatim bytes (e.g. 'print(1)\\n') OR an array mixing strings " +
        "and { key: '<name>' } objects for atomic text+key sequences. " +
        "Example: [ 'iHello', { key: 'esc' }, ':wq', { key: 'enter' } ]. " +
        "Allowed key names: enter, return, tab, space, backspace, esc, escape, up, down, " +
        "left, right, home, end, page-up, page-down, delete, insert, f1..f12, ctrl-a..ctrl-z.",
    },
  ),
});

interface BashDetails {
  exit_code?: number;
  duration_ms?: number;
  truncated?: boolean;
  output_path?: string;
  task_id?: string;
  bg_completions?: BgCompletion[];
}

interface BashStatusWaited {
  reason: "matched" | "exited" | "timeout" | "user_message" | "unavailable";
  elapsed_ms: number;
  match?: string;
  match_offset?: number;
}

/**
 * Minimal snapshot used when a watch ends without ever reading task status
 * (the bridge stayed busy past our deadline). Keeps the formatter from
 * dereferencing an absent snapshot.
 */
function unavailableSnapshot(): Record<string, unknown> {
  return { status: "unknown" };
}

interface BashStatusDetails {
  success: boolean;
  status: string;
  exit_code?: number;
  duration_ms?: number;
  output_preview?: string;
  command?: string;
  mode?: string;
  output_path?: string;
  pty_rows?: number;
  pty_cols?: number;
  pty_screen?: string;
  waited?: BashStatusWaited;
}

interface BashWriteDetails {
  success: boolean;
  bytes_written?: number;
}

interface BashKillDetails {
  success: boolean;
  status: string;
}

interface BashWatchDetails extends Record<string, unknown> {}

/** Local shape for Pi's render context — mirrors hoisted.ts pattern. */
interface RenderContextLike {
  lastComponent: import("@earendil-works/pi-tui").Component | undefined;
  isError: boolean;
}

async function callBashBridge(
  bridge: BinaryBridge,
  command: string,
  params: Record<string, unknown> = {},
  extCtx?: ExtensionContext,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  return await callBridge(bridge, command, params, extCtx, {
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
    ...options,
    keepBridgeOnTimeout: true,
  });
}

/** Truncate output to last N visual lines for terminal width. */
function truncateToVisualLines(text: string, maxLines: number): string {
  const lines = text.split("\n");
  if (lines.length <= maxLines) return text;
  return lines.slice(-maxLines).join("\n");
}

/** Reuse a compatible Text component from last render, or create fresh. */
function reuseText(last: import("@earendil-works/pi-tui").Component | undefined): Text {
  return last instanceof Text ? last : new Text("", 0, 0);
}

/** Reuse a compatible Container from last render, or create fresh. */
function reuseContainer(last: import("@earendil-works/pi-tui").Component | undefined): Container {
  return last instanceof Container ? last : new Container();
}

/** Extract BashSpawnHook from ExtensionAPI if available. */
function getBashSpawnHook(pi: ExtensionAPI): BashSpawnHook | undefined {
  // Pi exposes hooks via getHook() or similar — defensive access
  const api = pi as unknown as {
    getHook?: (name: string) => BashSpawnHook | undefined;
    hooks?: { bashSpawn?: BashSpawnHook };
  };
  if (typeof api.getHook === "function") {
    return api.getHook("bashSpawn");
  }
  return api.hooks?.bashSpawn;
}

export function registerBashTool(
  pi: ExtensionAPI,
  ctx: PluginContext,
  aftSearchRegistered = false,
): void {
  const spawnHook = getBashSpawnHook(pi);
  // Agent-facing wording: no internal vocabulary ("hoisted", "Rust handler",
  // "command rewriting") — describe what the tool does and what NOT to use it
  // for. The code-search prohibition steers to aft_search when registered,
  // else to the grep tool (same surface logic as the Rust grep footer). The
  // compression sentence only appears when compression is actually on —
  // advertising `compressed: false` when compression is disabled would
  // describe a no-op. Background/PTY/watch wording appears only when
  // `bash.background` is enabled.
  const searchSteer = aftSearchRegistered
    ? "use `aft_search` (concepts, identifiers, regex, literals), `read`, `aft_outline`, or `aft_zoom` instead"
    : "use the `grep` tool, `read`, `aft_outline`, or `aft_zoom` instead";
  const bashCfg = resolveBashConfig(ctx.config);
  const compressionSentence = bashCfg.compress
    ? " Output is compressed by default; pass `compressed: false` for raw output. Piped commands run verbatim and show the pipeline's output; for AFT's test/build summary, run the runner without `| head`, `| tail`, or `| grep`."
    : "";
  const tasksSentence = bashCfg.background
    ? ' Commands run in the foreground and return inline; a long-running one auto-promotes to background and delivers a completion reminder when it finishes — so for the common "I am waiting on this result" case, just run it and wait, no flags needed. Use `background: true` yourself ONLY when you have other useful work to do while it runs; then `bash_watch` waits on the task (sync blocks until exit/pattern, async notifies) and `bash_status` peeks at it — never background a command and immediately `bash_watch` it (that wastes a turn for what foreground returns in one), and never loop `bash_status` to wait. `pty: true` runs interactive programs (REPLs, TUIs), implies background, and is driven with `bash_status({ output_mode: "screen" })` plus `bash_write`.'
    : " Commands run in the foreground to completion; `timeout` is the hard kill cap (default 30 minutes).";
  pi.registerTool<typeof BashParams, BashDetails>({
    name: "bash",
    label: "bash",
    description: `Execute shell commands.${compressionSentence}${tasksSentence}

DO NOT use bash for code search or code exploration. If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — ${searchSteer}.`,
    promptSnippet: bashCfg.background
      ? "Run shell commands (timeout in milliseconds; supports workdir, background tasks, compressed output, PTY mode)"
      : "Run shell commands (timeout in milliseconds; supports workdir and compressed output)",
    promptGuidelines: [
      `DO NOT use bash for code search or exploration — ${searchSteer}.`,
      "Set compressed: false when you need ANSI color codes in the output.",
      "Piped commands run verbatim and show the pipeline's output; run test/build tools without pipes when you need AFT's summary.",
    ],
    parameters: bashParamsForConfig(bashCfg.background),
    async execute(_toolCallId, params: Static<typeof BashParams>, _signal, onUpdate, extCtx) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const bashCfg = resolveBashConfig(ctx.config);
      const foregroundWaitMs = resolveForegroundWaitMs(bashCfg.foreground_wait_window_ms);
      const backgroundDisabled = !bashCfg.background;
      // ptyRows/ptyCols are silently ignored when pty is false so agents
      // that defensively pass them on normal bash calls don't get stuck in
      // a retry loop. pty: true silently implies background: true (Rust
      // bash.rs handles the auto-promote); we mirror that here so the
      // Pi-side spawn payload also reflects the auto-promotion. When background
      // is disabled these params are omitted from the schema and defensively
      // ignored if a stale caller sends them anyway.
      const timeout = coerceOptionalInt(params.timeout, "timeout", 1, Number.MAX_SAFE_INTEGER);
      const ptyRows = backgroundDisabled
        ? undefined
        : coerceOptionalInt(params.ptyRows, "ptyRows", 1, 60);
      const ptyCols = backgroundDisabled
        ? undefined
        : coerceOptionalInt(params.ptyCols, "ptyCols", 1, 140);
      const compressed = coerceBoolean(params.compressed, true);
      // Coerce at the boundary: stringified pty/background flags (coerceBoolean).
      const requestedPty = !backgroundDisabled && coerceBoolean(params.pty);
      const effectiveBackground =
        !backgroundDisabled && (coerceBoolean(params.background) || requestedPty);
      const blockToCompletion = backgroundDisabled;
      // Hard-kill timeout sent to the bridge. For an EXPLICIT background task a
      // small `timeout` is a legitimate kill cap, so honor it verbatim. For the
      // FOREGROUND auto-promote path a `timeout` below the foreground wait
      // window is incoherent (the task would be killed before we promote it to
      // background), so treat it as unset and let the bridge apply its
      // 30-minute default — this is the #102 fix. When background is disabled
      // there is no promotion window, so `timeout` remains the hard cap.
      const effectiveTimeout =
        effectiveBackground || backgroundDisabled
          ? timeout
          : resolveBashKillTimeout(timeout, foregroundWaitMs);

      // Build spawn context for potential hook modification
      let spawnContext: BashSpawnContext = {
        command: params.command,
        cwd: params.workdir,
      };

      // Apply BashSpawnHook if available (Pi extension point)
      if (spawnHook) {
        try {
          spawnContext = await spawnHook(spawnContext);
        } catch (hookErr) {
          // Hook errors should not silently fail — surface them
          throw new Error(
            `BashSpawnHook failed: ${hookErr instanceof Error ? hookErr.message : String(hookErr)}`,
          );
        }
      }

      const bridgeCommand = spawnContext.command;

      let streamed = "";
      const response = await callBashBridge(
        bridge,
        "bash",
        {
          command: bridgeCommand,
          timeout: effectiveTimeout,
          workdir: spawnContext.cwd ?? params.workdir,
          env: spawnContext.env,
          description: params.description,
          background: effectiveBackground,
          notify_on_completion: effectiveBackground,
          compressed,
          pty: requestedPty,
          pty_rows: ptyRows,
          pty_cols: ptyCols,
          foreground_orchestrate: true,
          block_to_completion: blockToCompletion,
        },
        extCtx,
        {
          transportTimeoutMs: orchestratedTransportTimeoutMs(
            blockToCompletion,
            effectiveTimeout,
            foregroundWaitMs,
          ),
          onProgress: ({ text }) => {
            streamed += text;
            // Stream truncated output to avoid overwhelming the UI
            const displayText = truncateToVisualLines(streamed, 100);
            onUpdate?.(bashResult(displayText, { streaming: true }));
          },
        },
      ).catch((err) => {
        if (err instanceof Error && err.message.includes("permission_required")) {
          // Pi has no permission system — this should never reach us from Rust
          // (Track C scan returns empty for Pi). If it somehow did, throw clearly.
          throw new Error(
            "Permission ask reached Pi adapter — this is a bug. Pi has no permission system.",
          );
        }
        throw err;
      });

      if (response.success === false) {
        throw new Error((response.message as string | undefined) ?? "bash failed");
      }

      const taskId = response.task_id as string | undefined;
      if (response.status === "running" && taskId) {
        trackBgTask(resolveSessionId(extCtx), taskId);
        return bashResult((response.output as string | undefined) ?? "", { task_id: taskId });
      }

      const details: BashDetails = {
        exit_code: response.exit_code as number | undefined,
        duration_ms: response.duration_ms as number | undefined,
        truncated: response.truncated as boolean | undefined,
        output_path: response.output_path as string | undefined,
        task_id: taskId,
      };

      const output = (response.output as string | undefined) ?? "";
      return bashResult(
        withBashHints(output, bridgeCommand, aftSearchRegistered, extCtx.cwd),
        details,
      );
    },
    renderCall(args, theme, context) {
      return renderBashCall(args?.command, args?.description, theme, context);
    },
    renderResult(result, _options, theme, context) {
      return renderBashResult(result, theme, context);
    },
  });

  // Background control tools are part of the background surface. When
  // `bash.background` is disabled, `bash` still registers but runs foreground
  // commands to completion inline, so these tools are intentionally absent.
  if (bashCfg.background) {
    pi.registerTool<typeof BashStatusParams, BashStatusDetails>(createBashStatusTool(ctx));
    pi.registerTool<typeof BashWatchParams, BashWatchDetails>(createBashWatchTool(ctx));
    pi.registerTool<typeof BashWriteParams, BashWriteDetails>(createBashWriteTool(ctx));
    pi.registerTool<typeof BashTaskParams, BashKillDetails>(createBashKillTool(ctx));
  }
}

/**
 * Append AFT bash-output hints (conflicts / grep) to a foreground bash result.
 * Pi knows the exact command, so the grep hint is matched against it directly
 * rather than the echoed first output line. Mirrors OpenCode's
 * `tool.execute.after` nudges; only fires on terminal bash output (not
 * background-spawn/promotion messages, which have no real output yet).
 */
function withBashHints(
  output: string,
  command: string,
  aftSearchRegistered: boolean,
  projectRoot: string,
): string {
  return maybeAppendGrepSearchHint(
    maybeAppendConflictsHint(output),
    command,
    aftSearchRegistered,
    projectRoot,
  );
}

export function createBashStatusTool(ctx: PluginContext) {
  return {
    name: "bash_status",
    label: "bash_status",
    description:
      "Read-only snapshot of a background bash task. Returns immediately. Never waits. One look to check on a task is fine — never loop it to wait for completion. To wait, use bash_watch.",
    promptSnippet: "Inspect a background bash task by task_id",
    parameters: BashStatusParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashStatusParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashStatusDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      // bash_status is snapshot-only. wait_for / exit / timeout_ms moved to
      // bash_watch; if the agent passes them here they're silently ignored
      // at the TypeBox schema layer.
      const data = await bashStatusSnapshot(bridge, extCtx, params.task_id, params.output_mode);
      const details = data as unknown as BashStatusDetails;
      return bashStatusResult(
        await formatBashStatus(extCtx, params.task_id, details, params.output_mode),
        details,
      );
    },
  };
}

export function createBashWatchTool(ctx: PluginContext) {
  return {
    name: "bash_watch",
    label: "bash_watch",
    description:
      "Watch a background bash task. Sync (default) blocks until a pattern matches, the task exits, or timeout — use it when the result is the next thing you need, even for long builds/tests/installs (pass timeout_ms up to 30 min for those). The user can interrupt anytime; the wait auto-converts to an async notification. Async (background:true, requires pattern) registers a non-blocking notification and returns immediately — use when you have parallel work or want to end your turn. Never loop bash_status to wait.",
    promptSnippet: "Wait for or watch a background bash task",
    parameters: BashWatchParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashWatchParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashWatchDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const waitFor = parseWaitPattern(params.pattern);
      // Coerce at the boundary: stringified background must enable async mode (coerceBoolean).
      if (coerceBoolean(params.background)) {
        if (!waitFor) {
          throw new Error(
            "invalid_request: Use auto-reminder; bash_watch without pattern in async mode is redundant",
          );
        }
        const notifyParams: Record<string, unknown> = {
          task_id: params.task_id,
          once: coerceBoolean(params.once, true),
        };
        if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
        else notifyParams.pattern = waitFor.value;
        const sessionId = resolveSessionId(extCtx);
        markExplicitControl(sessionId, params.task_id, false);
        let registered: Record<string, unknown>;
        try {
          registered = await callBashBridge(bridge, "bash_notify", notifyParams, extCtx);
        } catch (err) {
          unmarkExplicitControl(sessionId, params.task_id);
          throw err;
        }
        if (registered.success === false) {
          unmarkExplicitControl(sessionId, params.task_id);
          const message = String(registered.message ?? "bash_notify failed");
          throw new Error(`${String(registered.code ?? "invalid_request")}: ${message}`);
        }
        const watchDetails = { registered: true, watchId: registered.watch_id } as BashWatchDetails;
        return textResult(
          `Watch registered: ${registered.watch_id} on task ${params.task_id}\nA notification will fire when the pattern matches or the task exits.`,
          watchDetails,
        );
      }
      const data = await waitForBashStatus(
        ctx,
        bridge,
        extCtx,
        params.task_id,
        undefined,
        waitFor,
        true,
        Math.min(
          coerceOptionalInt(params.timeout_ms, "timeout_ms", 1, MAX_BASH_STATUS_WAIT_TIMEOUT_MS) ??
            DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS,
          MAX_BASH_STATUS_WAIT_TIMEOUT_MS,
        ),
      );
      // User-message abort: the sync wait was interrupted because the user
      // sent a message. Auto-register the equivalent async watch so the
      // notification still arrives, and return text explaining the conversion.
      if (data.waited?.reason === "user_message") {
        const convertedText = await convertToAsyncWatchOnAbort(
          bridge,
          extCtx,
          params.task_id,
          waitFor,
          coerceBoolean(params.once, true),
        );
        return textResult(convertedText, { waited: data.waited } as BashWatchDetails);
      }
      const text = await formatBashStatus(
        extCtx,
        params.task_id,
        data as unknown as BashStatusDetails,
        undefined,
      );
      return textResult(text, data as BashWatchDetails);
    },
  };
}

/**
 * When a sync bash_watch wait is aborted because the user sent a message,
 * auto-register the equivalent async watch so the notification still arrives.
 * If the original sync watch had a pattern, register an async watch with the
 * same pattern. If it had no pattern (exit-only), the auto-reminder system
 * already handles exit notifications, so just return the conversion message.
 */
async function convertToAsyncWatchOnAbort(
  bridge: BinaryBridge,
  extCtx: ExtensionContext,
  taskId: string,
  waitFor: BashWaitPattern | undefined,
  once: boolean,
): Promise<string> {
  // No pattern: the auto-reminder system already handles exit notifications
  // for background tasks, so no explicit watch registration is needed.
  if (!waitFor) {
    return (
      `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
      `The task is still running in the background. A completion reminder will be ` +
      `delivered automatically when the task exits; don't poll bash_status.`
    );
  }
  // Register the equivalent async watch so the pattern/exit notification
  // still arrives. Reuse the same registration path as the explicit async mode.
  const notifyParams: Record<string, unknown> = {
    task_id: taskId,
    once,
  };
  if (waitFor.kind === "regex") notifyParams.regex = waitFor.source;
  else notifyParams.pattern = waitFor.value;
  const sessionId = resolveSessionId(extCtx);
  markExplicitControl(sessionId, taskId, false);
  try {
    const registered = await callBashBridge(bridge, "bash_notify", notifyParams, extCtx);
    if (registered.success === false) {
      unmarkExplicitControl(sessionId, taskId);
      return (
        `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
        `Auto-registering an async watch failed (${String(registered.message ?? "unknown error")}). ` +
        `The task is still running in the background. A completion reminder will be ` +
        `delivered automatically when the task exits.`
      );
    }
    return (
      `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
      `The wait has been converted to an async watch (${registered.watch_id}). ` +
      `A notification will fire when the pattern matches or the task exits.`
    );
  } catch (err) {
    unmarkExplicitControl(sessionId, taskId);
    return (
      `Sync watch for task ${taskId} was interrupted because you sent a message. ` +
      `Auto-registering an async watch failed (${err instanceof Error ? err.message : String(err)}). ` +
      `The task is still running in the background. A completion reminder will be ` +
      `delivered automatically when the task exits.`
    );
  }
}

export function createBashWriteTool(ctx: PluginContext) {
  return {
    name: "bash_write",
    label: "bash_write",
    description:
      'Write input bytes to a running PTY bash task. PTY-only; check bash_status reports mode: "pty" first. ' +
      'Input is either a string (verbatim bytes) or an array mixing strings and { key: "esc" | "enter" | "up" | "ctrl-c" | ... } objects ' +
      'for atomic text+key sequences such as [ "iHello", { key: "esc" }, ":wq", { key: "enter" } ]. ' +
      "Named keys cover enter/return/tab/space/backspace/esc/escape, arrows, home/end/page-up/page-down/delete/insert, f1..f12, and ctrl-a..ctrl-z. " +
      "Maximum 1 MiB per call (post-expansion).",
    promptSnippet: "Write keystrokes/input to a PTY bash task",
    parameters: BashWriteParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashWriteParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashWriteDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const data = await callBashBridge(
        bridge,
        "bash_write",
        { task_id: params.task_id, input: params.input },
        extCtx,
      );
      return textResult(
        JSON.stringify({ bytes_written: data.bytes_written }, null, 2),
        data as unknown as BashWriteDetails,
      );
    },
  };
}

export function createBashKillTool(ctx: PluginContext) {
  return {
    name: "bash_kill",
    label: "bash_kill",
    description:
      "Terminate a running background bash task spawned with bash({ background: true }).",
    promptSnippet: "Kill a background bash task by task_id",
    parameters: BashTaskParams,
    async execute(
      _toolCallId: string,
      params: Static<typeof BashTaskParams>,
      _signal: AbortSignal | undefined,
      _onUpdate: ((update: AgentToolResult<BashKillDetails>) => void) | undefined,
      extCtx: ExtensionContext,
    ) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const data = await callBashBridge(bridge, "bash_kill", { task_id: params.task_id }, extCtx);
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_kill failed");
      }
      const details = data as unknown as BashKillDetails & { kill_signaled?: boolean };
      if (details.kill_signaled === true) {
        return bashKillResult(`Task ${params.task_id}: kill_signaled`, details);
      }
      return bashKillResult(`Task ${params.task_id}: ${details.status}`, details);
    },
  };
}

function bashResult(
  output: string,
  details: Partial<BashDetails> & { streaming?: boolean },
): AgentToolResult<BashDetails> {
  return {
    content: [{ type: "text", text: output }],
    details: {
      exit_code: details.exit_code,
      duration_ms: details.duration_ms,
      truncated: details.truncated,
      output_path: details.output_path,
      task_id: details.task_id,
      bg_completions: details.bg_completions,
    } as BashDetails,
  };
}

function bashStatusResult(
  output: string,
  details: BashStatusDetails,
): AgentToolResult<BashStatusDetails> {
  return {
    content: [{ type: "text", text: output }],
    details,
  };
}

function bashKillResult(
  output: string,
  details: BashKillDetails,
): AgentToolResult<BashKillDetails> {
  return {
    content: [{ type: "text", text: output }],
    details,
  };
}

type BashWaitPattern = { kind: "substring"; value: string } | { kind: "regex"; source: string };
type OutputCursor = { output: number; stderr: number; combined: number };

async function bashStatusSnapshot(
  bridge: BinaryBridge,
  extCtx: ExtensionContext,
  taskId: string,
  outputMode: string | undefined,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  return await callBashBridge(
    bridge,
    "bash_status",
    { task_id: taskId, output_mode: outputMode },
    extCtx,
    options,
  );
}

async function waitForBashStatus(
  ctx: PluginContext,
  bridge: BinaryBridge,
  extCtx: ExtensionContext,
  taskId: string,
  outputMode: string | undefined,
  waitFor: BashWaitPattern | undefined,
  waitForExit: boolean,
  effectiveWaitMs: number,
): Promise<Record<string, unknown> & { waited: BashStatusWaited }> {
  const startedAt = Date.now();
  const deadline = startedAt + effectiveWaitMs;
  let spillCursor: OutputCursor = { output: 0, stderr: 0, combined: 0 };
  let scanText = "";
  let scanBaseOffset = 0;
  const bridgeOptions = {
    keepBridgeOnTimeout: true,
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
  };
  if (waitFor?.kind === "regex") {
    await validateWaitRegex(bridge, extCtx, waitFor);
  }

  // Pre-mark BEFORE first poll: ingestBgCompletions will suppress any push
  // frame that arrives while we're waiting, so no wake is ever scheduled for
  // this task. Mirrors the OpenCode fix; see bg-notifications.markTaskWaiting.
  const sessionId = resolveSessionId(extCtx);
  // Clear any stale abort flag from a previous turn so it doesn't insta-abort
  // this new wait.
  clearSyncWatchAbort(sessionId);
  if (waitForExit) markTaskWaiting(sessionId, taskId);
  let sawTerminal = false;
  let lastData: Record<string, unknown> | undefined;
  try {
    while (true) {
      let data: Record<string, unknown>;
      try {
        data = await bashStatusSnapshot(bridge, extCtx, taskId, outputMode, bridgeOptions);
      } catch (err) {
        // A single poll's transport timeout means the bridge is *busy*, not
        // that the task failed — the bridge is kept warm (keepBridgeOnTimeout).
        // Don't abort the whole watch (which surfaces a red failure every
        // poll); honor abort/deadline and otherwise retry. A genuine
        // non-timeout error still propagates.
        if (!isBridgeTransportTimeout(err)) throw err;
        if (isSyncWatchAborted(sessionId)) {
          return withWaited(lastData ?? unavailableSnapshot(), {
            reason: "user_message",
            elapsed_ms: Date.now() - startedAt,
          });
        }
        if (Date.now() >= deadline) {
          return withWaited(lastData ?? unavailableSnapshot(), {
            reason: "unavailable",
            elapsed_ms: Date.now() - startedAt,
          });
        }
        await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
        continue;
      }
      lastData = data;
      const terminal = isTerminalStatus(data.status);

      if (waitFor) {
        const scan = await readNewTaskOutput(data, spillCursor);
        if (scan) {
          spillCursor = scan.nextCursor;
          if (scanText.length === 0) scanBaseOffset = scan.baseOffset;
          scanText += scan.text;
          if (waitFor.kind === "regex") {
            const trimmed = trimWaitScanBuffer(scanText, scanBaseOffset, waitFor);
            scanText = trimmed.text;
            scanBaseOffset = trimmed.baseOffset;
          }
          const match = await findWaitMatch(bridge, extCtx, scanText, waitFor);
          if (match) {
            if (waitForExit && terminal) {
              sawTerminal = true;
              consumeBgCompletion(sessionId, taskId);
              await markBgCompletionDelivered(
                { ctx, directory: extCtx.cwd, sessionID: sessionId },
                taskId,
              );
            }
            return withWaited(data, {
              reason: "matched",
              elapsed_ms: Date.now() - startedAt,
              match: match.text,
              match_offset: scanBaseOffset + match.byteOffset,
            });
          }
          if (waitFor.kind === "substring") {
            const trimmed = trimWaitScanBuffer(scanText, scanBaseOffset, waitFor);
            scanText = trimmed.text;
            scanBaseOffset = trimmed.baseOffset;
          }
        }
      }

      if (terminal) {
        if (waitForExit) {
          sawTerminal = true;
          consumeBgCompletion(sessionId, taskId);
          await markBgCompletionDelivered(
            { ctx, directory: extCtx.cwd, sessionID: sessionId },
            taskId,
          );
        }
        return withWaited(data, { reason: "exited", elapsed_ms: Date.now() - startedAt });
      }

      // User-message abort: if the user sent a message while we were
      // blocking, convert this sync wait to an async watch so the agent's
      // turn ends promptly. The match/exit checks above win over abort.
      if (isSyncWatchAborted(sessionId)) {
        return withWaited(data, { reason: "user_message", elapsed_ms: Date.now() - startedAt });
      }

      if (Date.now() >= deadline) {
        return withWaited(data, { reason: "timeout", elapsed_ms: Date.now() - startedAt });
      }
      await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
    }
  } finally {
    if (waitForExit && !sawTerminal) unmarkTaskWaiting(sessionId, taskId);
  }
}

async function readNewTaskOutput(
  data: Record<string, unknown>,
  cursor: OutputCursor,
): Promise<{ text: string; baseOffset: number; nextCursor: OutputCursor } | undefined> {
  const outputPath = data.output_path as string | undefined;
  const stderrPath = data.mode === "pty" ? undefined : (data.stderr_path as string | undefined);
  if (!outputPath && !stderrPath) return undefined;
  const stdoutBytes = outputPath
    ? await readFileBytesFrom(outputPath, cursor.output)
    : Buffer.alloc(0);
  const stderrBytes = stderrPath
    ? await readFileBytesFrom(stderrPath, cursor.stderr)
    : Buffer.alloc(0);
  const bytesRead = stdoutBytes.length + stderrBytes.length;
  if (bytesRead === 0) return undefined;
  return {
    text: Buffer.concat([stdoutBytes, stderrBytes]).toString("utf8"),
    baseOffset: cursor.combined,
    nextCursor: {
      output: cursor.output + stdoutBytes.length,
      stderr: cursor.stderr + stderrBytes.length,
      combined: cursor.combined + bytesRead,
    },
  };
}

async function readFileBytesFrom(outputPath: string, cursor: number): Promise<Buffer> {
  const handle = await fs.open(outputPath, "r");
  try {
    const chunks: Buffer[] = [];
    let offset = cursor;
    while (true) {
      const buffer = Buffer.allocUnsafe(64 * 1024);
      const { bytesRead } = await handle.read(buffer, 0, buffer.length, offset);
      if (bytesRead === 0) break;
      chunks.push(Buffer.from(buffer.subarray(0, bytesRead)));
      offset += bytesRead;
    }
    return Buffer.concat(chunks);
  } finally {
    await handle.close().catch(() => undefined);
  }
}

function parseWaitPattern(value: unknown): BashWaitPattern | undefined {
  if (typeof value === "string") return { kind: "substring", value };
  if (isRegexWaitObject(value)) return { kind: "regex", source: value.regex };
  return undefined;
}

export function __parseWaitPatternForTests(value: unknown): BashWaitPattern | undefined {
  return parseWaitPattern(value);
}

function isRegexWaitObject(value: unknown): value is { regex: string } {
  return (
    typeof value === "object" &&
    value !== null &&
    "regex" in value &&
    typeof (value as { regex?: unknown }).regex === "string"
  );
}

type WaitMatch = { text: string; byteOffset: number };

async function validateWaitRegex(
  bridge: BinaryBridge,
  extCtx: ExtensionContext,
  pattern: Extract<BashWaitPattern, { kind: "regex" }>,
): Promise<void> {
  await matchRegexWithBridge(bridge, extCtx, pattern.source, "");
}

async function findWaitMatch(
  bridge: BinaryBridge,
  extCtx: ExtensionContext,
  text: string,
  pattern: BashWaitPattern,
): Promise<WaitMatch | undefined> {
  if (pattern.kind === "substring") {
    const index = text.indexOf(pattern.value);
    return index >= 0
      ? { text: pattern.value, byteOffset: Buffer.byteLength(text.slice(0, index), "utf8") }
      : undefined;
  }
  return await matchRegexWithBridge(bridge, extCtx, pattern.source, text);
}

async function matchRegexWithBridge(
  bridge: BinaryBridge,
  extCtx: ExtensionContext,
  pattern: string,
  text: string,
): Promise<WaitMatch | undefined> {
  try {
    const result = await callBashBridge(bridge, "bash_regex_match", { pattern, text }, extCtx);
    if (result.matched !== true) return undefined;
    return {
      text: typeof result.match_text === "string" ? result.match_text : "",
      byteOffset: coerceMatchOffset(result.match_offset),
    };
  } catch (err) {
    if (err instanceof BridgeError && err.code === "invalid_regex") {
      throw new Error(`invalid_request: invalid_regex: ${err.message}`);
    }
    throw err;
  }
}

function coerceMatchOffset(value: unknown): number {
  const offset = typeof value === "number" ? value : Number(value ?? 0);
  return Number.isFinite(offset) && offset >= 0 ? offset : 0;
}

function trimWaitScanBuffer(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number } {
  const keepFrom =
    pattern.kind === "substring"
      ? substringKeepStart(text, pattern.value)
      : regexKeepStart(text, REGEX_WAIT_SCAN_WINDOW_BYTES);
  if (keepFrom <= 0) return { text, baseOffset };

  return {
    text: text.slice(keepFrom),
    baseOffset: baseOffset + Buffer.byteLength(text.slice(0, keepFrom), "utf8"),
  };
}

function substringKeepStart(text: string, pattern: string): number {
  const keepChars = Math.max(0, pattern.length - 1);
  return text.length > keepChars ? text.length - keepChars : 0;
}

function regexKeepStart(text: string, maxBytes: number): number {
  if (Buffer.byteLength(text, "utf8") <= maxBytes) return 0;

  let low = 0;
  let high = text.length;
  while (low < high) {
    const mid = Math.floor((low + high) / 2);
    if (Buffer.byteLength(text.slice(mid), "utf8") > maxBytes) {
      low = mid + 1;
    } else {
      high = mid;
    }
  }
  return low;
}

export function __trimWaitScanBufferForTests(
  text: string,
  baseOffset: number,
  pattern: BashWaitPattern,
): { text: string; baseOffset: number } {
  return trimWaitScanBuffer(text, baseOffset, pattern);
}

function withWaited(
  data: Record<string, unknown>,
  waited: BashStatusWaited,
): Record<string, unknown> & { waited: BashStatusWaited } {
  return { ...data, waited };
}

function formatWaitSummary(waited: BashStatusWaited, details: BashStatusDetails): string {
  if (waited.reason === "matched") {
    return `Waited ${waited.elapsed_ms}ms; matched ${JSON.stringify(waited.match ?? "")} at offset ${waited.match_offset ?? 0}.`;
  }
  if (waited.reason === "timeout") {
    return `Waited ${waited.elapsed_ms}ms; timeout reached without match.`;
  }
  if (waited.reason === "unavailable") {
    return `Waited ${waited.elapsed_ms}ms; the bridge stayed busy and status couldn't be read. The task may still be running — check with bash_status({ taskId }).`;
  }
  const exit = typeof details.exit_code === "number" ? `, exit ${details.exit_code}` : "";
  return `Waited ${waited.elapsed_ms}ms; task exited (${details.status}${exit}).`;
}

async function formatBashStatus(
  extCtx: ExtensionContext,
  taskId: string,
  details: BashStatusDetails,
  requestedOutputMode: string | undefined,
): Promise<string> {
  const exit = typeof details.exit_code === "number" ? ` (exit ${details.exit_code})` : "";
  const dur =
    typeof details.duration_ms === "number" ? ` ${Math.round(details.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${details.status}${exit}${dur}`;
  if (details.waited)
    text += `
${formatWaitSummary(details.waited, details)}`;
  if (details.mode === "pty") {
    // PTY output is rendered from the raw terminal spill file; never feed it
    // through the piped-output compression/line renderer.
    text += await formatPtyStatus(extCtx, taskId, details, requestedOutputMode);
  } else {
    if (isTerminalStatus(details.status) && details.output_preview) {
      text += `
${details.output_preview}`;
    }
    if (!isTerminalStatus(details.status)) {
      text += `
A completion reminder will be delivered automatically; don't poll.`;
    }
  }
  return text;
}

async function formatPtyStatus(
  _extCtx: ExtensionContext,
  taskId: string,
  details: BashStatusDetails,
  requestedOutputMode: string | undefined,
): Promise<string> {
  if (!details.output_path) return "\n[PTY output path unavailable]";
  const outputMode = requestedOutputMode ?? "screen";
  const raw =
    outputMode === "raw" || outputMode === "both"
      ? await fs.readFile(details.output_path)
      : undefined;
  let suffix = "";
  if (outputMode === "raw") {
    suffix =
      raw && raw.length > 0
        ? `
${raw.toString("utf8")}`
        : "";
  } else if (outputMode === "both") {
    suffix = `
${JSON.stringify({ screen: details.pty_screen ?? "", raw: raw?.toString("utf8") ?? "" }, null, 2)}`;
  } else {
    suffix = details.pty_screen
      ? `
${details.pty_screen}`
      : "";
  }
  if (!isTerminalStatus(details.status)) {
    suffix += `
PTY task is still running. Use bash_status({ task_id: "${taskId}", output_mode: "screen" }) to inspect, bash_write({ task_id: "${taskId}", input: "..." }) to send keystrokes.`;
  }
  return suffix;
}

function renderBashCall(
  command: string | undefined,
  description: string | undefined,
  theme: Theme,
  context: RenderContextLike,
): Text {
  const text = reuseText(context.lastComponent);
  const display = description ?? (command ? shortenCommand(command) : "...");
  text.setText(`${theme.fg("toolTitle", theme.bold("bash"))} ${theme.fg("accent", display)}`);
  return text;
}

function renderBashResult(
  result: AgentToolResult<BashDetails>,
  theme: Theme,
  context: RenderContextLike,
): import("@earendil-works/pi-tui").Component {
  // Errors: red text with error details
  if (context.isError) {
    const errorText = result.content
      .filter((c) => c.type === "text")
      .map((c) => (c as { text?: string }).text ?? "")
      .join("\n")
      .trim();
    const text = reuseText(context.lastComponent);
    text.setText(`\n${theme.fg("error", errorText || "bash failed")}`);
    return text;
  }

  const details = result.details;
  const exitCode = details?.exit_code;
  const bgCompletions = details?.bg_completions ?? [];

  // Build result display
  const container = reuseContainer(context.lastComponent);
  container.clear();
  container.addChild(new Spacer(1));

  // Output preview is already capped by Rust's coordinated bash-output policy.
  const rawOutput = result.content
    .filter((c) => c.type === "text")
    .map((c) => (c as { text?: string }).text ?? "")
    .join("\n")
    .trim();
  if (rawOutput) {
    container.addChild(new Text(rawOutput, 1, 0));
    container.addChild(new Spacer(1));
  }

  // Exit code indicator
  if (exitCode !== undefined) {
    const exitColor = exitCode === 0 ? "success" : "error";
    const exitText = theme.fg(exitColor, `exit ${exitCode}`);
    container.addChild(new Text(exitText, 1, 0));
  }

  // Background completions notification (from Track D metadata)
  if (bgCompletions.length > 0) {
    container.addChild(new Spacer(1));
    for (const bg of bgCompletions) {
      const cmdPreview = bg.command ? bg.command.slice(0, 60) : "unknown command";
      const suffix = (bg.command?.length ?? 0) > 60 ? "..." : "";
      const exitInfo = bg.exit_code !== undefined ? `exit ${bg.exit_code}` : bg.status;
      const statusColor = bg.status === "completed" && bg.exit_code === 0 ? "success" : "warning";
      const line = theme.fg(
        statusColor,
        `Background task ${bg.task_id} completed (${exitInfo}): ${cmdPreview}${suffix}`,
      );
      container.addChild(new Text(line, 1, 0));
    }
  }

  // Duration info (muted)
  if (details?.duration_ms !== undefined) {
    container.addChild(new Spacer(1));
    const durationText = theme.fg("muted", `${details.duration_ms}ms`);
    container.addChild(new Text(durationText, 1, 0));
  }

  // Truncation notice
  if (details?.truncated) {
    container.addChild(new Spacer(1));
    const truncText = theme.fg("warning", "(output truncated)");
    container.addChild(new Text(truncText, 1, 0));
  }

  return container;
}

function shortenCommand(command: string): string {
  // Truncate long commands for UI display
  if (command.length <= 60) return command;
  return `${command.slice(0, 57)}...`;
}
