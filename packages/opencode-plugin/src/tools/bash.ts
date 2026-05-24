import * as fs from "node:fs/promises";
import type { BridgeRequestOptions } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import {
  consumeBgCompletion,
  markTaskWaiting,
  trackBgTask,
  unmarkTaskWaiting,
} from "../bg-notifications.js";
import { resolveBashConfig } from "../config.js";
import { sessionLog } from "../logger.js";
import { storeToolMetadata } from "../metadata-store.js";
import {
  disposePtyTerminal,
  getOrCreatePtyTerminal,
  readPtyBytes,
  renderScreen,
} from "../shared/pty-cache.js";
import { resolveIsSubagent } from "../shared/subagent-detect.js";
import type { PluginContext } from "../types.js";
import { callBridge, projectRootFor } from "./_shared.js";
import { runAsk } from "./permissions.js";

const z = tool.schema;
const METADATA_PREVIEW_LIMIT = 30 * 1024;
// Foreground polling wait-window: how long the plugin blocks the agent before
// promoting the task to background and returning. INTENTIONALLY decoupled
// from the task's own kill cap (`args.timeout`). Council decision:
// .alfonso/athena/council-aft-bash-timeout-design-5f25c3ee503ab303/
const FOREGROUND_WAIT_WINDOW_MS = 5_000;
const FOREGROUND_POLL_INTERVAL_MS = 100;
const BASH_WAIT_POLL_INTERVAL_MS = 100;
const DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS = 30_000;
const MAX_BASH_STATUS_WAIT_TIMEOUT_MS = 300_000;
// Bridge transport timeout for `bash` calls. The Rust handler returns a
// `running` status immediately and the plugin polls separately, so transport
// only needs to cover spawn + protocol round-trip. 30s is conservative for
// Rust-side spawn (project_root resolution, bash_background registry write,
// LSP integration overhead). NOT a function of args.timeout — explicit short
// timeouts kill the task in Rust, not via transport. See council audit
// `.alfonso/athena/council-aft-bash-timeout-audit-057818e1583d3883/`.
const BASH_TRANSPORT_TIMEOUT_MS = 30_000;
// Default hard-kill cap when caller doesn't pass `args.timeout`. Mirrors the
// Rust-side `DEFAULT_BG_TIMEOUT` (30 minutes). Used as the subagent foreground
// poll-window when no explicit timeout was provided — subagents cannot survive
// background promotion, so we poll until the task is terminal or this cap fires.
const DEFAULT_HARD_TIMEOUT_MS = 30 * 60 * 1000;

const BASH_DESCRIPTION = `Hoisted bash tool with output compression, command rewriting to AFT tools, optional background execution, and PTY mode for interactive programs. By default, output is compressed; pass compressed: false for raw output. Pass background: true to spawn in the background and get a task_id for bash_status/bash_kill. Pass pty: true with background: true for interactive REPLs and drive them with bash_status({ outputMode: "screen" }) plus bash_write. bash_status also supports explicit waits with exit: true and/or waitFor.`;

interface PermissionAsk {
  kind: "external_directory" | "bash";
  patterns: string[];
  always: string[];
}

type BridgeCaller = typeof callBridge;

async function withPermissionLoop(
  ctx: PluginContext,
  runtime: ToolContext,
  params: Record<string, unknown>,
  bridgeCall: BridgeCaller,
  options?: BridgeRequestOptions,
): ReturnType<BridgeCaller> {
  const first = await bridgeCall(ctx, runtime, "bash", params, options);
  if (first.success !== false || first.code !== "permission_required") return first;

  const asks = Array.isArray(first.asks) ? (first.asks as PermissionAsk[]) : [];
  const permissionsGranted: string[] = [];
  for (const ask of asks) {
    const permission = ask.kind === "external_directory" ? "external_directory" : "bash";
    await runAsk(
      runtime.ask({
        permission,
        patterns: ask.patterns,
        always: ask.always,
        metadata: {},
      }),
    );
    permissionsGranted.push(...(ask.always.length > 0 ? ask.always : ask.patterns));
  }

  const second = await bridgeCall(
    ctx,
    runtime,
    "bash",
    { ...params, permissions_granted: permissionsGranted },
    options,
  );
  if (second.success === false && second.code === "permission_required") {
    throw new Error("bash permission retry failed");
  }
  return second;
}

export function createBashTool(ctx: PluginContext): ToolDefinition {
  return {
    description: BASH_DESCRIPTION,
    args: {
      command: z
        .string()
        .describe(
          "Shell command to execute through AFT's unified bash schema. Supports normal shell syntax, pipes, redirection, and command rewriting to dedicated AFT tools when available.",
        ),
      timeout: z
        .number()
        .int()
        .positive()
        .optional()
        .describe(
          "Hard kill cap in milliseconds (positive integer). When omitted, the task can run up to 30 minutes. Foreground bash returns inline if the command finishes within ~5s; otherwise it's automatically promoted to background and a completion reminder is delivered when the task actually finishes.",
        ),
      workdir: z
        .string()
        .optional()
        .describe(
          "Working directory for command execution. Relative paths resolve through the bridge; defaults to the current tool context/project root when omitted.",
        ),
      description: z
        .string()
        .optional()
        .describe(
          "Short 5-10 word human-readable summary shown in OpenCode UI metadata instead of raw shell syntax.",
        ),
      background: z
        .boolean()
        .optional()
        .describe(
          "When true, spawn the command in the background and return a task_id for bash_status/bash_kill instead of waiting for completion. Defaults to false.",
        ),
      compressed: z
        .boolean()
        .optional()
        .describe(
          "When true or omitted, return compressed output with noisy terminal control sequences reduced. Set to false for raw output.",
        ),
      pty: z
        .boolean()
        .optional()
        .describe(
          'When true, spawn the command in a real PTY for interactive programs (python/node/bash REPLs, vim). Requires background: true and is unavailable in subagent sessions. Inspect with bash_status({ taskId, outputMode: "screen" }) and drive interactively with bash_write — its input accepts either a string OR an array like [ "iHello", { key: "esc" }, ":wq", { key: "enter" } ] for atomic text+key sequences.',
        ),
    },
    execute: async (args, context) => {
      const bashCfg = resolveBashConfig(ctx.config);
      let accumulatedOutput = "";
      const description = args.description as string | undefined;
      const metadata = (context as { metadata?: (data: Record<string, unknown>) => void }).metadata;
      const command = args.command as string;
      const cwd = (args.workdir as string | undefined) ?? context.directory;

      // Detect whether the calling session is a subagent (has a non-empty
      // parentID). AFT bash auto-promotes anything >~5s to background, but a
      // subagent terminates after its single response and cannot survive
      // backgrounding: any task_id we returned would be unreachable because
      // the subagent has no chance to call bash_status. So for subagents we
      // silently treat `background: true` as `false` and extend the
      // foreground poll window to the task's full hard-kill timeout — the
      // command still runs to completion, just inline.
      const isSubagent = await resolveIsSubagent(ctx.client, context.sessionID, context.directory);
      const requestedBackground = args.background === true;
      const requestedPty = args.pty === true;
      if (requestedPty && !requestedBackground) {
        throw new Error("PTY mode requires background: true");
      }
      if (requestedPty && isSubagent) {
        throw new Error(
          "PTY mode is not available in subagent sessions; subagents cannot drive interactive terminals.",
        );
      }
      const allowSubagentBg = bashCfg.subagent_background;
      const subagentForcedForeground = isSubagent && !allowSubagentBg;
      const effectiveBackground = subagentForcedForeground ? false : requestedBackground;
      // Only log when the gate actually changes behavior (subagent path).
      // The common primary-session foreground case is the overwhelming
      // majority of calls and produces no useful log signal.
      if (subagentForcedForeground && requestedBackground) {
        sessionLog(
          context.sessionID,
          "[bash] subagent + background:true → converting to foreground (subagent would lose task_id)",
        );
      }
      const shellEnv = await ctx.plugin?.trigger?.(
        "shell.env",
        { cwd, sessionID: context.sessionID, callID: getCallID(context) },
        { env: {} },
      );

      const data = await withPermissionLoop(
        ctx,
        context,
        {
          command,
          timeout: args.timeout,
          workdir: args.workdir,
          env: shellEnv?.env ?? {},
          description,
          background: effectiveBackground,
          notify_on_completion: effectiveBackground,
          compressed: args.compressed,
          pty: requestedPty,
          permissions_requested: true,
        },
        callBridge,
        {
          transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
          // Rust bash has its own watchdog that kills the child shell on the
          // bash-level timeout (`args.timeout`) and returns a normal timed_out
          // response well before our transport timeout fires. If we hit the
          // transport deadline anyway it means the response is just late —
          // don't sacrifice the bridge (and all its warm state) for that.
          keepBridgeOnTimeout: true,
          onProgress: ({ text }) => {
            accumulatedOutput = preview(accumulatedOutput + text);
            metadata?.({ output: accumulatedOutput, description });
          },
        },
      );

      if (data.success === false) {
        throw new Error((data.message as string) || "bash failed");
      }

      if (data.status === "running" && typeof data.task_id === "string") {
        const callID = getCallID(context);
        const taskId = data.task_id;
        if (effectiveBackground) {
          trackBgTask(context.sessionID, taskId);
          let startedLine = formatBackgroundLaunch(taskId, requestedPty);
          if (isSubagent && allowSubagentBg) startedLine += subagentGuidance(taskId);
          const metadataPayload = { description, output: startedLine, status: "running", taskId };
          metadata?.(metadataPayload);
          if (callID) {
            storeToolMetadata(context.sessionID, callID, {
              title: description ?? shortenCommand(command),
              metadata: metadataPayload,
            });
          }
          return startedLine;
        }

        // Wait-window is decoupled from `args.timeout`. For primary sessions
        // we always cap the foreground polling window at
        // FOREGROUND_WAIT_WINDOW_MS so agents get a fast "promoted" response
        // for unexpectedly long commands. If the agent passed a shorter
        // explicit `timeout`, honor that — there's no point polling longer
        // than the task can possibly survive.
        //
        // For SUBAGENTS, we extend the poll window to the task's full
        // hard-kill cap (`args.timeout` if provided, else the 30-minute
        // default). Subagents cannot survive background promotion, so the
        // bash call must stay inline until the task reaches a terminal
        // status or its own hard-kill timer fires. The transport timeout
        // is unaffected because each `bash_status` poll is a separate
        // short bridge call.
        //
        // Schema validation guarantees `args.timeout` is a positive
        // integer or undefined, so these expressions are well-defined.
        const argTimeout = args.timeout as number | undefined;
        const waitTimeoutMs = subagentForcedForeground
          ? (argTimeout ?? DEFAULT_HARD_TIMEOUT_MS)
          : argTimeout !== undefined
            ? Math.min(argTimeout, FOREGROUND_WAIT_WINDOW_MS)
            : FOREGROUND_WAIT_WINDOW_MS;
        const startedAt = Date.now();
        while (true) {
          const status = await callBridge(ctx, context, "bash_status", { task_id: taskId });
          if (status.success === false) {
            throw new Error((status.message as string | undefined) ?? "bash_status failed");
          }
          if (isTerminalStatus(status.status)) {
            const rendered = formatForegroundResult(status);
            const metadataPayload = foregroundMetadata(description, status, rendered);
            metadata?.(metadataPayload);
            if (callID) {
              storeToolMetadata(context.sessionID, callID, {
                title: description ?? shortenCommand(command),
                metadata: metadataPayload,
              });
            }
            return rendered;
          }
          if (Date.now() - startedAt >= waitTimeoutMs) {
            const promoted = await callBridge(ctx, context, "bash_promote", { task_id: taskId });
            if (promoted.success === false) {
              throw new Error((promoted.message as string | undefined) ?? "bash_promote failed");
            }
            trackBgTask(context.sessionID, taskId);
            let message = formatPromotionMessage(taskId, args.timeout as number | undefined);
            if (isSubagent && allowSubagentBg) message += subagentGuidance(taskId);
            const metadataPayload = { description, output: message, status: "running", taskId };
            metadata?.(metadataPayload);
            if (callID) {
              storeToolMetadata(context.sessionID, callID, {
                title: description ?? shortenCommand(command),
                metadata: metadataPayload,
              });
            }
            return message;
          }
          await sleep(FOREGROUND_POLL_INTERVAL_MS);
        }
      }

      const output = (data.output as string | undefined) ?? "";
      const metadataOutput = preview(output);
      const exit = data.exit_code as number | undefined;
      const truncated = data.truncated as boolean | undefined;
      const outputPath = data.output_path as string | undefined;
      const timedOut = data.timed_out === true;
      const callID = getCallID(context);
      const metadataPayload = {
        description,
        output: metadataOutput,
        exit,
        truncated,
        ...(outputPath ? { outputPath } : {}),
      };

      metadata?.(metadataPayload);
      if (callID) {
        storeToolMetadata(context.sessionID, callID, {
          title: description ?? shortenCommand(command),
          metadata: metadataPayload,
        });
      }

      // Agent-visible output is the raw bash output (matches OpenCode's native
      // bash contract). Exit code, truncation, output path are UI metadata —
      // they go through metadata?.() above. We surface the bare minimum the
      // agent NEEDS to know directly in the text:
      //   - non-zero exit code (agent must be able to detect command failure)
      //   - timeout marker (separate signal beyond exit 124)
      //   - truncation pointer (so agent knows full output exists on disk)
      let rendered = output;
      if (truncated && outputPath) {
        rendered += `\n[output truncated; full output at ${outputPath}]`;
      }
      if (timedOut) {
        rendered += `\n[command timed out]`;
      }
      if (typeof exit === "number" && exit !== 0) {
        rendered += `\n[exit code: ${exit}]`;
      }
      return rendered;
    },
  };
}

export function createBashStatusTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      'Check the status and captured output of a background bash task spawned with bash({ background: true }). For PTY tasks, pass outputMode: "screen" (default) to render the visible terminal, "raw" for bytes since the previous read, or "both". Pass exit: true and/or waitFor to block until the task exits, the text/regex appears, or timeoutMs elapses.',
    args: {
      taskId: z
        .string()
        .describe("Background task ID returned by bash({ background: true }), e.g. bash-6b454047."),
      outputMode: z
        .enum(["screen", "raw", "both"])
        .optional()
        .describe(
          "PTY output rendering mode. Defaults to screen for PTY tasks and preserves existing behavior for piped tasks when omitted.",
        ),
      waitFor: z
        .union([z.string(), z.object({ regex: z.string() })])
        .optional()
        .describe(
          "Wait until this text pattern appears in the task's output. String form: substring match. Object form: regex match (JavaScript regex syntax). The call returns as soon as the pattern is found OR the task reaches terminal status OR timeoutMs elapses.",
        ),
      exit: z
        .boolean()
        .optional()
        .describe(
          "Wait until the task reaches a terminal status (completed/failed/killed/timed_out). Useful for explicitly awaiting a background bash task before returning. Can be combined with waitFor — the call returns on whichever condition fires first.",
        ),
      timeoutMs: z
        .number()
        .int()
        .positive()
        .optional()
        .describe(
          "Maximum wait time in milliseconds. Default: 30000 (30s). Hard-capped at 300000 (5min). Ignored when neither waitFor nor exit is specified.",
        ),
    },
    execute: async (args, context) => {
      const taskId = args.taskId as string;
      const outputMode = args.outputMode as string | undefined;
      const waitFor = parseWaitPattern(args.waitFor);
      const waitForExit = args.exit === true;
      const effectiveWaitMs = Math.min(
        (args.timeoutMs as number | undefined) ?? DEFAULT_BASH_STATUS_WAIT_TIMEOUT_MS,
        MAX_BASH_STATUS_WAIT_TIMEOUT_MS,
      );
      const shouldWait = waitFor !== undefined || waitForExit;
      const metadata = (context as { metadata?: (data: Record<string, unknown>) => void }).metadata;
      const data = shouldWait
        ? await waitForBashStatus(
            ctx,
            context,
            taskId,
            outputMode,
            waitFor,
            waitForExit,
            effectiveWaitMs,
          )
        : await bashStatusSnapshot(ctx, context, taskId, outputMode);
      const waited = data.waited as BashStatusWaited | undefined;
      if (waited) metadata?.({ taskId, status: data.status, waited });
      return await formatBashStatusText(context, taskId, data, outputMode, waited);
    },
  };
}

export function createBashKillTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      "Terminate a running background bash task spawned with bash({ background: true }). Returns confirmation of kill or an error if the task already finished.",
    args: {
      taskId: z
        .string()
        .describe("Background task ID returned by bash({ background: true }), e.g. bash-6b454047."),
    },
    execute: async (args, context) => {
      const data = await callBridge(ctx, context, "bash_kill", {
        task_id: args.taskId as string,
      });
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_kill failed");
      }
      await disposePtyTerminal(ptyCacheKey(context, args.taskId as string));
      if (data.kill_signaled === true) {
        return `Task ${args.taskId}: kill_signaled`;
      }
      return `Task ${args.taskId}: ${String(data.status ?? "killed")}`;
    },
  };
}

type BashWaitPattern = { kind: "substring"; value: string } | { kind: "regex"; value: RegExp };

type BashStatusWaited = {
  reason: "matched" | "exited" | "timeout";
  elapsed_ms: number;
  match?: string;
  match_offset?: number;
};

type BashStatusWithWait = Record<string, unknown> & { waited?: BashStatusWaited };

async function bashStatusSnapshot(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const data = await callBridge(
    ctx,
    runtime,
    "bash_status",
    { task_id: taskId, output_mode: outputMode },
    options,
  );
  if (data.success === false) {
    throw new Error((data.message as string | undefined) ?? "bash_status failed");
  }
  return data;
}

async function waitForBashStatus(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  waitFor: BashWaitPattern | undefined,
  waitForExit: boolean,
  effectiveWaitMs: number,
): Promise<BashStatusWithWait> {
  const startedAt = Date.now();
  const deadline = startedAt + effectiveWaitMs;
  let spillCursor = 0;
  let scanText = "";
  let scanBaseOffset = 0;
  const bridgeOptions: BridgeRequestOptions = {
    keepBridgeOnTimeout: true,
    transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS,
  };

  while (true) {
    const data = await bashStatusSnapshot(ctx, runtime, taskId, outputMode, bridgeOptions);
    if (waitForExit && isTerminalStatus(data.status)) {
      consumeBgCompletion(runtime.sessionID, taskId);
      return withWaited(data, {
        reason: "exited",
        elapsed_ms: Date.now() - startedAt,
      });
    }

    if (waitFor) {
      const scan = await readNewTaskOutput(runtime, taskId, data, spillCursor);
      if (scan) {
        spillCursor = scan.nextCursor;
        if (scanText.length === 0) scanBaseOffset = scan.baseOffset;
        scanText += scan.text;
        const match = findWaitMatch(scanText, waitFor);
        if (match) {
          return withWaited(data, {
            reason: "matched",
            elapsed_ms: Date.now() - startedAt,
            match: match.text,
            match_offset:
              scanBaseOffset + Buffer.byteLength(scanText.slice(0, match.index), "utf8"),
          });
        }
      }
      if (isTerminalStatus(data.status)) {
        return withWaited(data, {
          reason: "exited",
          elapsed_ms: Date.now() - startedAt,
        });
      }
    }

    if (Date.now() >= deadline) {
      return withWaited(data, {
        reason: "timeout",
        elapsed_ms: Date.now() - startedAt,
      });
    }
    await sleep(Math.min(BASH_WAIT_POLL_INTERVAL_MS, Math.max(0, deadline - Date.now())));
  }
}

async function readNewTaskOutput(
  runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  cursor: number,
): Promise<{ text: string; baseOffset: number; nextCursor: number } | undefined> {
  const outputPath = data.output_path as string | undefined;
  if (!outputPath) return undefined;
  if (data.mode === "pty") {
    const state = await getOrCreatePtyTerminal(ptyCacheKey(runtime, taskId), outputPath);
    const baseOffset = state.offset;
    const bytes = await readPtyBytes(state);
    return { text: bytes.toString("utf8"), baseOffset, nextCursor: state.offset };
  }

  const bytes = await readFileBytesFrom(outputPath, cursor);
  return { text: bytes.toString("utf8"), baseOffset: cursor, nextCursor: cursor + bytes.length };
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
  if (isRegexWaitObject(value)) return { kind: "regex", value: new RegExp(value.regex) };
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

function withWaited(data: Record<string, unknown>, waited: BashStatusWaited): BashStatusWithWait {
  return { ...data, waited };
}

async function formatBashStatusText(
  runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  requestedOutputMode: string | undefined,
  waited: BashStatusWaited | undefined,
): Promise<string> {
  const status = data.status as string;
  const exit = typeof data.exit_code === "number" ? ` (exit ${data.exit_code})` : "";
  const dur =
    typeof data.duration_ms === "number" ? ` ${Math.round(data.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${status}${exit}${dur}`;
  if (waited) text += `\n${formatWaitSummary(waited, data)}`;
  if (data.mode === "pty") {
    text += await formatPtyStatus(runtime, taskId, data, requestedOutputMode);
  } else {
    const preview = data.output_preview as string | undefined;
    if (preview && status !== "running") {
      text += `\n${preview.slice(0, 2000)}`;
    }
    if (status === "running") {
      text += `\nA completion reminder will be delivered automatically; don't poll.`;
    }
  }
  return text;
}

function formatWaitSummary(waited: BashStatusWaited, data: Record<string, unknown>): string {
  if (waited.reason === "matched") {
    return `Waited ${waited.elapsed_ms}ms; matched ${JSON.stringify(waited.match ?? "")} at offset ${waited.match_offset ?? 0}.`;
  }
  if (waited.reason === "timeout") {
    return `Waited ${waited.elapsed_ms}ms; timeout reached without match.`;
  }
  const status = String(data.status ?? "unknown");
  const exit = typeof data.exit_code === "number" ? `, exit ${data.exit_code}` : "";
  return `Waited ${waited.elapsed_ms}ms; task exited (${status}${exit}).`;
}

async function formatPtyStatus(
  runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  requestedOutputMode: string | undefined,
): Promise<string> {
  const outputPath = data.output_path as string | undefined;
  if (!outputPath) return "\n[PTY output path unavailable]";
  const key = ptyCacheKey(runtime, taskId);
  const state = await getOrCreatePtyTerminal(key, outputPath);
  const raw = await readPtyBytes(state);
  const outputMode = requestedOutputMode ?? "screen";
  let suffix = "";
  if (outputMode === "raw") {
    suffix = raw.length > 0 ? `\n${raw.toString("utf8")}` : "";
  } else if (outputMode === "both") {
    suffix = `\n${JSON.stringify({ screen: renderScreen(state, 24, 80), raw: raw.toString("utf8") }, null, 2)}`;
  } else {
    const screen = renderScreen(state, 24, 80);
    suffix = screen ? `\n${screen}` : "";
  }
  if (data.status === "running") {
    suffix += `\nPTY task is still running. Use bash_status({ taskId: "${taskId}", outputMode: "screen" }) to inspect, bash_write({ taskId: "${taskId}", input: "..." }) to send keystrokes.`;
  } else if (isTerminalStatus(data.status)) {
    await disposePtyTerminal(key);
  }
  return suffix;
}

function ptyCacheKey(runtime: ToolContext, taskId: string): string {
  return `${projectRootFor(runtime)}::${runtime.sessionID ?? "__default__"}::${taskId}`;
}

function preview(output: string): string {
  return output.length <= METADATA_PREVIEW_LIMIT ? output : output.slice(-METADATA_PREVIEW_LIMIT);
}

function isTerminalStatus(status: unknown): boolean {
  return (
    status === "completed" || status === "failed" || status === "killed" || status === "timed_out"
  );
}

function subagentGuidance(taskId: string): string {
  return `

NOTE (subagent session): Continue with other work if you have it. If you don't, call bash_status({ taskId: "${taskId}", exit: true, timeoutMs: 60000 }) to wait for completion before returning to the parent. Subagents don't survive turn-end and won't receive the completion reminder.`;
}

function formatBackgroundLaunch(taskId: string, isPty: boolean): string {
  if (isPty) {
    // PTY tasks are inherently interactive — the agent MUST poll bash_status
    // to see the screen and bash_write to drive the program. The piped-task
    // "don't poll" copy is wrong for this mode.
    return `PTY task started: ${taskId}. Use bash_status({ taskId: "${taskId}", outputMode: "screen" }) to see the visible terminal, bash_write({ taskId: "${taskId}", input: ... }) to send keystrokes. A completion reminder fires automatically when the task exits.`;
  }
  return `Background task started: ${taskId}. A completion reminder will be delivered automatically; don't poll bash_status.`;
}

function formatPromotionMessage(taskId: string, timeout: number | undefined): string {
  // We waited up to FOREGROUND_WAIT_WINDOW_MS, or shorter if the agent's
  // explicit timeout capped us first. Report the actual elapsed wait so the
  // message is accurate. We do NOT echo the original command back — the
  // agent already has it in its own tool-call args, and bash_status returns
  // it on demand.
  const waited =
    timeout !== undefined
      ? Math.min(timeout, FOREGROUND_WAIT_WINDOW_MS)
      : FOREGROUND_WAIT_WINDOW_MS;
  return `Foreground bash didn't finish within ${waited}ms and was promoted to background: ${taskId}. A completion reminder will be delivered automatically; use bash_status({ taskId: "${taskId}" }) to inspect output or bash_kill({ taskId: "${taskId}" }) to terminate.`;
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

function foregroundMetadata(
  description: string | undefined,
  data: Record<string, unknown>,
  rendered: string,
): Record<string, unknown> {
  const outputPath = data.output_path as string | undefined;
  return {
    description,
    output: preview(rendered),
    exit: data.exit_code as number | undefined,
    truncated: data.output_truncated as boolean | undefined,
    ...(outputPath ? { outputPath } : {}),
  };
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function getCallID(ctx: unknown): string | undefined {
  const c = ctx as { callID?: string; callId?: string; call_id?: string };
  return c.callID ?? c.callId ?? c.call_id;
}

function shortenCommand(command: string): string {
  const collapsed = command.replace(/\s+/g, " ").trim();
  return collapsed.length <= 80 ? collapsed : `${collapsed.slice(0, 77)}...`;
}
