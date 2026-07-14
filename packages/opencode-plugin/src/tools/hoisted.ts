/**
 * Hoisted tools that replace opencode's built-in tools (read, write, edit, apply_patch).
 *
 * When hoist_builtin_tools is enabled (default), these tools are registered with
 * the SAME names as opencode's built-in tools, effectively overriding them.
 * When disabled, they're registered with aft_ prefix (e.g., aft_read).
 *
 * All file operations go through AFT's Rust binary for better performance,
 * backup tracking, formatting, and inline diagnostics.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { coerceBoolean, coerceStringArray } from "@cortexkit/aft-bridge";
import type { ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { resolveBashConfig } from "../config.js";
import type { PluginContext } from "../types.js";
import {
  callBridge,
  callToolCall,
  coerceOptionalInt,
  optionalInt,
  resolvePathFromProjectRoot,
  resolveProjectRoot,
} from "./_shared.js";
import { createBashKillTool, createBashStatusTool, createBashTool } from "./bash.js";
import { createBashWatchTool } from "./bash_watch.js";
import { createBashWriteTool } from "./bash_write.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  runAsk,
} from "./permissions.js";

/** Get relative path matching opencode's format — the desktop UI parses it to extract filename + dir. */
function relativeToWorktree(fp: string, worktree: string): string {
  return path.relative(worktree, fp);
}

type ReadAttachment = {
  kind?: unknown;
  mime?: unknown;
  data?: unknown;
  bytes?: unknown;
  width?: unknown;
  height?: unknown;
  resized?: unknown;
};

function readAttachments(data: Record<string, unknown>): ReadAttachment[] {
  return Array.isArray(data.attachments) ? (data.attachments as ReadAttachment[]) : [];
}

/** Test-only export. Production code uses buildUnifiedDiff directly. */
export const _buildUnifiedDiffForTest = (fp: string, before: string, after: string): string =>
  buildUnifiedDiff(fp, before, after);

/**
 * Build a unified diff string from before/after content using a proper
 * LCS-based diff algorithm with grouped hunks and 3 lines of context.
 *
 * The previous implementation compared lines by index, so any insertion
 * or deletion that shifted line numbers caused every subsequent line to
 * compare unequal — emitting the entire rest of the file as "changed"
 * (issue #22, regression introduced in v0.15.3 when apply_patch started
 * sending diffs).
 *
 * Output matches GNU diff -u style: --- /+++ headers, @@ hunk markers,
 * one hunk per change cluster (consecutive changes within 6 lines of
 * each other are merged into a single hunk).
 */
function buildUnifiedDiff(fp: string, before: string, after: string): string {
  const beforeLines = before.split("\n");
  const afterLines = after.split("\n");

  // LCS is O(n*m) in lines; a 5000x5000 matrix uses ~100 MB and ~250 ms,
  // which we accept for normal source files. Above that we skip diff
  // generation rather than block the plugin event loop on a single edit.
  // Byte-size gating misses the real cost (a 100 KB minified bundle is one
  // line; a 30 KB markdown file with 1500 lines is the expensive case).
  const LINE_CAP = 5000;
  if (beforeLines.length > LINE_CAP || afterLines.length > LINE_CAP) {
    const limit = Math.max(beforeLines.length, afterLines.length);
    return `Index: ${fp}\n(diff skipped: file has ${limit} lines, above ${LINE_CAP}-line diff cap)\n`;
  }

  const ops = diffLines(beforeLines, afterLines);

  // No changes → empty diff (caller decides whether to render the header).
  if (ops.every((op) => op.tag === "eq")) {
    return `Index: ${fp}\n===================================================================\n--- ${fp}\n+++ ${fp}\n`;
  }

  const CONTEXT = 3;
  const HUNK_GAP = CONTEXT * 2; // merge hunks closer than this
  const hunks = groupIntoHunks(ops, CONTEXT, HUNK_GAP, beforeLines.length, afterLines.length);

  let diff = `Index: ${fp}\n===================================================================\n--- ${fp}\n+++ ${fp}\n`;
  for (const hunk of hunks) {
    diff += `@@ -${hunk.beforeStart},${hunk.beforeCount} +${hunk.afterStart},${hunk.afterCount} @@\n`;
    for (const line of hunk.lines) {
      diff += `${line}\n`;
    }
  }
  return diff;
}

type DiffOp =
  | { tag: "eq"; beforeIdx: number; afterIdx: number; line: string }
  | { tag: "del"; beforeIdx: number; line: string }
  | { tag: "ins"; afterIdx: number; line: string };

/**
 * LCS-based line diff. Builds a length table then walks back to produce ops.
 * O(n*m) time and space — fine for the 100KB SIZE_CAP guard above.
 */
function diffLines(a: readonly string[], b: readonly string[]): DiffOp[] {
  const n = a.length;
  const m = b.length;

  // dp[i][j] = LCS length of a[0..i] and b[0..j]
  // Use a flat Uint32Array for memory efficiency on large files.
  const dp = new Uint32Array((n + 1) * (m + 1));
  const w = m + 1;
  for (let i = 1; i <= n; i++) {
    for (let j = 1; j <= m; j++) {
      if (a[i - 1] === b[j - 1]) {
        dp[i * w + j] = dp[(i - 1) * w + (j - 1)] + 1;
      } else {
        const up = dp[(i - 1) * w + j];
        const left = dp[i * w + (j - 1)];
        dp[i * w + j] = up >= left ? up : left;
      }
    }
  }

  // Walk back to produce ops in reverse, then reverse at the end.
  const ops: DiffOp[] = [];
  let i = n;
  let j = m;
  while (i > 0 && j > 0) {
    if (a[i - 1] === b[j - 1]) {
      ops.push({ tag: "eq", beforeIdx: i - 1, afterIdx: j - 1, line: a[i - 1] });
      i--;
      j--;
    } else if (dp[(i - 1) * w + j] >= dp[i * w + (j - 1)]) {
      ops.push({ tag: "del", beforeIdx: i - 1, line: a[i - 1] });
      i--;
    } else {
      ops.push({ tag: "ins", afterIdx: j - 1, line: b[j - 1] });
      j--;
    }
  }
  while (i > 0) {
    ops.push({ tag: "del", beforeIdx: i - 1, line: a[i - 1] });
    i--;
  }
  while (j > 0) {
    ops.push({ tag: "ins", afterIdx: j - 1, line: b[j - 1] });
    j--;
  }
  ops.reverse();
  return ops;
}

interface Hunk {
  beforeStart: number; // 1-based
  beforeCount: number;
  afterStart: number; // 1-based
  afterCount: number;
  lines: string[]; // each prefixed with " ", "+", or "-"
}

/**
 * Group ops into hunks. Consecutive change ops are clustered with `context`
 * lines on each side; clusters closer than `gap` are merged into one hunk.
 */
function groupIntoHunks(
  ops: DiffOp[],
  context: number,
  gap: number,
  beforeLen: number,
  afterLen: number,
): Hunk[] {
  // Find indices of change ops (ins or del).
  const changeIdx: number[] = [];
  for (let k = 0; k < ops.length; k++) {
    if (ops[k].tag !== "eq") changeIdx.push(k);
  }
  if (changeIdx.length === 0) return [];

  // Build hunk ranges in op-index space, then merge nearby ones.
  const ranges: Array<[number, number]> = [];
  for (const idx of changeIdx) {
    const start = Math.max(0, idx - context);
    const end = Math.min(ops.length - 1, idx + context);
    if (ranges.length > 0 && start <= ranges[ranges.length - 1][1] + gap) {
      ranges[ranges.length - 1][1] = Math.max(ranges[ranges.length - 1][1], end);
    } else {
      ranges.push([start, end]);
    }
  }

  // Materialize each range as a hunk. Track 1-based line numbers from the
  // first op's recorded indices.
  const hunks: Hunk[] = [];
  for (const [start, end] of ranges) {
    let beforeStart = -1;
    let afterStart = -1;
    let beforeCount = 0;
    let afterCount = 0;
    const lines: string[] = [];
    for (let k = start; k <= end; k++) {
      const op = ops[k];
      if (op.tag === "eq") {
        if (beforeStart === -1) beforeStart = op.beforeIdx + 1;
        if (afterStart === -1) afterStart = op.afterIdx + 1;
        beforeCount++;
        afterCount++;
        lines.push(` ${op.line}`);
      } else if (op.tag === "del") {
        if (beforeStart === -1) beforeStart = op.beforeIdx + 1;
        if (afterStart === -1) {
          // Pure-deletion hunk at start: position after-cursor is one past
          // the last preceding equal op. Walk forward to find the next
          // ins/eq to anchor afterStart, otherwise clamp to end.
          afterStart = inferAfterStart(ops, k, afterLen);
        }
        beforeCount++;
        lines.push(`-${op.line}`);
      } else {
        if (afterStart === -1) afterStart = op.afterIdx + 1;
        if (beforeStart === -1) {
          beforeStart = inferBeforeStart(ops, k, beforeLen);
        }
        afterCount++;
        lines.push(`+${op.line}`);
      }
    }
    // Empty file edge case: GNU diff uses 0 for line numbers when count is 0.
    if (beforeCount === 0) beforeStart = 0;
    if (afterCount === 0) afterStart = 0;
    hunks.push({ beforeStart, beforeCount, afterStart, afterCount, lines });
  }
  return hunks;
}

/** Find what afterStart should be when a hunk begins with deletions. */
function inferAfterStart(ops: DiffOp[], from: number, afterLen: number): number {
  // Look forward for any op carrying an afterIdx.
  for (let k = from; k < ops.length; k++) {
    const op = ops[k];
    if (op.tag === "eq") return op.afterIdx + 1;
    if (op.tag === "ins") return op.afterIdx + 1;
  }
  // No future after-line — point past the last line.
  return afterLen;
}

/** Find what beforeStart should be when a hunk begins with insertions. */
function inferBeforeStart(ops: DiffOp[], from: number, beforeLen: number): number {
  for (let k = from; k < ops.length; k++) {
    const op = ops[k];
    if (op.tag === "eq") return op.beforeIdx + 1;
    if (op.tag === "del") return op.beforeIdx + 1;
  }
  return beforeLen;
}

const z = tool.schema;
// Diagnostics on edit are config-driven only (`lsp.diagnostics_on_edit`).
// There is deliberately NO per-call `diagnostics` param: agents never used it,
// and the agent-facing diagnostics paths are the status bar (passive,
// automatic E/W on tool results) and aft_inspect (active pull). The config
// knob remains for users whose models won't call aft_inspect.
function diagnosticsOnEditDefault(ctx: PluginContext): boolean {
  return ctx.config.lsp?.diagnostics_on_edit ?? false;
}

async function readCurrentFileForPreview(filePath: string): Promise<string> {
  try {
    return await fs.promises.readFile(filePath, "utf-8");
  } catch (error) {
    if (
      error &&
      typeof error === "object" &&
      "code" in error &&
      (error as { code?: string }).code === "ENOENT"
    ) {
      return "";
    }
    throw error;
  }
}

// ---------------------------------------------------------------------------
// Tool descriptions focus on behavior, modes, and return values.
// Parameter docs live in Zod .describe() and reach the LLM via JSON Schema.
// ---------------------------------------------------------------------------

const READ_DESCRIPTION = `Read file contents or list directory entries.

Use either startLine/endLine OR offset/limit to read a section of a file.

Behavior:
- Returns line-numbered content (e.g., "1: const x = 1")
- Lines longer than 2000 characters are truncated
- Output capped at 50KB
- Binary files are auto-detected and return a size-only message
- Supported images (PNG, JPEG, GIF, WebP) and PDFs are returned as tool attachments; range arguments are ignored for media
- Directories return sorted entries with trailing / for subdirectories

Examples:
  Read full file: { "filePath": "src/app.ts" }
  Read lines 50-100: { "filePath": "src/app.ts", "startLine": 50, "endLine": 100 }
  Read 30 lines from line 200: { "filePath": "src/app.ts", "offset": 200, "limit": 30 }
  List directory: { "filePath": "src/" }
`;

/**
 * Creates the simple read tool. Registers as "read" when hoisted, "aft_read" when not.
 */
export function createReadTool(ctx: PluginContext): ToolDefinition {
  return {
    description: READ_DESCRIPTION,
    args: {
      filePath: z
        .string()
        .describe("Path to file or directory (absolute or relative to project root)"),
      startLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
        "1-based line to start reading from",
      ),
      endLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
        "1-based line to stop reading at (inclusive)",
      ),
      limit: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
        "Max lines to return (default: 2000)",
      ),
      offset: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
        "1-based line number to start reading from (use with limit). Ignored if startLine is provided",
      ),
    },
    execute: async (args, context): Promise<ToolResult> => {
      const file = args.filePath as string;
      const projectRoot = await resolveProjectRoot(ctx, context);

      // Resolve relative paths from the same session/project root used by the bridge.
      const filePath = resolvePathFromProjectRoot(projectRoot, file);

      // Apply OpenCode's external-directory rule first. Under AFT's project
      // restriction, reads continue to Rust so its session task registry can
      // distinguish exact bash artifacts from ordinary external paths.
      {
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath, {
          serverValidatedRead: true,
        });
        if (denial) return permissionDeniedResponse(denial);
      }

      // Permission check
      try {
        await runAsk(
          context.ask({
            permission: "read",
            patterns: [filePath],
            always: ["*"],
            metadata: {},
          }),
        );
      } catch (error) {
        if (error instanceof Error && error.message) return permissionDeniedResponse(error.message);
        return permissionDeniedResponse("Permission denied.");
      }

      const rawStartLine = coerceOptionalInt(
        args.startLine,
        "startLine",
        1,
        Number.MAX_SAFE_INTEGER,
      );
      const rawEndLine = coerceOptionalInt(args.endLine, "endLine", 1, Number.MAX_SAFE_INTEGER);
      const rawLimit = coerceOptionalInt(args.limit, "limit", 1, Number.MAX_SAFE_INTEGER);
      const rawOffset = coerceOptionalInt(args.offset, "offset", 1, Number.MAX_SAFE_INTEGER);

      // Normalize offset/limit to startLine/endLine (backward compat with opencode's read)
      let startLine = rawStartLine;
      let endLine = rawEndLine;
      if (startLine === undefined && rawOffset !== undefined) {
        startLine = rawOffset;
        if (rawLimit !== undefined) {
          endLine = rawOffset + rawLimit - 1;
        }
      }

      const rawArgs: Record<string, unknown> = { filePath: file };
      if (startLine !== undefined) rawArgs.startLine = startLine;
      if (endLine !== undefined) rawArgs.endLine = endLine;
      // Only send limit if we did NOT convert offset to startLine/endLine.
      if (rawLimit !== undefined && rawOffset === undefined) rawArgs.limit = rawLimit;

      const response = await callToolCall(ctx, context, "read", rawArgs);

      // Error response (e.g. file not found)
      if (response.success === false) {
        throw new Error((response.message as string) || "read failed");
      }

      const dp = relativeToWorktree(filePath, projectRoot) || file;
      const output = response.text;

      const attachments = readAttachments(response);
      if (attachments.length > 0) {
        const toolAttachments = attachments
          .filter(
            (attachment) =>
              typeof attachment.mime === "string" && typeof attachment.data === "string",
          )
          .map((attachment) => ({
            type: "file" as const,
            mime: attachment.mime as string,
            url: `data:${attachment.mime};base64,${attachment.data}`,
          }));
        if (toolAttachments.length > 0) {
          const first = attachments[0];
          const firstMime = typeof first.mime === "string" ? first.mime : "";
          return {
            output,
            title: dp,
            attachments: toolAttachments,
            metadata: {
              preview: output,
              filepath: filePath,
              title: dp,
              isImage: first.kind === "image" || firstMime.startsWith("image/"),
              isPdf: first.kind === "pdf" || firstMime === "application/pdf",
            },
          };
        }
      }

      return { output, title: dp, metadata: { title: dp } };
    },
  };
}

// ---------------------------------------------------------------------------
// WRITE tool
// ---------------------------------------------------------------------------

function getWriteDescription(ctx: PluginContext, editToolName: string): string {
  const backupText =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config."
      : "Existing files are backed up before overwriting (undo via aft_safety).";
  return `Write content to a file, creating it and parent directories automatically. ${backupText} Auto-formats when the project has a formatter configured. Use it to create files or replace whole contents; for partial edits, use the \`${editToolName}\` tool.`;
}

function createWriteTool(ctx: PluginContext, editToolName = "edit"): ToolDefinition {
  return {
    description: getWriteDescription(ctx, editToolName),
    args: {
      filePath: z
        .string()
        .describe("Path to the file to write (absolute or relative to project root)"),
      content: z.string().describe("The full content to write to the file"),
    },
    execute: async (args, context): Promise<ToolResult> => {
      const file = args.filePath as string;
      const content = args.content as string;
      const projectRoot = await resolveProjectRoot(ctx, context);

      const filePath = resolvePathFromProjectRoot(projectRoot, file);

      const relPath = path.relative(projectRoot, filePath);

      // External-directory check first (mirrors opencode-native write.ts:43).
      {
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      const rawArgs: Record<string, unknown> = { filePath: file, content };

      const preview = await callToolCall(ctx, context, "write", rawArgs, { preview: true });
      if (preview.success === false) {
        throw new Error((preview.message as string) || "write preview failed");
      }

      const denial = await askEditPermission(context, [relPath], {
        filepath: filePath,
        diff: typeof preview.preview_diff === "string" ? preview.preview_diff : "",
      });
      if (denial) return permissionDeniedResponse(denial);

      const data = await callToolCall(ctx, context, "write", rawArgs);

      // Error response (e.g. path validation failure)
      if (data.success === false) {
        throw new Error((data.message as string) || "write failed");
      }

      const output = data.text;

      // Return UI metadata directly on the result. OpenCode's `fromPlugin`
      // (registry.ts) preserves a tool's returned `title`/`metadata` (since
      // v1.4.8; our floor is far past that), so there's no need for the old
      // module-level store + `tool.execute.after` merge — that workaround
      // intermittently lost the diff under duplicate plugin loads (`--port 0`
      // / Desktop) because the store Map lived in one ESM graph and the merge
      // ran in another. See GitHub #96.
      const diff = data.diff as
        | {
            before?: string;
            after?: string;
            additions?: number;
            deletions?: number;
            truncated?: boolean;
          }
        | undefined;
      if (!diff) return output;

      // See the edit tool: >512KB files return counts-only (`truncated`) with
      // no before/after — fabricating an empty `before` would render the whole
      // file as added. Fall back to the preview's hunk-scoped diff.
      const truncated = diff.truncated === true;
      const dp = relativeToWorktree(filePath, projectRoot);
      const beforeContent = diff.before ?? "";
      const afterContent = diff.after ?? content;
      return {
        output,
        title: dp,
        metadata: {
          diff: truncated
            ? typeof preview.preview_diff === "string"
              ? preview.preview_diff
              : ""
            : buildUnifiedDiff(filePath, beforeContent, afterContent),
          ...(truncated
            ? {}
            : {
                filediff: {
                  file: filePath,
                  before: beforeContent,
                  after: afterContent,
                  additions: diff?.additions ?? 0,
                  deletions: diff?.deletions ?? 0,
                },
              }),
          diagnostics: {},
        },
      };
    },
  };
}

// ---------------------------------------------------------------------------
// EDIT tool
// ---------------------------------------------------------------------------

function getEditDescription(ctx: PluginContext, writeToolName: string): string {
  const backupBehavior =
    ctx.config.backup?.enabled === false
      ? "- Backup capture is disabled by user config"
      : "- Backs up files before editing (recoverable via aft_safety undo)";
  return `Edit a file by finding and replacing text, or by targeting named symbols. To write or overwrite a whole file, use the \`${writeToolName}\` tool — \`edit\` requires an explicit edit mode and will not silently overwrite a file from \`content\` alone.

**Modes** (determined by which parameters you provide):

Mode priority: appendContent > edits > symbol (without oldString) > oldString (find/replace). If none match, the call is rejected — there is no implicit "write" fallback. To edit multiple files, make parallel \`edit\` calls in one response.

1. **Append** — pass \`filePath\` + \`appendContent\`
   Appends text to the end of a file, creating the file if it does not exist.
   Example: \`{ "filePath": "notes.txt", "appendContent": "new line\\n" }\`

2. **Batch edits** — pass \`filePath\` + \`edits\` array
   Multiple edits in one file atomically. Each edit is either:
   - \`{ "oldString": "old", "newString": "new" }\` — find/replace
   - \`{ "oldString": "old", "newString": "new", "replaceAll": true }\` — replace every match
   - \`{ "startLine": 5, "endLine": 7, "content": "new lines" }\` — replace line range (1-based, both inclusive)
   Set content to empty string to delete lines.

3. **Symbol replace** — pass \`filePath\` + \`symbol\` + \`content\`
   Replaces an entire named symbol (function, class, type) with new content.
   Includes decorators, attributes, and doc comments in the replacement range.
   **Important:** You must NOT provide \`oldString\` when using symbol mode — if present, the tool silently falls back to find/replace mode.
   Example: \`{ "filePath": "src/app.ts", "symbol": "handleRequest", "content": "function handleRequest() { ... }" }\`

4. **Find and replace** — pass \`filePath\` + \`oldString\` + \`newString\`
   Finds the exact text in \`oldString\` and replaces it with \`newString\`.
   Supports fuzzy matching (handles whitespace differences automatically).
   If multiple matches exist, specify which one with \`occurrence\` or use \`replaceAll: true\`.
   Example: \`{ "filePath": "src/app.ts", "oldString": "const x = 1", "newString": "const x = 2" }\`

5. **Replace all occurrences** — add \`replaceAll: true\`
   Replaces every occurrence of \`oldString\` in the file.
   Example: \`{ "filePath": "src/app.ts", "oldString": "oldName", "newString": "newName", "replaceAll": true }\`

6. **Select specific occurrence** — add \`occurrence: N\` (0-indexed)
   When multiple matches exist, select the Nth one (0 = first, 1 = second, etc.).
   Example: \`{ "filePath": "src/app.ts", "oldString": "TODO", "newString": "DONE", "occurrence": 0 }\`

Note: Modes 5 and 6 are options on mode 4 (find/replace) — they require \`oldString\`.

**Behavior:**
${backupBehavior}
- Auto-formats using project formatter if configured
- Tree-sitter syntax validation on all edits
- Symbol replace includes decorators, attributes, and doc comments in range
- Response is a compact server-rendered summary; before/after diff details are attached as UI metadata when available.`;
}

function createEditTool(ctx: PluginContext, writeToolName = "write"): ToolDefinition {
  return {
    description: getEditDescription(ctx, writeToolName),
    args: {
      filePath: z
        .string()
        .optional()
        .describe("Path to the file to edit (absolute or relative to project root)"),
      oldString: z.string().optional().describe("Text to find (exact match, with fuzzy fallback)"),
      newString: z
        .string()
        .optional()
        .describe("Text to replace with (omit or set to empty string to delete the matched text)"),
      replaceAll: z.boolean().optional().describe("Replace all occurrences"),
      occurrence: optionalInt(0, Number.MAX_SAFE_INTEGER).describe(
        "0-indexed occurrence to replace when multiple matches exist",
      ),
      symbol: z.string().optional().describe("Named symbol to replace (function, class, type)"),
      content: z
        .string()
        .optional()
        .describe(
          "Replacement content for symbol mode. For whole-file writes, use the `write` tool.",
        ),
      appendContent: z
        .string()
        .optional()
        .describe("Text to append to the end of filePath; creates the file if needed"),
      edits: z
        .array(
          z.object({
            oldString: z.string().optional().describe("Text to find for a batch find/replace edit"),
            newString: z
              .string()
              .optional()
              .describe("Replacement text for a batch find/replace edit"),
            replaceAll: z
              .boolean()
              .optional()
              .describe("Replace every occurrence for this batch item"),
            occurrence: optionalInt(0, Number.MAX_SAFE_INTEGER).describe(
              "0-indexed occurrence for this batch item",
            ),
            startLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
              "1-based start line for a batch line-range edit",
            ),
            endLine: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
              "1-based end line for a batch line-range edit",
            ),
            content: z.string().optional().describe("Replacement text for a batch line-range edit"),
          }),
        )
        .optional()
        .describe(
          "Batch edits — array of { oldString, newString }, { oldString, newString, replaceAll: true }, or { startLine, endLine, content } objects",
        ),
    },
    execute: async (args, context): Promise<ToolResult> => {
      // Footgun guard: top-level startLine/endLine are not valid params on
      // edit. They only exist nested inside `edits[]` for batch line-range
      // mode. Without this guard, OpenCode schema handling can strip the
      // unknown keys before the request reaches the server, producing an
      // unrelated mode-resolution error instead of a useful batch-edit hint.
      const argsRecord = args as Record<string, unknown>;
      if (argsRecord.startLine !== undefined || argsRecord.endLine !== undefined) {
        throw new Error(
          "edit: 'startLine'/'endLine' are not top-level parameters. " +
            "For line-range edits, nest them inside the `edits` array: " +
            '`edits: [{ startLine: N, endLine: M, content: "..." }]`. ' +
            "For find/replace, use `oldString`/`newString` instead.",
        );
      }

      const file = args.filePath as string;
      if (!file) throw new Error("'filePath' parameter is required");
      const projectRoot = await resolveProjectRoot(ctx, context);

      const filePath = resolvePathFromProjectRoot(projectRoot, file);

      const relPath = path.relative(projectRoot, filePath);

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      const occurrence = coerceOptionalInt(
        args.occurrence,
        "occurrence",
        0,
        Number.MAX_SAFE_INTEGER,
      );

      const rawArgs: Record<string, unknown> = { filePath: file };
      for (const key of ["appendContent", "symbol", "content", "oldString", "newString"] as const) {
        if (argsRecord[key] !== undefined) rawArgs[key] = argsRecord[key];
      }
      if (Array.isArray(argsRecord.edits)) {
        rawArgs.edits = argsRecord.edits.map((item) => {
          if (!item || typeof item !== "object" || Array.isArray(item)) return item;
          const batchItem = item as Record<string, unknown>;
          return batchItem.replaceAll === undefined
            ? batchItem
            : { ...batchItem, replaceAll: coerceBoolean(batchItem.replaceAll) };
        });
      } else if (argsRecord.edits !== undefined) {
        rawArgs.edits = argsRecord.edits;
      }
      if (argsRecord.replaceAll !== undefined) {
        rawArgs.replaceAll = coerceBoolean(argsRecord.replaceAll);
      }
      if (occurrence !== undefined) rawArgs.occurrence = occurrence;

      const preview = await callToolCall(ctx, context, "edit", rawArgs, { preview: true });
      if (preview.success === false) {
        throw new Error((preview.message as string) || "edit preview failed");
      }

      const denial = await askEditPermission(context, [relPath], {
        filepath: filePath,
        diff: typeof preview.preview_diff === "string" ? preview.preview_diff : "",
      });
      if (denial) return permissionDeniedResponse(denial);

      const data = await callToolCall(ctx, context, "edit", rawArgs);

      // tool_call returns `{ success: false }` responses as data, so failed
      // edits (match-not-found, ambiguous, syntax rollback, or glob with zero
      // matches) must still be surfaced as thrown tool errors.
      if (data.success === false) {
        throw new Error((data.message as string) || "edit failed");
      }

      const output = data.text;
      const diff = data.diff as
        | {
            before?: string;
            after?: string;
            additions?: number;
            deletions?: number;
            truncated?: boolean;
          }
        | undefined;
      if (!diff) return output;

      // UI metadata returned directly on the result (see write tool for the
      // rationale; replaces the old metadata-store + after-hook merge that
      // intermittently lost the diff under duplicate plugin loads — GitHub #96).
      //
      // Files over Rust's 512KB diff cap return counts-only (`truncated: true`)
      // with no before/after. Fabricating empty contents here rendered a blank
      // diff in the UI — use the preview's hunk-scoped unified diff instead
      // (it scales with the edit, not the file) and omit the filediff view.
      const truncated = diff.truncated === true;
      const beforeContent = diff.before ?? "";
      const afterContent = diff.after ?? "";
      const uiMeta = {
        diff: truncated
          ? typeof preview.preview_diff === "string"
            ? preview.preview_diff
            : ""
          : buildUnifiedDiff(filePath, beforeContent, afterContent),
        ...(truncated
          ? {}
          : {
              filediff: {
                file: filePath,
                before: beforeContent,
                after: afterContent,
                additions: diff.additions ?? 0,
                deletions: diff.deletions ?? 0,
              },
            }),
        diagnostics: {},
      };
      return { output, title: relativeToWorktree(filePath, projectRoot), metadata: uiMeta };
    },
  };
}

// ---------------------------------------------------------------------------
// APPLY_PATCH tool
// ---------------------------------------------------------------------------

function applyPatchDescription(ctx: PluginContext): string {
  const backupBehavior =
    ctx.config.backup?.enabled === false
      ? "- Backup capture is disabled by user config; applied file changes are not recorded in the undo stack."
      : "- Per-file commit: each file's edits apply independently. If a later file fails, earlier successful changes are kept. Use `aft_safety` undo if you need to revert the applied changes.\n- Files are backed up before modification";
  return `Use the \`apply_patch\` tool to edit files. Your patch language is a stripped‑down, file‑oriented diff format designed to be easy to parse and safe to apply. You can think of it as a high‑level envelope:

*** Begin Patch
[ one or more file sections ]
*** End Patch

Within that envelope, you get a sequence of file operations.
You MUST include a header to specify the action you are taking.
Each operation starts with one of three headers:

*** Add File: <path> - create a new file. Every following line is a + line (the initial contents).
*** Delete File: <path> - remove an existing file. Nothing follows.
*** Update File: <path> - patch an existing file in place (optionally with a rename).
*** Move to: <path> - after update file header, renames the file.


Example patch:

\`\`\`
*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print("Hi")
+print("Hello, world!")
*** Delete File: obsolete.txt
*** End Patch
\`\`\`

**Behavior:**
${backupBehavior}
- Parent directories are created automatically for new files
- Fuzzy matching for context anchors (handles whitespace and Unicode differences)

**It is important to remember:**

- You must include a header with your intended action (Add/Delete/Update)
- You must prefix new lines with \`+\` even when creating a new file

Edits return as soon as the write completes unless \`lsp.diagnostics_on_edit\` requests legacy sync-wait behavior. Call \`aft_inspect\` afterward to check diagnostics across a batch of edits.`;
}

function applyPatchErrorMessage(response: Record<string, unknown>, fallback: string): string {
  for (const key of ["text", "output", "message"] as const) {
    const value = response[key];
    if (typeof value === "string" && value.length > 0) return value;
  }
  return fallback;
}

function stringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string")
    : [];
}

function createApplyPatchTool(ctx: PluginContext): ToolDefinition {
  return {
    description: applyPatchDescription(ctx),
    args: {
      patchText: z.string().describe("The full patch text including Begin/End markers"),
    },
    execute: async (args, context): Promise<ToolResult> => {
      const patchText = args.patchText as string;
      if (!patchText) throw new Error("'patchText' is required");

      const preview = await callToolCall(
        ctx,
        context,
        "apply_patch",
        { patchText },
        { preview: true },
      );
      if (preview.success === false) {
        throw new Error(applyPatchErrorMessage(preview, "apply_patch preview failed"));
      }

      const askedExternalPaths = new Set<string>();
      for (const filePath of stringArray(preview.affected_paths)) {
        if (askedExternalPaths.has(filePath)) continue;
        askedExternalPaths.add(filePath);
        const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
        if (denial) return permissionDeniedResponse(denial);
      }

      const affectedRelPaths = stringArray(preview.affected_rel_paths);
      const denial = await askEditPermission(context, affectedRelPaths, {
        diff: typeof preview.preview_diff === "string" ? preview.preview_diff : "",
        filepath: typeof preview.filepath === "string" ? preview.filepath : affectedRelPaths[0],
      });
      if (denial) return permissionDeniedResponse(denial);

      const response = await callToolCall(ctx, context, "apply_patch", { patchText });
      if (response.success === false) {
        throw new Error(applyPatchErrorMessage(response, "apply_patch failed"));
      }

      const metadata =
        response.metadata &&
        typeof response.metadata === "object" &&
        !Array.isArray(response.metadata)
          ? (response.metadata as Record<string, unknown>)
          : {};
      const result: {
        output: string;
        title?: string;
        metadata: { diff: unknown; files: unknown };
      } = {
        output:
          typeof response.text === "string" ? response.text : applyPatchErrorMessage(response, ""),
        metadata: {
          diff: typeof metadata.diff === "string" ? metadata.diff : "",
          files: Array.isArray(metadata.files) ? metadata.files : [],
        },
      };
      if (typeof response.title === "string" && response.title.length > 0) {
        result.title = response.title;
      }
      return result;
    },
  };
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

function deleteDescription(ctx: PluginContext): string {
  const backupText =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config, so this tool does not create undo snapshots. "
      : "Each file is backed up before deletion — use aft_safety undo to recover any of them. For directories, every file inside is individually backed up before the tree is removed. ";
  return (
    "Delete one or more files (or directories).\n\n" +
    backupText +
    "Directory deletion requires recursive: true. Without it, passing a directory returns an error.\n\n" +
    "Partial success is allowed: deletable files are deleted; failed ones are reported in `skipped_files` with `complete: false`."
  );
}

function createDeleteTool(ctx: PluginContext): ToolDefinition {
  return {
    description: deleteDescription(ctx),
    args: {
      files: z
        .array(z.string())
        .min(1)
        .describe("Paths to delete (one or more). May include directories when recursive=true."),
      recursive: z
        .boolean()
        .optional()
        .describe(
          "Required to delete a directory and its contents. Defaults to false; passing a directory without this returns an error.",
        ),
    },
    execute: async (args, context): Promise<string> => {
      // Coerce at the boundary: some hosts deliver `files` as a bare string or
      // a JSON-stringified array despite the schema, which would crash the
      // unchecked `.map` below before any validation runs.
      const inputs = coerceStringArray(args.files);
      if (inputs.length === 0) {
        throw new Error("delete: `files` must be a non-empty array of paths");
      }
      // Coerce at the boundary: hosts deliver this boolean as the model's raw
      // emitted value (e.g. the string "true") despite the declared schema, same
      // as `files` above. A strict `=== true` then drops a stringified flag and
      // an agent's `recursive: true` is silently lost (see coerceBoolean).
      const recursive = coerceBoolean(args.recursive);
      const projectRoot = await resolveProjectRoot(ctx, context);
      const absolutePaths = inputs.map((f) => resolvePathFromProjectRoot(projectRoot, f));

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const asked = new Set<string>();
        for (const filePath of absolutePaths) {
          if (asked.has(filePath)) continue;
          asked.add(filePath);
          const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
          if (denial) return permissionDeniedResponse(denial);
        }
      }

      await runAsk(
        context.ask({
          permission: "edit",
          patterns: absolutePaths,
          always: ["*"],
          metadata: { action: "delete", count: absolutePaths.length },
        }),
      );

      // Single batched call so every file shares one op_id; one `aft_safety
      // undo` then restores the whole delete atomically.
      const response = await callToolCall(ctx, context, "delete", {
        files: absolutePaths,
        recursive,
      });

      if (response.success === false) {
        throw new Error((response.message as string | undefined) ?? "delete failed");
      }

      const deletedEntries = (response.deleted as Array<{ file: string }> | undefined) ?? [];
      const skipped =
        (response.skipped_files as Array<{ file: string; reason: string }> | undefined) ?? [];
      const deleted = deletedEntries.map((entry) => entry.file);

      // Refuse a fully-failed batch with a real error so the agent surface
      // doesn't silently render "completed" for nothing-actually-deleted.
      if (deleted.length === 0 && skipped.length > 0) {
        throw new Error(
          `delete failed for all ${skipped.length} file(s):\n` +
            skipped.map((entry) => `  ${entry.file}: ${entry.reason}`).join("\n"),
        );
      }

      return response.text;
    },
  };
}

// ---------------------------------------------------------------------------
// Move / Rename
// ---------------------------------------------------------------------------

function moveDescription(ctx: PluginContext): string {
  const backupText =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config. "
      : "Creates an undo backup before moving. ";
  return (
    `Move or rename a file. ${backupText}Creates parent directories for destination automatically\n` +
    "Note: This moves/renames files at the OS level. To move a code symbol (function, class, type) between files while updating imports, use `aft_refactor` op='move' instead."
  );
}

function createMoveTool(ctx: PluginContext): ToolDefinition {
  return {
    description: moveDescription(ctx),
    args: {
      filePath: z
        .string()
        .describe("Source file path to move (absolute or relative to project root)"),
      destination: z
        .string()
        .describe("Destination file path (absolute or relative to project root)"),
    },
    execute: async (args, context): Promise<string> => {
      const projectRoot = await resolveProjectRoot(ctx, context);
      const filePath = resolvePathFromProjectRoot(projectRoot, args.filePath as string);
      const destPath = resolvePathFromProjectRoot(projectRoot, args.destination as string);

      // External-directory check first (mirrors opencode-native edit.ts:68).
      {
        const sourceDenial = await assertExternalDirectoryPermission(ctx, context, filePath, {
          kind: "file",
        });
        if (sourceDenial) return permissionDeniedResponse(sourceDenial);
        if (destPath !== filePath) {
          const destDenial = await assertExternalDirectoryPermission(ctx, context, destPath);
          if (destDenial) return permissionDeniedResponse(destDenial);
        }
      }

      await runAsk(
        context.ask({
          permission: "edit",
          patterns: [filePath, destPath],
          always: ["*"],
          metadata: { action: "move" },
        }),
      );

      const result = await callToolCall(ctx, context, "move", {
        filePath: args.filePath as string,
        destination: args.destination as string,
      });
      if (result.success === false) {
        throw new Error((result.message as string) || "move failed");
      }
      return result.text;
    },
  };
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

/**
 * Returns hoisted tools keyed by opencode's built-in names.
 * Overrides: read, write, edit, apply_patch (always when hoisting is on).
 *
 * Bash hoisting follows the resolved `bash` config. When bash is enabled, the
 * primary `bash` tool is registered. Background control tools (`bash_status`,
 * `bash_write`, `bash_watch`, and `bash_kill`) are registered only when
 * `bash.background` resolves true. With `bash.background: false`, foreground
 * bash runs to completion inline and no background surface is exposed.
 */
export function hoistedTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const tools: Record<string, ToolDefinition> = {
    read: createReadTool(ctx),
    write: createWriteTool(ctx, "edit"),
    edit: createEditTool(ctx, "write"),
    apply_patch: createApplyPatchTool(ctx),
    aft_delete: createDeleteTool(ctx),
    aft_move: createMoveTool(ctx),
  };

  // Bash hoisting is gated by the single resolved bash config — see
  // `resolveBashConfig` in config.ts for the precedence rules. `bash` itself
  // registers whenever bash is enabled; the background control tools register
  // only when `bash.background` is enabled.
  const bashCfg = resolveBashConfig(ctx.config);
  if (bashCfg.enabled) {
    tools.bash = createBashTool(ctx);
    if (bashCfg.background) {
      tools.bash_status = createBashStatusTool(ctx);
      tools.bash_write = createBashWriteTool(ctx);
      tools.bash_watch = createBashWatchTool(ctx);
      tools.bash_kill = createBashKillTool(ctx);
    }
  }

  return tools;
}

/**
 * Returns the same tools with aft_ prefix (for when hoisting is disabled).
 */
export function aftPrefixedTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const aftEditTool = createEditTool(ctx, "aft_write");

  const tools: Record<string, ToolDefinition> = {
    aft_read: createReadTool(ctx),
    aft_write: createWriteTool(ctx, "aft_edit"),
    aft_edit: {
      ...aftEditTool,
      // Returns the inner aft_edit tool's result OR a JSON envelope string for
      // the legacy mode:"write" shim. Newer @opencode-ai/plugin versions
      // widened ToolResult from `string` to `string | { output, metadata? }`,
      // so we accept both shapes here; the OpenCode runtime handles both.
      execute: async (args, context) => {
        const argRecord = args as Record<string, unknown>;
        // Legacy back-compat: callers (mostly older tests/integrations) used
        // `{ mode, file, ... }` instead of the current schema. Translate
        // `file` -> `filePath` so the rest of the wrapper sees the modern
        // shape. The current edit tool ignores the `mode` field; we keep it
        // in the args object only so the explicit `mode: "write"` branch
        // below can detect it.
        const normalizedArgs: Record<string, unknown> =
          argRecord.mode !== undefined &&
          argRecord.filePath === undefined &&
          typeof argRecord.file === "string"
            ? { ...argRecord, filePath: argRecord.file }
            : { ...argRecord };

        // Explicit legacy `mode: "write"` — route directly to the Rust
        // `write` command. We do NOT fall through to the modern edit tool
        // here, because the modern tool deliberately rejects content-only
        // calls (the v0.17.2 footgun fix). Legacy `mode: "write"` is an
        // *explicit* whole-file write request, which is fine; the danger is
        // *implicit* whole-file writes where a typo in another mode-selecting
        // param silently degrades into overwrite. Returns the same JSON
        // envelope shape the legacy callers expect (success / file /
        // syntax_valid / etc.), not the human-readable string the modern
        // `write` tool returns.
        if (
          normalizedArgs.mode === "write" &&
          typeof normalizedArgs.filePath === "string" &&
          typeof normalizedArgs.content === "string"
        ) {
          const file = normalizedArgs.filePath as string;
          const projectRoot = await resolveProjectRoot(ctx, context);
          const filePath = resolvePathFromProjectRoot(projectRoot, file);
          const relPath = path.relative(projectRoot, filePath);

          // External-directory check first (mirrors opencode-native write.ts:43).
          {
            const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
            if (denial) return permissionDeniedResponse(denial);
          }

          const currentContent = await readCurrentFileForPreview(filePath);
          const previewDiff = buildUnifiedDiff(
            filePath,
            currentContent,
            normalizedArgs.content as string,
          );
          const denial = await askEditPermission(context, [relPath], {
            filepath: filePath,
            diff: previewDiff,
          });
          if (denial) return permissionDeniedResponse(denial);
          const writeParams: Record<string, unknown> = {
            file: filePath,
            content: normalizedArgs.content as string,
            create_dirs: normalizedArgs.create_dirs !== false,
            diagnostics: normalizedArgs.diagnostics ?? diagnosticsOnEditDefault(ctx),
          };
          const response = await callBridge(ctx, context, "write", writeParams);
          if (response.success === false) {
            throw new Error((response.message as string | undefined) ?? "write failed");
          }
          return JSON.stringify(response);
        }

        return aftEditTool.execute(normalizedArgs, context);
      },
    },
    aft_apply_patch: createApplyPatchTool(ctx),
    aft_delete: createDeleteTool(ctx),
    aft_move: createMoveTool(ctx),
  };

  // Hoist-off mode: same gating as hoisted mode but with the aft_ prefix on
  // the primary bash tool so it doesn't override OpenCode's native bash. The
  // background control tools keep their unprefixed names because they refer to
  // AFT-spawned task IDs that the native bash doesn't know about.
  const bashCfg = resolveBashConfig(ctx.config);
  if (bashCfg.enabled) {
    tools.aft_bash = createBashTool(ctx);
    if (bashCfg.background) {
      tools.bash_status = createBashStatusTool(ctx);
      tools.bash_write = createBashWriteTool(ctx);
      tools.bash_watch = createBashWatchTool(ctx);
      tools.bash_kill = createBashKillTool(ctx);
    }
  }

  return tools;
}
