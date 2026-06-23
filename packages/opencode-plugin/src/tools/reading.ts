import {
  coerceBoolean,
  formatZoomMultiTargetResult,
  formatZoomText,
  unwrapRustZoomBatchEnvelope,
} from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam, optionalInt, resolvePathArg } from "./_shared.js";
import { assertExternalDirectoryPermission, permissionDeniedResponse } from "./permissions.js";

const z = tool.schema;

/** Build a short TUI title for an `aft_zoom` invocation, based on which mode the agent used. */
function buildZoomTitle(args: {
  filePath?: string;
  url?: string;
  symbols?: string | string[];
  targets?: { filePath: string; symbol: string } | Array<{ filePath: string; symbol: string }>;
}): string {
  // Use isEmptyParam so empty arrays / null / "" don't produce
  // "0 targets across files" — let the function fall through to the
  // filePath/url/symbols branches instead.
  if (!isEmptyParam(args.targets)) {
    if (Array.isArray(args.targets)) {
      if (args.targets.length === 1 && args.targets[0]) {
        return `${args.targets[0].filePath}#${args.targets[0].symbol}`;
      }
      return `${args.targets.length} targets across files`;
    }
    // biome-ignore lint/style/noNonNullAssertion: isEmptyParam guards null/undefined
    return `${args.targets!.filePath}#${args.targets!.symbol}`;
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
        // Coerce at the boundary: stringified "true" must enable files mode (coerceBoolean).
        const filesMode = coerceBoolean(args.files);
        const hasUrl =
          typeof target === "string" &&
          (target.startsWith("http://") || target.startsWith("https://"));
        const isArray = Array.isArray(target) && target.length > 0;

        if (filesMode) {
          if (Array.isArray(target)) {
            if (target.length === 0) {
              throw new Error("'target' must be a non-empty string or array of strings");
            }
            const resolvedTargets = await Promise.all(
              target.map((entry) => resolvePathArg(ctx, context, entry)),
            );
            const permissionDenied = await assertPathExternalPermissions(
              ctx,
              context,
              resolvedTargets,
              "directory",
            );
            if (permissionDenied) return permissionDeniedResponse(permissionDenied);

            const response = await callBridge(ctx, context, "outline", {
              target: resolvedTargets,
              files: true,
            });
            if (response.success === false) {
              throw new Error((response.message as string) || "outline failed");
            }
            return formatOutlineFilesText(response);
          }

          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }

          const resolvedPath = await resolvePathArg(ctx, context, target);

          let isDirectory = false;
          try {
            const { stat } = await import("node:fs/promises");
            const st = await stat(resolvedPath);
            isDirectory = st.isDirectory();
          } catch {
            // Let Rust report missing paths with its structured error shape.
          }

          const permissionDenied = await assertPathExternalPermissions(
            ctx,
            context,
            resolvedPath,
            isDirectory ? "directory" : "file",
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);

          const params = isDirectory
            ? { directory: resolvedPath, files: true }
            : { file: resolvedPath, files: true };
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
          const resolvedTargets = await Promise.all(
            (target as string[]).map((entry) => resolvePathArg(ctx, context, entry)),
          );
          const permissionDenied = await assertPathExternalPermissions(
            ctx,
            context,
            resolvedTargets,
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);

          const response = await callBridge(ctx, context, "outline", {
            files: resolvedTargets,
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

        const resolvedTarget = await resolvePathArg(ctx, context, target);
        let isDirectory = false;
        try {
          const { stat } = await import("node:fs/promises");
          const st = await stat(resolvedTarget);
          isDirectory = st.isDirectory();
        } catch {
          // Path doesn't exist locally — fall through to single-file mode and
          // let Rust report the real error with its preferred shape.
        }

        const permissionDenied = await assertPathExternalPermissions(
          ctx,
          context,
          resolvedTarget,
          isDirectory ? "directory" : "file",
        );
        if (permissionDenied) return permissionDeniedResponse(permissionDenied);

        if (isDirectory) {
          const response = await callBridge(ctx, context, "outline", { directory: resolvedTarget });
          if (response.success === false) {
            throw new Error((response.message as string) || "outline failed");
          }
          return JSON.stringify(response, null, 2);
        }

        const response = await callBridge(ctx, context, "outline", { file: resolvedTarget });
        if (response.success === false) {
          throw new Error((response.message as string) || "outline failed");
        }
        return formatOutlineText(response);
      },
    },

    aft_zoom: {
      description:
        "Inspect code symbols or documentation sections. For code, returns the full source of a symbol. Pass `callgraph: true` to also include call-graph annotations (calls-out / called-by within the same file). For Markdown and HTML, returns the section content under the given heading.\n\nUse exactly ONE mode: `{ filePath, symbols }`, `{ url, symbols }`, or `{ targets }`. `symbols` can be a string or array (one or many lookups in the same file/URL). Use `targets` for cross-file batches: `{ filePath, symbol }` or an array of them.",
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
        callgraph: z
          .boolean()
          .optional()
          .describe(
            "Include call-graph annotations (calls-out / called-by within the same file). Default false; off keeps zoom output minimal.",
          ),
      },
      execute: async (args, context): Promise<ToolResult> => {
        // GPT-family models send empty strings / empty arrays / empty objects
        // instead of omitting optional params. Use `isEmptyParam` so e.g.
        // `targets: []` or `url: ""` don't trigger mutual-exclusion errors
        // against fields the agent didn't actually intend to provide.
        // `targets` also accepts nested object/array shapes. Only treat
        // `targets` as not-provided when EVERY entry is fully empty
        // (`[{filePath: "", symbol: ""}]`, `{filePath: "", symbol: ""}`)
        // — that's the GPT-class "I didn't intend this param" signal.
        // If any entry has even one non-empty field, the agent intends
        // targets mode; let the per-entry validation below surface the
        // specific error ("targets[0].filePath must be non-empty" etc).
        const hasTargetsProvided = (t: unknown): boolean => {
          if (isEmptyParam(t)) return false;
          const entryEmpty = (entry: unknown): boolean => {
            if (!entry || typeof entry !== "object") return true;
            const fp = (entry as { filePath?: unknown }).filePath;
            const sym = (entry as { symbol?: unknown }).symbol;
            const fpEmpty = typeof fp !== "string" || fp.length === 0;
            const symEmpty = typeof sym !== "string" || sym.length === 0;
            return fpEmpty && symEmpty;
          };
          if (Array.isArray(t)) return !t.every(entryEmpty);
          return !entryEmpty(t);
        };
        const hasFilePath = !isEmptyParam(args.filePath);
        const hasUrl = !isEmptyParam(args.url);
        const hasTargets = hasTargetsProvided(args.targets);
        const hasSymbols = !isEmptyParam(args.symbols);
        // Coerce at the boundary: stringified "true" must request callgraph (coerceBoolean).
        const wantCallgraph = coerceBoolean(args.callgraph);

        // TUI title + scalar metadata for the tool-call header. OpenCode's UI
        // only auto-renders SCALAR args (strings, numbers, booleans) — arrays
        // and objects are dropped from the `[key=value, ...]` line. Stringify
        // collection-shaped args here so `targets`/`symbols` stay visible.
        // Attached to every successful return via `withMeta`. (Error paths
        // can't carry a title: OpenCode skips `tool.execute.after` when execute
        // throws, and the plugin `context.metadata()` callback is unbridged, so
        // the return value is the only channel that survives.)
        const zoomTitle = buildZoomTitle(args);
        const zoomDisplay: Record<string, unknown> = { title: zoomTitle };
        if (hasFilePath) zoomDisplay.filePath = args.filePath;
        if (hasUrl) zoomDisplay.url = args.url;
        if (hasSymbols) {
          zoomDisplay.symbols =
            typeof args.symbols === "string" ? args.symbols : JSON.stringify(args.symbols);
        }
        if (hasTargets) zoomDisplay.targets = JSON.stringify(args.targets);
        if (args.contextLines !== undefined) zoomDisplay.contextLines = args.contextLines;
        if (wantCallgraph) zoomDisplay.callgraph = true;
        const withMeta = (output: string): ToolResult => ({
          output,
          title: zoomTitle,
          metadata: zoomDisplay,
        });

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
          const resolvedTargets = await Promise.all(
            targets.map((t) => resolvePathArg(ctx, context, t.filePath)),
          );
          const permissionDenied = await assertPathExternalPermissions(
            ctx,
            context,
            resolvedTargets,
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);

          const responses = await Promise.all(
            targets.map((t, index) => {
              const params: Record<string, unknown> = {
                file: resolvedTargets[index],
                symbol: t.symbol,
              };
              if (args.contextLines !== undefined) params.context_lines = args.contextLines;
              if (wantCallgraph) params.callgraph = true;
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
          return withMeta(formatZoomMultiTargetResult(entries).text);
        }

        if (!hasFilePath && !hasUrl) {
          throw new Error("Provide exactly one of 'filePath', 'url', or 'targets'");
        }
        if (hasFilePath && hasUrl) {
          throw new Error("Provide exactly ONE of 'filePath' or 'url' — not both");
        }

        // URL mode: pass through to Rust; Rust fetches, validates, and caches.
        const file = hasUrl
          ? (args.url as string)
          : await resolvePathArg(ctx, context, args.filePath as string);
        if (!hasUrl) {
          const permissionDenied = await assertPathExternalPermissions(ctx, context, file);
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        }

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
              if (wantCallgraph) params.callgraph = true;
              return callBridge(ctx, context, "zoom", params).catch((err) => ({
                success: false,
                message: err instanceof Error ? err.message : String(err),
              }));
            }),
          );
          if (symbolsArray.length === 1) {
            const response = results[0] ?? { success: false, message: "missing zoom response" };
            const rustBatch = unwrapRustZoomBatchEnvelope(response as Record<string, unknown>);
            if (rustBatch) {
              const batch = formatZoomBatchResult(
                targetLabel,
                rustBatch.names,
                rustBatch.responses,
              );
              return withMeta(batch.text);
            }
            if ((response as { success?: boolean }).success === false) {
              throw new Error(
                ((response as { message?: string }).message as string) || "zoom failed",
              );
            }
            return withMeta(formatZoomText(targetLabel, response as Record<string, unknown>));
          }
          return withMeta(formatZoomBatchResult(targetLabel, symbolsArray, results).text);
        }

        // No symbols specified: zoom by line-range fallback (or whole file).
        const params: Record<string, unknown> = { file };
        if (args.contextLines !== undefined) params.context_lines = args.contextLines;
        if (wantCallgraph) params.callgraph = true;

        const data = await callBridge(ctx, context, "zoom", params);
        if (data.success === false) {
          throw new Error((data.message as string) || "zoom failed");
        }
        return withMeta(formatZoomText(targetLabel, data));
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

async function assertPathExternalPermissions(
  ctx: PluginContext,
  context: ToolContext,
  target: string | string[],
  kind: "file" | "directory" = "file",
): Promise<string | undefined> {
  const targets = Array.isArray(target) ? target : [target];
  const checked = new Set<string>();

  for (const resolvedPath of targets) {
    if (typeof resolvedPath !== "string" || resolvedPath.length === 0) continue;
    const key = `${kind}:${resolvedPath}`;
    if (checked.has(key)) continue;
    checked.add(key);

    const denial = await assertExternalDirectoryPermission(ctx, context, resolvedPath, { kind });
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
