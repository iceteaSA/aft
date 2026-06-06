import * as fs from "node:fs/promises";
import {
  type BinaryBridge,
  type BridgeRequestOptions,
  maybeAppendConflictsHint,
  maybeAppendGrepHint,
  maybeStripCompressorPipe,
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
import {
  disposePtyTerminal,
  getOrCreatePtyTerminal,
  readPtyBytes,
  renderScreen,
} from "../shared/pty-cache.js";
import type { PluginContext } from "../types.js";
import {
  bridgeFor,
  callBridge,
  coerceOptionalInt,
  optionalInt,
  resolveSessionId,
  textResult,
} from "./_shared.js";

// Foreground polling wait-window: how long the plugin blocks the agent before
// promoting the task to background and returning. INTENTIONALLY decoupled
// from the task's own kill cap (`params.timeout`). Council decision:
// .alfonso/athena/council-aft-bash-timeout-design-5f25c3ee503ab303/
// The value is resolved per-call from bash config (default 8000ms, floored at
// 5000ms) via resolveBashConfig().foreground_wait_window_ms.
const FOREGROUND_POLL_INTERVAL_MS = 100;
const BASH_WAIT_POLL_INTERVAL_MS = 100;
const DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS = 30_000;
const MAX_BASH_STATUS_WAIT_TIMEOUT_MS = 300_000;
// Bridge transport budget for `bash` calls. Rust returns `running` immediately
// and the plugin polls separately, so transport only needs to cover spawn +
// protocol round-trip; not a function of params.timeout. See council audit
// `.alfonso/athena/council-aft-bash-timeout-audit-057818e1583d3883/`.
const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

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

const BashParams = Type.Object({
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
  background: Type.Optional(
    Type.Boolean({
      description:
        "Spawn command in background and return immediately with a task_id. Use bash_status to poll completion and bash_kill to terminate. Ideal for long-running tasks like builds or dev servers.",
    }),
  ),
  compressed: Type.Optional(
    Type.Boolean({
      description:
        "Compress output by removing ANSI codes, carriage returns, and excessive blank lines. Default: true. Set to false for raw terminal output including color codes.",
    }),
  ),
  pty: Type.Optional(
    Type.Boolean({
      description:
        'Spawn the command in a real PTY for interactive programs. Implies background: true automatically. Inspect with bash_status({ task_id, output_mode: "screen" }) and send input with bash_write.',
    }),
  ),
  ptyRows: optionalInt(1, 60),
  ptyCols: optionalInt(1, 140),
});

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
  reason: "matched" | "exited" | "timeout";
  elapsed_ms: number;
  match?: string;
  match_offset?: number;
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

export function registerBashTool(pi: ExtensionAPI, ctx: PluginContext): void {
  const spawnHook = getBashSpawnHook(pi);
  // aft_search is registered (and thus the right redirect target) when semantic
  // search is on AND the surface isn't minimal AND it isn't disabled — mirror
  // the registration gate in index.ts (surface.semantic). When unavailable the
  // grep-redirect hint points at the grep tool instead.
  const aftSearchAvailable =
    (ctx.config.tool_surface ?? "recommended") !== "minimal" &&
    ctx.config.semantic_search === true &&
    !(ctx.config.disabled_tools ?? []).includes("aft_search");

  pi.registerTool<typeof BashParams, BashDetails>({
    name: "bash",
    label: "bash",
    description:
      'Execute shell commands through AFT\'s Rust bash handler. By default, output is compressed. Pass `compressed: false` for raw output. Pass `background: true` to spawn in the background and get a task_id for `bash_status`/`bash_kill`. Pass `pty: true` with `background: true` for interactive programs and drive them with `bash_status({ output_mode: "screen" })` plus `bash_write`.',
    promptSnippet:
      "Run shell commands (timeout in milliseconds; supports workdir, background tasks, compressed output, PTY mode)",
    promptGuidelines: [
      "Use bash only when a dedicated AFT tool is not a better fit.",
      "Set compressed: false when you need ANSI color codes in the output.",
    ],
    parameters: BashParams,
    async execute(_toolCallId, params: Static<typeof BashParams>, _signal, onUpdate, extCtx) {
      const bridge = bridgeFor(ctx, extCtx.cwd);
      const bashCfg = resolveBashConfig(ctx.config);
      const foregroundWaitMs = bashCfg.foreground_wait_window_ms;
      // ptyRows/ptyCols are silently ignored when pty is false so agents
      // that defensively pass them on normal bash calls don't get stuck in
      // a retry loop. pty: true silently implies background: true (Rust
      // bash.rs handles the auto-promote); we mirror that here so the
      // Pi-side spawn payload also reflects the auto-promotion.
      const timeout = coerceOptionalInt(params.timeout, "timeout", 1, Number.MAX_SAFE_INTEGER);
      const ptyRows = coerceOptionalInt(params.ptyRows, "ptyRows", 1, 60);
      const ptyCols = coerceOptionalInt(params.ptyCols, "ptyCols", 1, 140);
      const effectiveBackground = params.background === true || params.pty === true;

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

      const compressionEnabled = bashCfg.compress && params.compressed !== false;
      const pipeStrip = maybeStripCompressorPipe(spawnContext.command, compressionEnabled);
      const bridgeCommand = pipeStrip.command;

      let streamed = "";
      const response = await callBashBridge(
        bridge,
        "bash",
        {
          command: bridgeCommand,
          timeout,
          workdir: spawnContext.cwd ?? params.workdir,
          env: spawnContext.env,
          description: params.description,
          background: effectiveBackground,
          notify_on_completion: effectiveBackground,
          compressed: params.compressed,
          pty: params.pty,
          pty_rows: ptyRows,
          pty_cols: ptyCols,
        },
        extCtx,
        {
          // Rust bash has its own watchdog that kills the child shell on the
          // bash-level timeout and returns a normal timed_out response well
          // before our transport timeout fires. If we hit the transport
          // deadline anyway it means the response is just late — don't
          // sacrifice the bridge (and all its warm state) for that.
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
        if (effectiveBackground) {
          trackBgTask(resolveSessionId(extCtx), taskId);
          return bashResult(formatBackgroundLaunch(taskId, params.pty === true), {
            task_id: taskId,
          });
        }

        // Wait-window decoupled from params.timeout. Always cap polling at
        // foregroundWaitMs so agents get a fast promotion message
        // for unexpectedly long commands. Honor a shorter explicit timeout
        // when present — polling beyond the task's kill cap is pointless.
        // Schema validation guarantees params.timeout is a positive integer
        // or undefined, so this Math.min is always well-defined.
        const waitTimeoutMs =
          timeout !== undefined ? Math.min(timeout, foregroundWaitMs) : foregroundWaitMs;
        const startedAt = Date.now();
        while (true) {
          const status = await callBashBridge(bridge, "bash_status", { task_id: taskId }, extCtx);
          if (status.success === false) {
            throw new Error((status.message as string | undefined) ?? "bash_status failed");
          }
          if (isTerminalStatus(status.status)) {
            return bashResult(
              appendPipeStripNote(
                withBashHints(formatForegroundResult(status), bridgeCommand, aftSearchAvailable),
                pipeStrip.note,
              ),
              {
                exit_code: status.exit_code as number | undefined,
                duration_ms: status.duration_ms as number | undefined,
                truncated: status.output_truncated as boolean | undefined,
                output_path: status.output_path as string | undefined,
                task_id: taskId,
              },
            );
          }
          if (Date.now() - startedAt >= waitTimeoutMs) {
            const promoted = await callBashBridge(
              bridge,
              "bash_promote",
              { task_id: taskId },
              extCtx,
            );
            if (promoted.success === false) {
              throw new Error((promoted.message as string | undefined) ?? "bash_promote failed");
            }
            trackBgTask(resolveSessionId(extCtx), taskId);
            return bashResult(formatPromotionMessage(taskId, params.timeout, foregroundWaitMs), {
              task_id: taskId,
            });
          }
          await sleep(FOREGROUND_POLL_INTERVAL_MS);
        }
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
        appendPipeStripNote(
          withBashHints(output, bridgeCommand, aftSearchAvailable),
          pipeStrip.note,
        ),
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

  // bash_status and bash_kill ride alongside `bash` regardless of which
  // experimental flag enabled it: foreground bash auto-promotes long-running
  // tasks to background after a short wait-window, so the agent always needs
  // a way to inspect or kill promoted tasks. The `experimental.bash.background`
  // flag only gates explicit `bash({ background: true })` spawning, not the
  // promotion path.
  pi.registerTool<typeof BashStatusParams, BashStatusDetails>(createBashStatusTool(ctx));
  pi.registerTool<typeof BashWatchParams, BashWatchDetails>(createBashWatchTool(ctx));
  pi.registerTool<typeof BashWriteParams, BashWriteDetails>(createBashWriteTool(ctx));
  pi.registerTool<typeof BashTaskParams, BashKillDetails>(createBashKillTool(ctx));
}

function appendPipeStripNote(output: string, note: string | undefined): string {
  return note ? `${output}\n\n${note}` : output;
}

function formatBackgroundLaunch(taskId: string, isPty: boolean): string {
  if (isPty) {
    // PTY tasks are inherently interactive — the agent MUST poll bash_status
    // to see the screen and bash_write to drive the program. The piped-task
    // "don't poll" copy is wrong for this mode.
    return `PTY task started: ${taskId}. Use bash_status({ task_id: "${taskId}", output_mode: "screen" }) to see the visible terminal, bash_write({ task_id: "${taskId}", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.`;
  }
  return `Background task started: ${taskId}. A completion reminder will be delivered automatically; don't poll bash_status.`;
}

function formatPromotionMessage(
  taskId: string,
  timeout: number | undefined,
  waitWindowMs: number,
): string {
  // Reports actual elapsed wait, not the user's full kill cap. The agent
  // already has the original command in its tool-call args; bash_status
  // returns it on demand if a downstream tool ever needs it.
  const waited = timeout !== undefined ? Math.min(timeout, waitWindowMs) : waitWindowMs;
  return `Foreground bash didn't finish within ${formatSeconds(waited)} and was promoted to background: ${taskId}. A completion reminder will be delivered automatically; use bash_status({ task_id: "${taskId}" }) to inspect output or bash_kill({ task_id: "${taskId}" }) to terminate.`;
}

/** Render a millisecond duration as a compact seconds string (8000 -> "8s", 5500 -> "5.5s"). */
function formatSeconds(ms: number): string {
  return `${Number((ms / 1000).toFixed(1))}s`;
}

/**
 * Append AFT bash-output hints (conflicts / grep) to a foreground bash result.
 * Pi knows the exact command, so the grep hint is matched against it directly
 * rather than the echoed first output line. Mirrors OpenCode's
 * `tool.execute.after` nudges; only fires on terminal bash output (not
 * background-spawn/promotion messages, which have no real output yet).
 */
function withBashHints(output: string, command: string, aftSearchAvailable: boolean): string {
  let result = maybeAppendConflictsHint(output);
  result = maybeAppendGrepHint(result, command, aftSearchAvailable);
  return result;
}

function formatForegroundResult(data: Record<string, unknown>): string {
  const output = (data.output_preview as string | undefined) ?? "";
  const outputPath = data.output_path as string | undefined;
  const truncated = data.output_truncated === true;
  const status = data.status as string | undefined;
  const exit = data.exit_code as number | undefined;
  let rendered = output;
  if (truncated && outputPath) {
    rendered += `\n[output truncated; full output at ${outputPath}]`;
  }
  if (status === "timed_out") {
    rendered += `\n[command timed out]`;
  }
  if (typeof exit === "number" && exit !== 0) {
    rendered += `\n[exit code: ${exit}]`;
  }
  return rendered;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function createBashStatusTool(ctx: PluginContext) {
  return {
    name: "bash_status",
    label: "bash_status",
    description:
      "Read-only snapshot of a background bash task. Returns immediately. Never waits. Use bash_watch to block on or register for pattern matches and exit events.",
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
      "Watch a background bash task. Two modes. Async (background:true, requires pattern) registers a non-blocking notification and returns immediately — use this to be pinged when a specific line appears or the task exits, without freezing your turn. Sync (default) blocks until a pattern matches/the task exits/timeout, and is ONLY for short bounded waits (seconds, e.g. a dev server printing a readiness line). Do NOT sync-wait for a long task (build/test/install): blocking locks the user out until it ends — instead end your turn and let the automatic completion reminder arrive, or use async mode.",
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
      if (params.background === true) {
        if (!waitFor) {
          throw new Error(
            "invalid_request: Use auto-reminder; bash_watch without pattern in async mode is redundant",
          );
        }
        const notifyParams: Record<string, unknown> = {
          task_id: params.task_id,
          once: params.once !== false,
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
      await disposePtyTerminal(ptyCacheKey(extCtx, params.task_id));
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

type BashWaitPattern =
  | { kind: "substring"; value: string }
  | { kind: "regex"; value: RegExp; source: string };
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

  // Pre-mark BEFORE first poll: ingestBgCompletions will suppress any push
  // frame that arrives while we're waiting, so no wake is ever scheduled for
  // this task. Mirrors the OpenCode fix; see bg-notifications.markTaskWaiting.
  const sessionId = resolveSessionId(extCtx);
  if (waitForExit) markTaskWaiting(sessionId, taskId);
  let sawTerminal = false;
  try {
    while (true) {
      const data = await bashStatusSnapshot(bridge, extCtx, taskId, outputMode, bridgeOptions);
      const terminal = isTerminalStatus(data.status);

      if (waitFor) {
        const scan = await readNewTaskOutput(extCtx, taskId, data, spillCursor);
        if (scan) {
          spillCursor = scan.nextCursor;
          if (scanText.length === 0) scanBaseOffset = scan.baseOffset;
          scanText += scan.text;
          const match = findWaitMatch(scanText, waitFor);
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
              match_offset:
                scanBaseOffset + Buffer.byteLength(scanText.slice(0, match.index), "utf8"),
            });
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

      if (Date.now() >= deadline) {
        return withWaited(data, { reason: "timeout", elapsed_ms: Date.now() - startedAt });
      }
      await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
    }
  } finally {
    if (waitForExit && !sawTerminal) unmarkTaskWaiting(sessionId, taskId);
    if (waitFor) {
      await disposePtyTerminal(watchPtyCacheKey(extCtx, taskId));
    }
  }
}

async function readNewTaskOutput(
  extCtx: ExtensionContext,
  taskId: string,
  data: Record<string, unknown>,
  cursor: OutputCursor,
): Promise<{ text: string; baseOffset: number; nextCursor: OutputCursor } | undefined> {
  const outputPath = data.output_path as string | undefined;
  if (data.mode === "pty") {
    if (!outputPath) return undefined;
    const { rows, cols } = ptyDimensions(data);
    const state = await getOrCreatePtyTerminal(
      watchPtyCacheKey(extCtx, taskId),
      outputPath,
      rows,
      cols,
    );
    const baseOffset = state.offset;
    const bytes = await readPtyBytes(state);
    if (bytes.length === 0) return undefined;
    return {
      text: bytes.toString("utf8"),
      baseOffset,
      nextCursor: { output: state.offset, stderr: 0, combined: state.offset },
    };
  }

  const stderrPath = data.stderr_path as string | undefined;
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
  if (isRegexWaitObject(value))
    return { kind: "regex", value: new RegExp(value.regex), source: value.regex };
  return undefined;
}

function isRegexWaitObject(value: unknown): value is { regex: string } {
  return (
    typeof value === "object" &&
    value !== null &&
    "regex" in value &&
    typeof (value as { regex?: unknown }).regex === "string"
  );
}

function findWaitMatch(
  text: string,
  pattern: BashWaitPattern,
): { text: string; index: number } | undefined {
  if (pattern.kind === "substring") {
    const index = text.indexOf(pattern.value);
    return index >= 0 ? { text: pattern.value, index } : undefined;
  }
  pattern.value.lastIndex = 0;
  const match = pattern.value.exec(text);
  return match ? { text: match[0], index: match.index } : undefined;
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
  extCtx: ExtensionContext,
  taskId: string,
  details: BashStatusDetails,
  requestedOutputMode: string | undefined,
): Promise<string> {
  if (!details.output_path) return "\n[PTY output path unavailable]";
  const key = ptyCacheKey(extCtx, taskId);
  const { rows, cols } = ptyDimensions(details);
  const state = await getOrCreatePtyTerminal(key, details.output_path, rows, cols);
  const raw = await readPtyBytes(state);
  const outputMode = requestedOutputMode ?? "screen";
  let suffix = "";
  if (outputMode === "raw") {
    suffix =
      raw.length > 0
        ? `
${raw.toString("utf8")}`
        : "";
  } else if (outputMode === "both") {
    suffix = `
${JSON.stringify({ screen: renderScreen(state, rows, cols), raw: raw.toString("utf8") }, null, 2)}`;
  } else {
    const screen = renderScreen(state, rows, cols);
    suffix = screen
      ? `
${screen}`
      : "";
  }
  if (!isTerminalStatus(details.status)) {
    suffix += `\nPTY task is still running. Use bash_status({ task_id: "${taskId}", output_mode: "screen" }) to inspect, bash_write({ task_id: "${taskId}", input: "..." }) to send keystrokes.`;
  } else {
    await disposePtyTerminal(key);
  }
  return suffix;
}

function ptyDimensions(data: { pty_rows?: unknown; pty_cols?: unknown }): {
  rows: number;
  cols: number;
} {
  const rows = typeof data.pty_rows === "number" ? data.pty_rows : 24;
  const cols = typeof data.pty_cols === "number" ? data.pty_cols : 80;
  return { rows, cols };
}

function ptyCacheKey(extCtx: ExtensionContext, taskId: string): string {
  return `${extCtx.cwd}::${resolveSessionId(extCtx) ?? "__default__"}::${taskId}`;
}
function watchPtyCacheKey(extCtx: ExtensionContext, taskId: string): string {
  return `${ptyCacheKey(extCtx, taskId)}::watch`;
}

function isTerminalStatus(status: unknown): boolean {
  // Explicit allowlist (parity with opencode-plugin) so an unexpected status
  // string from Rust doesn't accidentally end the foreground polling loop.
  return (
    status === "completed" || status === "failed" || status === "killed" || status === "timed_out"
  );
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
