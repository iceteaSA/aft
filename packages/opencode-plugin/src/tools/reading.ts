import { dirname, resolve } from "node:path";
import { formatZoomMultiTargetResult, formatZoomText } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { storeToolMetadata } from "../metadata-store.js";
import type { PluginContext } from "../types.js";
import { callBridge, optionalInt } from "./_shared.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

/** Read the OpenCode runtime callID off the tool context (shape varies by host version). */
function getCallID(ctx: unknown): string | undefined {
  const c = ctx as { callID?: string; callId?: string; call_id?: string };
  return c.callID ?? c.callId ?? c.call_id;
}

/** Build a short TUI title for an `aft_zoom` invocation, based on which mode the agent used. */
function buildZoomTitle(args: {
  filePath?: string;
  url?: string;
  symbols?: string | string[];
  targets?: { filePath: string; symbol: string } | Array<{ filePath: string; symbol: string }>;
}): string {
  if (args.targets !== undefined && args.targets !== null) {
    if (Array.isArray(args.targets)) {
      if (args.targets.length === 1 && args.targets[0]) {
        return `${args.targets[0].filePath}#${args.targets[0].symbol}`;
      }
      return `${args.targets.length} targets across files`;
    }
    return `${args.targets.filePath}#${args.targets.symbol}`;
  }

  const path = args.filePath ?? args.url ?? "";
  if (typeof args.symbols === "string") return path ? `${path}#${args.symbols}` : args.symbols;
  if (Array.isArray(args.symbols) && args.symbols.length > 0) {
    if (args.symbols.length === 1) return path ? `${path}#${args.symbols[0]}` : args.symbols[0];
    return path ? `${path} (${args.symbols.length} symbols)` : `${args.symbols.length} symbols`;
  }
  return path || "(no target)";
}

interface ZoomBatchSymbolResult {
  name: string;
  success: boolean;
  content?: string;
  error?: string;
}

interface ZoomBatchResult {
  complete: boolean;
  symbols: ZoomBatchSymbolResult[];
  text: string;
}

/**
 * Tool definitions for code reading commands: outline + zoom.
 */
export function readingTools(ctx: PluginContext): Record<string, ToolDefinition> {
  return {
    aft_outline: {
      description:
        "Structural outline of source code, documentation files, or remote URLs. For code, returns symbols (functions, classes, types) with line ranges. For Markdown and HTML, returns heading hierarchy. Use this to explore structure before reading specific sections with aft_zoom. Set `files: true` with a directory target for a flat indexed file tree with language, symbol count, and byte metadata.\n\n" +
        "Pass a single `target`:\n" +
        "  • file path → outline that file (with signatures)\n" +
        "  • directory path → outline all source files under it (recursively, up to 200 files)\n" +
        "  • URL (http:// or https://) → fetch and outline a remote HTML/Markdown document\n" +
        "  • array of paths → outline multiple files in one call; with files:true, every path must be a directory",
      args: {
        target: z
          .union([z.string(), z.array(z.string())])
          .describe(
            "What to outline: a file path, directory path, URL, or array of paths. The mode is auto-detected: URLs by `http://`/`https://` prefix, directories by stat, arrays as multi-file.",
          ),
        files: z
          .boolean()
          .optional()
          .describe(
            "Directory-only mode: when true, target must be a directory or array of directories and the result is a flat file tree with path, language, symbol count, and byte size instead of a symbol outline.",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const target = args.target;
        const filesMode = args.files === true;
        const hasUrl =
          typeof target === "string" &&
          (target.startsWith("http://") || target.startsWith("https://"));
        const isArray = Array.isArray(target) && target.length > 0;

        if (filesMode) {
          if (Array.isArray(target)) {
            if (target.length === 0) {
              throw new Error("'target' must be a non-empty string or array of strings");
            }
            const permissionDenied = await assertOutlineFilesExternalPermissions(context, target);
            if (permissionDenied) return permissionDeniedResponse(permissionDenied);

            const response = await callBridge(ctx, context, "outline", { target, files: true });
            if (response.success === false) {
              throw new Error((response.message as string) || "outline failed");
            }
            return formatOutlineFilesText(response);
          }

          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }

          const resolvedPath = resolve(context.directory, target);
          const permissionDenied = await assertOutlineFilesExternalPermissions(
            context,
            resolvedPath,
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);

          let isDirectory = false;
          try {
            const { stat } = await import("node:fs/promises");
            const st = await stat(resolvedPath);
            isDirectory = st.isDirectory();
          } catch {
            // Let Rust report missing paths with its structured error shape.
          }

          const params = isDirectory
            ? { directory: resolvedPath, files: true }
            : { file: target, files: true };
          const response = await callBridge(ctx, context, "outline", params);
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return formatOutlineFilesText(response);
        }

        // URL mode: pass through to Rust; Rust fetches, validates, and caches.
        if (hasUrl) {
          const response = await callBridge(ctx, context, "outline", { file: target });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return formatOutlineText(response);
        }

        // Multi-file mode
        if (isArray) {
          const response = await callBridge(ctx, context, "outline", {
            files: target as string[],
          });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return formatOutlineText(response);
        }

        // String mode: stat to disambiguate file vs directory
        if (typeof target !== "string" || target.length === 0) {
          throw new Error("'target' must be a non-empty string or array of strings");
        }

        let isDirectory = false;
        try {
          const { stat } = await import("node:fs/promises");
          const resolved = resolve(context.directory, target);
          const st = await stat(resolved);
          isDirectory = st.isDirectory();
        } catch {
          // Path doesn't exist locally — fall through to single-file mode and
          // let Rust report the real error with its preferred shape.
        }

        if (isDirectory) {
          const dirPath = resolve(context.directory, target);
          const response = await callBridge(ctx, context, "outline", { directory: dirPath });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return JSON.stringify(response, null, 2);
        }

        const response = await callBridge(ctx, context, "outline", { file: target });
        if (response.success === false) {
          throw new Error((response.message as string) || "outline failed");
        }
        return formatOutlineText(response);
      },
    },

    aft_zoom: {
      description:
        "Inspect code symbols or documentation sections. For code, returns the full source of a symbol with call-graph annotations (what it calls and what calls it). For Markdown and HTML, returns the section content under the given heading.\n\nUse exactly ONE mode: `{ filePath, symbols }`, `{ url, symbols }`, or `{ targets }`. `symbols` can be a string or array (one or many lookups in the same file/URL). Use `targets` for cross-file batches: `{ filePath, symbol }` or an array of them.",
      args: {
        filePath: z
          .string()
          .optional()
          .describe("Path to file (absolute or relative to project root)"),
        url: z
          .string()
          .optional()
          .describe("HTTP/HTTPS URL of an HTML or Markdown document to fetch and zoom into"),
        symbols: z
          .union([z.string(), z.array(z.string())])
          .optional()
          .describe(
            "Symbol name for code, or heading text for Markdown/HTML. Pass a string for one lookup or an array for batched lookups in the same file/URL.",
          ),
        targets: z
          .union([
            z.object({
              filePath: z.string().describe("Path to file (absolute or relative to project root)"),
              symbol: z.string().describe("Symbol name in that file"),
            }),
            z.array(
              z.object({
                filePath: z
                  .string()
                  .describe("Path to file (absolute or relative to project root)"),
                symbol: z.string().describe("Symbol name in that file"),
              }),
            ),
          ])
          .optional()
          .describe(
            "Cross-file batch: `{ filePath, symbol }` or an array of them. Mutually exclusive with filePath/url/symbols.",
          ),
        contextLines: optionalInt(1, Number.MAX_SAFE_INTEGER).describe(
          "Lines of context before/after the symbol (default: 3)",
        ),
      },
      execute: async (args, context): Promise<string> => {
        const hasFilePath = typeof args.filePath === "string" && args.filePath.length > 0;
        const hasUrl = typeof args.url === "string" && args.url.length > 0;
        const hasTargets = args.targets !== undefined && args.targets !== null;
        const hasSymbols =
          args.symbols !== undefined &&
          args.symbols !== null &&
          (typeof args.symbols === "string"
            ? args.symbols.length > 0
            : Array.isArray(args.symbols) && args.symbols.length > 0);

        // Set TUI title BEFORE any bridge call so even errors render with a
        // meaningful tool-call header. OpenCode's default auto-title only
        // surfaces the first scalar arg, which leaves array/object-only
        // invocations (symbols: [...], targets: {...}, targets: [...]) showing
        // up as a bare `aft_zoom` with no hint.
        const zoomCallID = getCallID(context);
        if (zoomCallID) {
          const title = buildZoomTitle(args);
          storeToolMetadata(context.sessionID, zoomCallID, { title, metadata: { title } });
        }

        // Multi-target mode (cross-file). Mutually exclusive with the other
        // modes so the agent doesn't accidentally provide overlapping inputs
        // that get silently ignored.
        if (hasTargets) {
          if (hasFilePath || hasUrl || hasSymbols) {
            throw new Error(
              "'targets' is mutually exclusive with 'filePath', 'url', and 'symbols'",
            );
          }
          const targets = Array.isArray(args.targets)
            ? (args.targets as Array<{ filePath: string; symbol: string }>)
            : ([args.targets] as Array<{ filePath: string; symbol: string }>);
          if (targets.length === 0) {
            throw new Error("'targets' must be a non-empty object or array");
          }
          for (const [i, entry] of targets.entries()) {
            if (!entry || typeof entry.filePath !== "string" || entry.filePath.length === 0) {
              throw new Error(`targets[${i}].filePath must be a non-empty string`);
            }
            if (typeof entry.symbol !== "string" || entry.symbol.length === 0) {
              throw new Error(`targets[${i}].symbol must be a non-empty string`);
            }
          }
          const responses = await Promise.all(
            targets.map((t) => {
              const params: Record<string, unknown> = { file: t.filePath, symbol: t.symbol };
              if (args.contextLines !== undefined) params.context_lines = args.contextLines;
              return callBridge(ctx, context, "zoom", params).catch((err) => ({
                success: false,
                message: err instanceof Error ? err.message : String(err),
              }));
            }),
          );
          const entries = targets.map((t, i) => ({
            targetLabel: t.filePath,
            name: t.symbol,
            response: responses[i] ?? { success: false, message: "missing zoom response" },
          }));
          return formatZoomMultiTargetResult(entries).text;
        }

        if (!hasFilePath && !hasUrl) {
          throw new Error("Provide exactly one of 'filePath', 'url', or 'targets'");
        }
        if (hasFilePath && hasUrl) {
          throw new Error("Provide exactly ONE of 'filePath' or 'url' — not both");
        }

        // URL mode: pass through to Rust; Rust fetches, validates, and caches.
        const file = hasUrl ? (args.url as string) : (args.filePath as string);

        // Header label — what the agent typed, not the on-disk cache path.
        const targetLabel = (hasUrl ? (args.url as string) : (args.filePath as string)) ?? file;

        // Normalize symbols → array (or undefined if not provided).
        // String input is treated as a single-element array; single-string
        // shortcut still returns the raw zoom text instead of a batch wrapper
        // so the happy path doesn't show "Incomplete" framing.
        const symbolsArray: string[] | undefined = hasSymbols
          ? typeof args.symbols === "string"
            ? [args.symbols]
            : (args.symbols as string[])
          : undefined;

        if (symbolsArray) {
          const results = await Promise.all(
            symbolsArray.map((sym) => {
              const params: Record<string, unknown> = { file, symbol: sym };
              if (args.contextLines !== undefined) params.context_lines = args.contextLines;
              return callBridge(ctx, context, "zoom", params).catch((err) => ({
                success: false,
                message: err instanceof Error ? err.message : String(err),
              }));
            }),
          );
          if (symbolsArray.length === 1) {
            const response = results[0] ?? { success: false, message: "missing zoom response" };
            if ((response as { success?: boolean }).success === false) {
              throw new Error(
                ((response as { message?: string }).message as string) || "zoom failed",
              );
            }
            return formatZoomText(targetLabel, response as Record<string, unknown>);
          }
          return formatZoomBatchResult(targetLabel, symbolsArray, results).text;
        }

        // No symbols specified: zoom by line-range fallback (or whole file).
        const params: Record<string, unknown> = { file };
        if (args.contextLines !== undefined) params.context_lines = args.contextLines;

        const data = await callBridge(ctx, context, "zoom", params);
        if (data.success === false) {
          throw new Error((data.message as string) || "zoom failed");
        }
        return formatZoomText(targetLabel, data);
      },
    },
  };
}

/**
 * Format multi-symbol zoom results as plain text. Successful entries use
 * `formatZoomText` (line-numbered, no JSON escapes); failures render as
 * `Symbol "name" not found: <reason>`. Sections are blank-line separated.
 *
 * Exported for regression tests.
 */
export function formatZoomBatchResult(
  targetLabel: string,
  symbols: string[],
  responses: Record<string, unknown>[],
): ZoomBatchResult {
  const entries = symbols.map((name, index): ZoomBatchSymbolResult => {
    const response = responses[index] ?? { success: false, message: "missing zoom response" };
    if (response.success === false) {
      const message =
        typeof response.message === "string" && response.message.length > 0
          ? response.message
          : "zoom failed";
      return { name, success: false, error: message };
    }
    return { name, success: true, content: formatZoomText(targetLabel, response) };
  });
  const complete = entries.every((entry) => entry.success);
  const sections: string[] = [];
  if (!complete) {
    sections.push("Incomplete zoom results: one or more symbols failed.");
  }
  for (const entry of entries) {
    if (entry.success) {
      sections.push(entry.content ?? "");
    } else {
      sections.push(`Symbol "${entry.name}" not found: ${entry.error ?? "zoom failed"}`);
    }
  }
  return { complete, symbols: entries, text: sections.join("\n\n") };
}

/**
 * Format an outline response into agent-readable text, appending honest skip
 * reporting when files were intentionally skipped (parse error, unsupported
 * language, file not found, too large). Without this, agents only see the tree
 * and assume all input files were processed.
 */
interface SkippedOutlineFile {
  file: string;
  reason: string;
}

const MAX_UNCHECKED_FILES_IN_FOOTER = 10;

async function assertOutlineFilesExternalPermissions(
  context: ToolContext,
  target: string | string[],
): Promise<string | undefined> {
  const targets = Array.isArray(target) ? target : [target];
  const checkedParents = new Set<string>();

  for (const rawTarget of targets) {
    if (typeof rawTarget !== "string" || rawTarget.length === 0) continue;
    const resolvedPath = resolve(context.directory, rawTarget);
    const parentDir = dirname(resolvedPath);
    if (checkedParents.has(parentDir)) continue;
    checkedParents.add(parentDir);

    const denial = await assertExternalDirectoryPermission(context, resolvedPath);
    if (denial) return denial;
  }

  return undefined;
}

function formatOutlineText(response: Record<string, unknown>): string {
  const text = (response.text as string | undefined) ?? "";
  const skipped = response.skipped_files as SkippedOutlineFile[] | undefined;
  if (!skipped || skipped.length === 0) {
    return text;
  }
  const lines = skipped.map(({ file, reason }) => `  ${file} — ${reason}`).join("\n");
  const header = text.length > 0 ? `${text}\n\n` : "";
  return `${header}Skipped ${skipped.length} file(s):\n${lines}`;
}

export function formatOutlineFilesText(response: Record<string, unknown>): string {
  const text = formatOutlineText(response);
  const uncheckedFiles = Array.isArray(response.unchecked_files)
    ? response.unchecked_files.filter(
        (file): file is string => typeof file === "string" && file.length > 0,
      )
    : [];
  const isPartial =
    response.complete === false || response.walk_truncated === true || uncheckedFiles.length > 0;

  if (!isPartial) {
    return text;
  }

  const footer: string[] = [];
  if (response.walk_truncated === true) {
    const uncheckedCount = uncheckedFiles.length;
    const suffix =
      uncheckedCount > 0
        ? ` ${uncheckedCount} additional files in this directory were not indexed.`
        : " Some files in this directory were not indexed.";
    footer.push(`⚠ Partial result: walk truncated at 200 files.${suffix}`);
  } else {
    const suffix =
      uncheckedFiles.length > 0
        ? ` ${uncheckedFiles.length} files in this directory were not indexed.`
        : " Some files in this directory were not indexed.";
    footer.push(`⚠ Partial result:${suffix}`);
  }

  if (uncheckedFiles.length > 0) {
    footer.push("Unchecked files:");
    footer.push(
      ...uncheckedFiles.slice(0, MAX_UNCHECKED_FILES_IN_FOOTER).map((file) => `  ${file}`),
    );
    const remaining = uncheckedFiles.length - MAX_UNCHECKED_FILES_IN_FOOTER;
    if (remaining > 0) {
      footer.push(`  ... +${remaining} more`);
    }
  }

  const header = text.length > 0 ? `${text}\n\n` : "";
  return `${header}${footer.join("\n")}`;
}
