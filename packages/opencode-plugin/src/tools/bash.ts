import * as fs from "node:fs/promises";
import {
  type BridgeRequestOptions,
  coerceBoolean,
  maybeAppendGrepSearchHint,
  resolveBashKillTimeout,
} from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { trackBgTask } from "../bg-notifications.js";
import { resolveBashConfig } from "../config.js";
import { sessionLog } from "../logger.js";
import { resolveIsSubagent } from "../shared/subagent-detect.js";
import type { PluginContext } from "../types.js";
import { callBashBridge, coerceOptionalInt, optionalInt, projectRootFor } from "./_shared.js";
import { runAsk } from "./permissions.js";

const z = tool.schema;
const METADATA_PREVIEW_LIMIT = 30 * 1024;
// Default hard timeout of 30 minutes when the caller omits a timeout. This
// sizes the bridge transport timeout for bash calls where the server blocks
// until the command completes or is killed.
const DEFAULT_HARD_TIMEOUT_MS = 30 * 60 * 1000;
// The margin gives Rust time to promote or finalize the task and deliver the
// final response after the server's foreground wait window or hard kill timeout.
const BASH_TRANSPORT_MARGIN_MS = 10_000;

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

/**
 * Agent-facing tool description, selected from the live configuration so it
 * never advertises behavior this project doesn't have:
 * - code-search prohibition steers to `aft_search` when registered, else the
 *   `grep` tool (same surface logic as the Rust grep-rewrite footer); the
 *   registration variant is selected late in index.ts once the final tool map
 *   is known.
 * - the compression sentence only appears when output compression is on —
 *   advertising `compressed: false` otherwise would describe a no-op.
 * - the background/PTY/watch sentences only appear when `bash.background` is on.
 *   With it off, the foreground tool surface runs commands to completion
 *   inline and treats `timeout` as the hard kill cap.
 *
 * Wording rules: this is read by AGENTS choosing a tool, not by users reading
 * docs. No internal vocabulary ("hoisted", "command rewriting", "unified bash
 * schema") — describe what the tool does and what NOT to use it for.
 */
export function bashToolDescription(
  aftSearchRegistered: boolean,
  compressionOn: boolean,
  backgroundOn: boolean,
): string {
  const searchSteer = aftSearchRegistered
    ? "use aft_search (concepts, identifiers, regex, literals), read, aft_outline, or aft_zoom instead"
    : "use the grep tool, read, aft_outline, or aft_zoom instead";
  const compression = compressionOn
    ? " Output is compressed by default; pass compressed: false for raw output. Piped commands run verbatim and show the pipeline's output; for AFT's test/build summary, run the runner without | head, | tail, or | grep."
    : "";
  const tasks = backgroundOn
    ? ' Commands run in the foreground and return inline; a long-running one auto-promotes to background and delivers a completion reminder when it finishes — so for the common "I am waiting on this result" case, just run it and wait, no flags needed. Use background: true yourself ONLY when you have other useful work to do while it runs; then bash_watch waits on the task (sync blocks until exit/pattern, async notifies) and bash_status peeks at it — never background a command and immediately bash_watch it (that wastes a turn for what foreground returns in one), and never loop bash_status to wait. pty: true runs interactive programs (REPLs, TUIs), implies background, and is driven with bash_status({ outputMode: "screen" }) plus bash_write.'
    : " Commands run in the foreground to completion; timeout is the hard kill cap (default 30 minutes).";
  return `Execute shell commands.${compression}${tasks}

DO NOT use bash for code search or code exploration. If you are about to run grep, rg, sed, awk, find, or cat through bash to locate or read code: STOP — ${searchSteer}.`;
}

interface PermissionAsk {
  kind: "external_directory" | "bash";
  patterns: string[];
  always: string[];
}

type BridgeCaller = typeof callBashBridge;

function pushUnique(target: string[], values: string[]): void {
  for (const value of values) {
    if (!target.includes(value)) target.push(value);
  }
}

function groupBashPermissionAsks(asks: PermissionAsk[]): PermissionAsk[] {
  const grouped: PermissionAsk[] = [];
  let bashAsk: PermissionAsk | undefined;

  for (const ask of asks) {
    if (ask.kind === "bash") {
      if (!bashAsk) {
        bashAsk = { kind: "bash", patterns: [], always: [] };
        grouped.push(bashAsk);
      }
      pushUnique(bashAsk.patterns, ask.patterns);
      pushUnique(bashAsk.always, ask.always);
      continue;
    }

    grouped.push(ask);
  }

  return grouped;
}

function permissionsGrantedForRetry(asks: PermissionAsk[]): string[] {
  return asks.flatMap((ask) => (ask.always.length > 0 ? ask.always : ask.patterns));
}

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
  for (const ask of groupBashPermissionAsks(asks)) {
    const permission = ask.kind === "external_directory" ? "external_directory" : "bash";
    await runAsk(
      runtime.ask({
        permission,
        patterns: ask.patterns,
        always: ask.always,
        metadata: {},
      }),
    );
  }

  const second = await bridgeCall(
    ctx,
    runtime,
    "bash",
    { ...params, permissions_granted: permissionsGrantedForRetry(asks) },
    options,
  );
  if (second.success === false && second.code === "permission_required") {
    throw new Error("bash permission retry failed");
  }
  return second;
}

export function createBashTool(
  ctx: PluginContext,
  aftSearchRegisteredOverride?: boolean,
): ToolDefinition {
  const initialBashCfg = resolveBashConfig(ctx.config);
  const backgroundFlagArg = initialBashCfg.background
    ? {
        background: z
          .boolean()
          .optional()
          .describe(
            "When true, spawn the command in the background and return a taskId for bash_status/bash_kill instead of waiting for completion. Defaults to false.",
          ),
      }
    : {};
  const ptyArgs = initialBashCfg.background
    ? {
        pty: z
          .boolean()
          .optional()
          .describe(
            'When true, spawn the command in a real PTY for interactive programs (python/node/bash REPLs, vim). Implies background: true automatically. Unavailable in subagent sessions. Inspect with bash_status({ taskId, outputMode: "screen" }) and drive interactively with bash_write — its input accepts either a string OR an array like [ "iHello", { key: "esc" }, ":wq", { key: "enter" } ] for atomic text+key sequences.',
          ),
        ptyRows: optionalInt(1, 60).describe(
          "PTY terminal height in rows — ignored when pty is false. Defaults to 24 when pty: true. Minimum 1, maximum 60.",
        ),
        ptyCols: optionalInt(1, 140).describe(
          "PTY terminal width in columns — ignored when pty is false. Defaults to 80 when pty: true. Minimum 1, maximum 140.",
        ),
      }
    : {};
  const args = {
    command: z
      .string()
      .describe("Shell command to execute. Supports pipes, redirection, and normal shell syntax."),
    timeout: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
      initialBashCfg.background
        ? "Hard kill cap in milliseconds (positive integer). When omitted, the task can run up to 30 minutes. Foreground bash returns inline if the command finishes within ~8s (configurable via bash.foreground_wait_window_ms); otherwise it's automatically promoted to background and a completion reminder is delivered when the task actually finishes."
        : "Hard kill cap in milliseconds (positive integer). When omitted, the foreground command can run up to 30 minutes and returns inline when it finishes.",
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
    ...backgroundFlagArg,
    compressed: z
      .boolean()
      .optional()
      .describe(
        "When true or omitted, return compressed output with noisy terminal control sequences reduced. Set to false for raw output.",
      ),
    ...ptyArgs,
  };

  return {
    description: bashToolDescription(false, initialBashCfg.compress, initialBashCfg.background),
    args: args as ToolDefinition["args"],
    execute: async (args, context) => {
      const bashCfg = resolveBashConfig(ctx.config);
      const ctxAftSearchRegistered =
        (ctx as { aftSearchRegistered?: boolean }).aftSearchRegistered === true;
      const aftSearchRegistered = aftSearchRegisteredOverride ?? ctxAftSearchRegistered;
      let accumulatedOutput = "";
      const description = args.description as string | undefined;
      const metadata = (context as { metadata?: (data: Record<string, unknown>) => void }).metadata;
      const rawCommand = args.command as string;
      const command = rawCommand;
      const cwd = (args.workdir as string | undefined) ?? context.directory;

      // Detect whether the calling session is a subagent (has a non-empty
      // parentID). AFT bash auto-promotes long foreground tasks to background
      // (default ~8s, configurable via bash.foreground_wait_window_ms), but a
      // subagent terminates after its single response and cannot survive
      // backgrounding: any task_id we returned would be unreachable because
      // the subagent has no chance to call bash_status. So for subagents we
      // silently treat `background: true` as `false` and ask the server to
      // keep the call inline until the command completes or reaches its
      // hard-kill timeout.
      const isSubagent = await resolveIsSubagent(ctx.client, context.sessionID, context.directory);
      const backgroundDisabled = !bashCfg.background;
      // Coerce at the boundary: stringified pty/background flags (coerceBoolean).
      const requestedPty = !backgroundDisabled && coerceBoolean(args.pty);
      // pty:true silently implies background:true (Rust bash.rs handles the
      // auto-promote). Agents don't need to set both flags. When background is
      // disabled, those args are omitted from the schema and defensively ignored.
      const requestedBackground =
        !backgroundDisabled && (coerceBoolean(args.background) || requestedPty);
      // ptyRows/ptyCols are silently ignored when pty is false so agents
      // that defensively pass them on normal bash calls don't get stuck in
      // a retry loop. pty: true silently implies background: true (Rust
      // bash.rs handles the auto-promote); no explicit check needed.
      if (requestedPty && isSubagent) {
        throw new Error(
          "PTY mode is not available in subagent sessions; subagents cannot drive interactive terminals.",
        );
      }
      const allowSubagentBg = bashCfg.subagent_background;
      const subagentForcedForeground = isSubagent && !allowSubagentBg;
      const blockToCompletion = subagentForcedForeground || backgroundDisabled;
      const effectiveBackground = blockToCompletion ? false : requestedBackground;

      // Hard-kill timeout sent to the bridge. For an EXPLICIT background task a
      // small `timeout` is a legitimate kill cap (kill after N ms), so honor it
      // verbatim. For the FOREGROUND auto-promote path a `timeout` below the
      // foreground wait window is incoherent (the task would be killed before we
      // promote it to background), so treat it as unset and let the bridge apply
      // its 30-minute default — this is the #102 fix. When background is
      // disabled there is no promotion window, so `timeout` remains the hard cap.
      const rawTimeout = coerceOptionalInt(args.timeout, "timeout", 1, Number.MAX_SAFE_INTEGER);
      const ptyRows = coerceOptionalInt(args.ptyRows, "ptyRows", 1, 60);
      const ptyCols = coerceOptionalInt(args.ptyCols, "ptyCols", 1, 140);
      const compressed = coerceBoolean(args.compressed, true);
      const foregroundWaitMs = resolveForegroundWaitMs(bashCfg.foreground_wait_window_ms);
      const effectiveTimeout =
        effectiveBackground || backgroundDisabled
          ? rawTimeout
          : resolveBashKillTimeout(rawTimeout, foregroundWaitMs);
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
          timeout: effectiveTimeout,
          workdir: args.workdir,
          env: shellEnv?.env ?? {},
          description,
          background: effectiveBackground,
          notify_on_completion: effectiveBackground,
          compressed,
          pty: requestedPty,
          pty_rows: ptyRows,
          pty_cols: ptyCols,
          permissions_requested: true,
          foreground_orchestrate: true,
          block_to_completion: blockToCompletion,
        },
        callBashBridge,
        {
          transportTimeoutMs: orchestratedTransportTimeoutMs(
            blockToCompletion,
            effectiveTimeout,
            foregroundWaitMs,
          ),
          onProgress: ({ text }) => {
            accumulatedOutput = preview(accumulatedOutput + text);
            metadata?.({ output: accumulatedOutput, description });
          },
        },
      );

      if (data.success === false) {
        throw new Error((data.message as string) || "bash failed");
      }

      const uiTitle = description ?? shortenCommand(command);
      if (data.status === "running" && typeof data.task_id === "string") {
        const taskId = data.task_id;
        trackBgTask(context.sessionID, taskId);
        let rendered = (data.output as string | undefined) ?? "";
        if (isSubagent && allowSubagentBg) rendered += subagentGuidance(taskId);
        const metadataPayload = { description, output: rendered, status: "running", taskId };
        metadata?.(metadataPayload);
        return { output: rendered, title: uiTitle, metadata: metadataPayload };
      }

      const rendered = maybeAppendGrepSearchHint(
        (data.output as string | undefined) ?? "",
        command,
        aftSearchRegistered,
        projectRootFor(context),
      );
      const metadataPayload = foregroundMetadata(description, data, rendered);
      metadata?.(metadataPayload);
      return {
        output: rendered,
        title: uiTitle,
        metadata: metadataPayload,
      };
    },
  };
}

export function createBashStatusTool(ctx: PluginContext): ToolDefinition {
  return {
    description:
      "Read-only snapshot of a background or PTY bash task's current state and output. Returns immediately. Never waits. One look to check on a task is fine — never loop it to wait for completion. To wait, use bash_watch.",
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
    },
    execute: async (args, context) => {
      const taskId = args.taskId as string;
      const outputMode = args.outputMode as string | undefined;
      // bash_status is snapshot-only as of bash_watch landing. waitFor/exit/
      // timeoutMs moved to bash_watch — if the agent passes them here, they're
      // silently ignored at the Zod schema layer (extra keys stripped).
      const data = await bashStatusSnapshot(ctx, context, taskId, outputMode);
      return await formatBashStatusText(context, taskId, data, outputMode);
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
      const data = await callBashBridge(ctx, context, "bash_kill", {
        task_id: args.taskId as string,
      });
      if (data.success === false) {
        throw new Error((data.message as string | undefined) ?? "bash_kill failed");
      }
      if (data.kill_signaled === true) {
        return `Task ${args.taskId}: kill_signaled`;
      }
      return `Task ${args.taskId}: ${String(data.status ?? "killed")}`;
    },
  };
}

async function bashStatusSnapshot(
  ctx: PluginContext,
  runtime: ToolContext,
  taskId: string,
  outputMode: string | undefined,
  options?: BridgeRequestOptions,
): Promise<Record<string, unknown>> {
  const data = await callBashBridge(
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

async function formatBashStatusText(
  runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  requestedOutputMode: string | undefined,
): Promise<string> {
  const status = data.status as string;
  const exit = typeof data.exit_code === "number" ? ` (exit ${data.exit_code})` : "";
  const dur =
    typeof data.duration_ms === "number" ? ` ${Math.round(data.duration_ms / 1000)}s` : "";
  let text = `Task ${taskId}: ${status}${exit}${dur}`;
  if (data.mode === "pty") {
    // PTY output is rendered from the raw terminal spill file; never feed it
    // through the piped-output compression/line renderer.
    text += await formatPtyStatus(runtime, taskId, data, requestedOutputMode);
  } else {
    const preview = data.output_preview as string | undefined;
    if (preview && status !== "running") {
      text += `\n${preview}`;
    }
    if (status === "running") {
      text += `\nA completion reminder will be delivered automatically; don't poll.`;
    }
  }
  return text;
}

async function formatPtyStatus(
  _runtime: ToolContext,
  taskId: string,
  data: Record<string, unknown>,
  requestedOutputMode: string | undefined,
): Promise<string> {
  const outputPath = data.output_path as string | undefined;
  if (!outputPath) return "\n[PTY output path unavailable]";
  const outputMode = requestedOutputMode ?? "screen";
  const raw =
    outputMode === "raw" || outputMode === "both" ? await fs.readFile(outputPath) : undefined;
  let suffix = "";
  if (outputMode === "raw") {
    suffix =
      raw && raw.length > 0
        ? `
${raw.toString("utf8")}`
        : "";
  } else if (outputMode === "both") {
    suffix = `
${JSON.stringify({ screen: String(data.pty_screen ?? ""), raw: raw?.toString("utf8") ?? "" }, null, 2)}`;
  } else {
    const screen = data.pty_screen as string | undefined;
    suffix = screen
      ? `
${screen}`
      : "";
  }
  if (data.status === "running") {
    suffix += `
PTY task is still running. Use bash_status({ taskId: "${taskId}", outputMode: "screen" }) to inspect, bash_write({ taskId: "${taskId}", input: "..." }) to send keystrokes.`;
  }
  return suffix;
}

function preview(output: string): string {
  return output.length <= METADATA_PREVIEW_LIMIT ? output : output.slice(-METADATA_PREVIEW_LIMIT);
}

function subagentGuidance(taskId: string): string {
  return `

NOTE (subagent session): Continue with other work if you have it. If you don't, call bash_watch({ taskId: "${taskId}", timeoutMs: 60000 }) to wait for completion before returning to the parent. Subagents don't survive turn-end and won't receive the completion reminder.`;
}

function foregroundMetadata(
  description: string | undefined,
  data: Record<string, unknown>,
  rendered: string,
): Record<string, unknown> {
  const outputPath = data.output_path as string | undefined;
  const truncated =
    typeof data.truncated === "boolean"
      ? data.truncated
      : (data.output_truncated as boolean | undefined);
  return {
    description,
    output: preview(rendered),
    exit: data.exit_code as number | undefined,
    truncated,
    ...(outputPath ? { outputPath } : {}),
  };
}

function getCallID(ctx: unknown): string | undefined {
  const c = ctx as { callID?: string; callId?: string; call_id?: string };
  return c.callID ?? c.callId ?? c.call_id;
}

function shortenCommand(command: string): string {
  const collapsed = command.replace(/\s+/g, " ").trim();
  return collapsed.length <= 80 ? collapsed : `${collapsed.slice(0, 77)}...`;
}
