import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import type { ToolContext, ToolDefinition } from "@opencode-ai/plugin";
import { z } from "zod";
import type { PluginContext } from "../types.js";
import { callBridge } from "./_shared.js";
import {
  askGlobPermission,
  askGrepPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
} from "./permissions.js";

/**
 * Expand a leading `~` to the user's home directory. Mirrors shell-style
 * expansion so agent calls like `grep ... in ~/Work/...` resolve before
 * any permission check or bridge call sees the literal tilde. Required
 * because Node's `path.resolve` treats `~` as a literal directory name,
 * so `~/foo` ends up resolved to `<cwd>/~/foo`.
 */
function expandTilde(input: string): string {
  if (!input || !input.startsWith("~")) return input;
  if (input === "~") return os.homedir();
  if (input.startsWith("~/") || input.startsWith(`~${path.sep}`)) {
    return path.resolve(os.homedir(), input.slice(2));
  }
  return input;
}

type ToolArg = ToolDefinition["args"][string];
type SearchPathKind = "file" | "directory";
type SearchPathTarget = { target: string; kind: SearchPathKind };

type GrepMatch = {
  file?: string;
  line?: number;
  line_text?: string;
  text?: string;
};

type GrepResponse = {
  text?: string;
  matches?: GrepMatch[];
  total_matches?: number;
  files_with_matches?: number;
};

function arg(schema: unknown): ToolArg {
  return schema as ToolArg;
}

function formatGrepOutput(response: GrepResponse): string {
  if (typeof response.text === "string") {
    return response.text;
  }

  const matches = Array.isArray(response.matches) ? response.matches : [];
  const totalMatches = response.total_matches ?? matches.length;
  const filesWithMatches = response.files_with_matches ?? new Set(matches.map((m) => m.file)).size;

  if (matches.length === 0) {
    return `Found ${totalMatches} match(es) in ${filesWithMatches} file(s).`;
  }

  const body = matches
    .map((match) => {
      const file = match.file ?? "unknown";
      const line = match.line ?? 0;
      const text = match.line_text ?? match.text ?? "";
      return `${file}:${line}: ${text}`;
    })
    .join("\n");

  return `${body}\n\nFound ${totalMatches} match(es) in ${filesWithMatches} file(s).`;
}

/** Ensure glob patterns match files in subdirectories — prefix with **\/ if no path separator. */
function normalizeGlob(pattern: string): string {
  if (!pattern.includes("/") && !pattern.startsWith("**/")) {
    return `**/${pattern}`;
  }
  return pattern;
}

function absoluteSearchPath(context: ToolContext, target: string): string {
  const expanded = expandTilde(target);
  return path.isAbsolute(expanded) ? expanded : path.resolve(context.directory, expanded);
}

function searchPathExists(context: ToolContext, target: string): boolean {
  return fs.existsSync(absoluteSearchPath(context, target));
}

function splitSearchPathArg(context: ToolContext, raw: string): string[] {
  if (searchPathExists(context, raw) || !/\s/.test(raw)) {
    return [raw];
  }

  const fragments = raw.trim().split(/\s+/).filter(Boolean);
  if (fragments.length < 2 || !fragments.every((fragment) => searchPathExists(context, fragment))) {
    return [raw];
  }

  return fragments;
}

function searchPathKind(
  context: ToolContext,
  target: string,
  defaultKind: SearchPathKind,
): SearchPathKind {
  try {
    const stat = fs.lstatSync(absoluteSearchPath(context, target));
    if (defaultKind === "file") {
      return stat.isDirectory() ? "directory" : "file";
    }
    return stat.isFile() ? "file" : "directory";
  } catch {
    return defaultKind;
  }
}

function searchPathTargets(
  context: ToolContext,
  raw: string,
  defaultKind: SearchPathKind,
): SearchPathTarget[] {
  return splitSearchPathArg(context, raw).map((target) => ({
    target,
    kind: searchPathKind(context, target, defaultKind),
  }));
}

/**
 * Brace-aware comma split. Allows users to type either of:
 *
 *   - "*.ts,*.tsx"            (multiple OpenCode-style includes)
 *   - "**\/*.{vue,ts,tsx}"    (a single glob with a brace alternation)
 *   - "*.ts,**\/*.{vue,tsx}"  (mix of both)
 *
 * Without brace awareness the naive `String#split(",")` chops the brace
 * group apart and the resulting `**\/*.{vue` glob fails parsing in
 * ripgrep / globset with `unclosed alternate group; missing '}'`.
 */
export function splitIncludeArg(raw: string): string[] {
  const out: string[] = [];
  let depth = 0;
  let buf = "";
  for (const ch of raw) {
    if (ch === "{") {
      depth++;
      buf += ch;
      continue;
    }
    if (ch === "}") {
      if (depth > 0) depth--;
      buf += ch;
      continue;
    }
    if (ch === "," && depth === 0) {
      const trimmed = buf.trim();
      if (trimmed.length > 0) out.push(trimmed);
      buf = "";
      continue;
    }
    buf += ch;
  }
  const tail = buf.trim();
  if (tail.length > 0) out.push(tail);
  return out;
}

/**
 * Tool definitions for indexed search-backed grep and glob.
 */
export function searchTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const grepTool: ToolDefinition = {
    description:
      "Search file contents using regular expressions. Returns matching lines with file paths and line numbers (no surrounding context lines — use `read` for that). Always case-sensitive. Capped at 100 matches; if you hit the cap, narrow with `path` or `include` and re-run.",
    args: {
      pattern: arg(z.string().describe("Regular expression pattern to search for")),
      include: arg(
        z.string().optional().describe("File pattern to include (e.g. '*.ts', '*.{ts,tsx}')"),
      ),
      path: arg(
        z
          .string()
          .optional()
          .describe("Directory to search (absolute or relative to project root)"),
      ),
    },
    execute: async (args, context): Promise<string> => {
      const pattern = String(args.pattern);
      const includeArg = args.include ? String(args.include) : undefined;
      const pathArg = args.path ? expandTilde(String(args.path)) : undefined;

      // Match OpenCode native ordering: grep permission first (on the raw
      // pattern + path the agent typed), then external_directory check on
      // the resolved search target if it points outside the project.
      const grepDenied = await askGrepPermission(context, pattern, {
        path: pathArg,
        include: includeArg,
      });
      if (grepDenied) return permissionDeniedResponse(grepDenied);

      if (pathArg) {
        for (const target of searchPathTargets(context, pathArg, "file")) {
          const externalDenied = await assertExternalDirectoryPermission(context, target.target, {
            kind: target.kind,
          });
          if (externalDenied) return permissionDeniedResponse(externalDenied);
        }
      }

      const response = await callBridge(ctx, context, "grep", {
        pattern,
        case_sensitive: true,
        include: includeArg
          ? splitIncludeArg(includeArg).map(normalizeGlob).filter(Boolean)
          : undefined,
        path: pathArg,
        max_results: 100,
      });

      if (response.success === false) {
        throw new Error((response.message as string) || "grep failed");
      }

      return formatGrepOutput(response as GrepResponse);
    },
  };

  const globTool: ToolDefinition = {
    description:
      "Find files matching a glob pattern. Returns matching file paths sorted by modification time.",
    args: {
      pattern: arg(
        z.string().describe("Glob pattern to match (e.g. '**/*.ts', 'src/**/*.test.*')"),
      ),
      path: arg(
        z
          .string()
          .optional()
          .describe("Directory to search (absolute or relative to project root)"),
      ),
    },
    execute: async (args, context): Promise<string> => {
      // Handle absolute paths embedded in the pattern (e.g. "/abs/path/src/**/*.ts")
      // Split into path (directory prefix) and pattern (glob suffix)
      let globPattern = expandTilde(String(args.pattern));
      let globPath = args.path ? expandTilde(String(args.path)) : undefined;

      if (!globPath && globPattern.startsWith("/")) {
        // Find the last directory component before any glob metacharacters.
        // Exact absolute paths need the same split because the bridge matches
        // glob patterns relative to the search path.
        const metaIdx = globPattern.search(/[*?{}[\]]/);
        if (metaIdx > 0) {
          const lastSlash = globPattern.lastIndexOf("/", metaIdx);
          if (lastSlash > 0) {
            globPath = globPattern.slice(0, lastSlash);
            globPattern = `**/${globPattern.slice(lastSlash + 1)}`;
          }
        } else if (metaIdx === -1) {
          globPath = path.dirname(globPattern);
          globPattern = path.basename(globPattern);
        }
      }

      // Match OpenCode native ordering: glob permission first, then
      // external_directory check on the resolved search root if it's
      // outside the project.
      const globDenied = await askGlobPermission(context, globPattern, { path: globPath });
      if (globDenied) return permissionDeniedResponse(globDenied);

      if (globPath) {
        for (const target of searchPathTargets(context, globPath, "directory")) {
          const externalDenied = await assertExternalDirectoryPermission(context, target.target, {
            kind: target.kind,
          });
          if (externalDenied) return permissionDeniedResponse(externalDenied);
        }
      }

      const response = await callBridge(ctx, context, "glob", {
        pattern: globPattern,
        path: globPath,
      });

      if (response.success === false) {
        throw new Error((response.message as string) || "glob failed");
      }

      if (typeof response.text === "string") {
        return response.text;
      }

      if (Array.isArray(response.files)) {
        return response.files.join("\n");
      }

      return (response.text as string) || JSON.stringify(response);
    },
  };

  const hoisting = ctx.config.hoist_builtin_tools !== false;
  return {
    [hoisting ? "grep" : "aft_grep"]: grepTool,
    [hoisting ? "glob" : "aft_glob"]: globTool,
  };
}
