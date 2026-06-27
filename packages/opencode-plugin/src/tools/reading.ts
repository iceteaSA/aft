import { coerceBoolean, coerceTargetParam, formatZoomText } from "@cortexkit/aft-bridge";
import type { ToolContext, ToolDefinition, ToolResult } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import {
  callToolCall,
  coerceOptionalInt,
  isEmptyParam,
  optionalInt,
  resolvePathArg,
} from "./_shared.js";
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
        "For understanding a specific feature, prefer aft_search + aft_zoom on named symbols; use aft_outline on a whole directory only for high-level structure mapping. aft_zoom with `callgraph:true` gives one-level forward calls-out; use aft_callgraph only for reverse callers or multi-level traces.\n\n" +
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
        includeTests: z
          .boolean()
          .optional()
          .describe(
            "Directory outline only: include test files. Defaults to false; tests are hidden.",
          ),
      },
      execute: async (args, context): Promise<string> => {
        // Coerce at the boundary: a host may deliver the string|array `target` as
        // a JSON-stringified array, which would otherwise be treated as one
        // literal path (coerceTargetParam). And a stringified "true" must enable
        // files mode (coerceBoolean).
        const target = coerceTargetParam(args.target);
        const filesMode = coerceBoolean(args.files);
        const hasIncludeTests = !isEmptyParam(args.includeTests);
        const includeTests = coerceBoolean(args.includeTests);
        const rawArgs: Record<string, unknown> = {
          target,
          ...(filesMode ? { files: true } : {}),
          ...(hasIncludeTests ? { includeTests } : {}),
        };

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
            filesMode ? "directory" : "file",
          );
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        } else {
          if (typeof target !== "string" || target.length === 0) {
            throw new Error("'target' must be a non-empty string or array of strings");
          }

          const hasUrl =
            !filesMode && (target.startsWith("http://") || target.startsWith("https://"));
          if (!hasUrl) {
            const resolvedTarget = await resolvePathArg(ctx, context, target);
            const permissionDenied = await assertPathExternalPermissions(
              ctx,
              context,
              resolvedTarget,
              await permissionKindForPath(resolvedTarget),
            );
            if (permissionDenied) return permissionDeniedResponse(permissionDenied);
          }
        }

        const response = await callToolCall(ctx, context, "outline", rawArgs);
        if (response.success === false) {
          throw new Error((response.message as string) || "outline failed");
        }
        return response.text;
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
        const contextLines = coerceOptionalInt(
          args.contextLines,
          "contextLines",
          1,
          Number.MAX_SAFE_INTEGER,
        );

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
        if (contextLines !== undefined) zoomDisplay.contextLines = contextLines;
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

          const rawArgs: Record<string, unknown> = { targets };
          if (contextLines !== undefined) rawArgs.contextLines = contextLines;
          if (wantCallgraph) rawArgs.callgraph = true;

          const response = await callToolCall(ctx, context, "zoom", rawArgs);
          if (response.success === false) {
            throw new Error(response.text || response.message || "zoom failed");
          }
          return withMeta(response.text);
        }

        if (!hasFilePath && !hasUrl) {
          throw new Error("Provide exactly one of 'filePath', 'url', or 'targets'");
        }
        if (hasFilePath && hasUrl) {
          throw new Error("Provide exactly ONE of 'filePath' or 'url' — not both");
        }

        // URL mode passes through to Rust; Rust fetches, validates, and caches.
        // File mode still resolves locally before dispatch so external-directory
        // permission checks approve the same path the server will read.
        if (!hasUrl) {
          const file = await resolvePathArg(ctx, context, args.filePath as string);
          const permissionDenied = await assertPathExternalPermissions(ctx, context, file);
          if (permissionDenied) return permissionDeniedResponse(permissionDenied);
        }

        const rawArgs: Record<string, unknown> = hasUrl
          ? { url: args.url }
          : { filePath: args.filePath };
        if (hasSymbols) rawArgs.symbols = args.symbols;
        if (contextLines !== undefined) rawArgs.contextLines = contextLines;
        if (wantCallgraph) rawArgs.callgraph = true;

        const response = await callToolCall(ctx, context, "zoom", rawArgs);
        if (response.success === false) {
          throw new Error(response.text || response.message || "zoom failed");
        }
        return withMeta(response.text);
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

async function permissionKindForPath(resolvedPath: string): Promise<"file" | "directory"> {
  try {
    const { stat } = await import("node:fs/promises");
    const st = await stat(resolvedPath);
    return st.isDirectory() ? "directory" : "file";
  } catch {
    // If stat fails, keep the tool call moving so the server can report the
    // real path error. Use a file label because a missing path cannot be a
    // directory that the permission prompt could grant.
    return "file";
  }
}

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
