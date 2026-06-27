import * as fs from "node:fs";
import * as path from "node:path";
import type { ToolContext } from "@opencode-ai/plugin";

import { sendIgnoredMessage } from "../shared/ignored-message.js";
import type { PluginContext } from "../types.js";
import { expandTilde, projectRootFor } from "./_shared.js";

const UNSUPPORTED_ASK_HOST =
  "AFT requires OpenCode 1.15.5 or newer for permission asks; please upgrade OpenCode";

/**
 * Throttle for the user-facing "restrict_to_project_root blocked this" panel.
 * An agent that probes several external paths shouldn't spawn a panel per path;
 * we surface the notice at most once per session per 5 minutes. Keyed by
 * sessionID. Module-level is acceptable here: this is best-effort UI-noise
 * suppression, not correctness — a duplicate plugin load worst-cases to one
 * extra panel, never a missed block (the block + agent denial are independent
 * of this map).
 */
const RESTRICT_NOTICE_THROTTLE_MS = 5 * 60 * 1000;
const restrictNoticeLastSentAt = new Map<string, number>();

function restrictNoticeWording(target: string): string {
  return (
    `AFT blocked access to a path outside the project:\n  ${target}\n` +
    "`restrict_to_project_root` is enabled (full isolation), so AFT does not access paths " +
    "outside the project root. To allow external paths, set `restrict_to_project_root: false` " +
    "in your aft.jsonc."
  );
}

/**
 * Fire the throttled restriction notice to the user (ignored panel, no agent
 * turn). Best-effort: never throws into the caller's tool path.
 */
function notifyRestrictBlocked(ctx: PluginContext, context: ToolContext, target: string): void {
  const sessionID = context.sessionID;
  if (!sessionID) return;
  const now = Date.now();
  const last = restrictNoticeLastSentAt.get(sessionID);
  if (last !== undefined && now - last < RESTRICT_NOTICE_THROTTLE_MS) return;
  restrictNoticeLastSentAt.set(sessionID, now);
  void sendIgnoredMessage(ctx.client, sessionID, restrictNoticeWording(target)).catch(() => {
    // UI-only notice; a delivery failure must not affect the block itself.
  });
}

/** Agent-facing denial returned when restrict_to_project_root blocks a path. */
function restrictDenialMessage(target: string): string {
  return (
    `Blocked: '${target}' is outside the project root and restrict_to_project_root is enabled ` +
    "(AFT full isolation). Not overridable per-call; set restrict_to_project_root: false in " +
    "aft.jsonc to allow external paths."
  );
}

/**
 * Execute a `ctx.ask(...)` result.
 *
 * As of `@opencode-ai/plugin@1.15.5`, `ask()` returns `Promise<void>` again
 * (it briefly returned `Effect.Effect<void>` in 1.14.x–1.15.4; the Promise
 * shape is what the SDK originally used and what AFT supports today).
 *
 * On deny, the Promise rejects with `DeniedError` / `RejectedError`, so
 * callers can rely on a normal `try/catch` to detect denial. This helper
 * stays as a single chokepoint so that if the SDK ever changes its return
 * shape again, only this function needs to be touched.
 */
export async function runAsk(maybe: Promise<void>): Promise<void> {
  await maybe;
}

export function resolveAbsolutePath(context: ToolContext, target: string): string {
  const expanded = expandTilde(target);
  return path.isAbsolute(expanded) ? expanded : path.resolve(projectRootFor(context), expanded);
}

export function resolveRelativePattern(context: ToolContext, target: string): string {
  return path.relative(projectRootFor(context), resolveAbsolutePath(context, target)) || ".";
}

export function resolveRelativePatternFromAbsolute(
  context: ToolContext,
  absolutePath: string,
): string {
  return path.relative(projectRootFor(context), absolutePath) || ".";
}

export function resolveRelativePatterns(context: ToolContext, targets: string[]): string[] {
  const seen = new Set<string>();
  const patterns: string[] = [];

  for (const target of targets) {
    if (!target) continue;
    const pattern = resolveRelativePattern(context, target);
    if (seen.has(pattern)) continue;
    seen.add(pattern);
    patterns.push(pattern);
  }

  return patterns;
}

export function workspacePattern(_context: ToolContext): string {
  return ".";
}

export async function askEditPermission(
  context: ToolContext,
  patterns: string[],
  metadata: Record<string, unknown> = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission: "edit",
        patterns: patterns.length > 0 ? patterns : [workspacePattern(context)],
        always: ["*"],
        metadata,
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied.";
  }
}

/**
 * Check if `child` is inside `parent`. Mirrors `AppFileSystem.contains` in
 * opencode core (uses `path.relative` and ensures it doesn't start with `..`).
 */
function containsPath(parent: string, child: string): boolean {
  if (!parent) return false;
  const rel = path.relative(parent, child);
  return rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
}

/**
 * Convert POSIX-style drive paths to Windows drive paths.
 *
 * Mirrors `AppFileSystem.windowsPath` in opencode core — these forms can
 * leak into our input from Git Bash, Cygwin, and WSL conversions:
 *
 *   `/c/Users/...`         → `C:/Users/...`
 *   `/cygdrive/c/...`      → `C:/...`
 *   `/mnt/c/...`           → `C:/...`
 *
 * No-op on non-Windows.
 */
function windowsPath(p: string): string {
  if (process.platform !== "win32") return p;
  return p
    .replace(/^\/([a-zA-Z]):(?:[\\/]|$)/, (_, drive) => `${drive.toUpperCase()}:/`)
    .replace(/^\/([a-zA-Z])(?:\/|$)/, (_, drive) => `${drive.toUpperCase()}:/`)
    .replace(/^\/cygdrive\/([a-zA-Z])(?:\/|$)/, (_, drive) => `${drive.toUpperCase()}:/`)
    .replace(/^\/mnt\/([a-zA-Z])(?:\/|$)/, (_, drive) => `${drive.toUpperCase()}:/`);
}

/**
 * Resolve symlinks before containsPath() comparisons on every platform.
 *
 * Existing targets are canonicalized directly. For not-yet-created write
 * targets, walk upward until an existing parent can be realpath'd, then rejoin
 * the missing tail; this catches writes through a symlinked parent directory.
 * Windows drive/path normalization is applied before realpath so drive-form
 * variants still compare consistently. This helper is total: permission checks
 * must degrade to a lexical absolute path instead of throwing.
 */
function normalizePath(p: string): string {
  const resolved = path.resolve(windowsPath(p));
  try {
    return fs.realpathSync.native(resolved);
  } catch {
    return normalizeNearestExistingParent(resolved);
  }
}

function normalizeNearestExistingParent(resolved: string): string {
  const missingTail: string[] = [];
  let current = resolved;

  while (true) {
    try {
      const realParent = fs.realpathSync.native(current);
      return missingTail.length === 0
        ? realParent
        : path.join(realParent, ...missingTail.reverse());
    } catch {
      const parent = path.dirname(current);
      if (parent === current) return resolved;
      missingTail.push(path.basename(current));
      current = parent;
    }
  }
}

/**
 * Normalize a path pattern (which may end in `*`) for the same reasons
 * normalizePath() exists, but without trying to realpath a pattern that
 * doesn't correspond to a real entry.
 *
 * Mirrors `AppFileSystem.normalizePathPattern` in opencode core.
 *
 *   `*`                 → `*`
 *   `~/projects/*`      → `~/projects/*`  (`~` is expanded by opencode's matcher)
 *   `C:\some\dir\*`     → `C:\some\dir\*` (drive case canonicalized via realpath of the dir part)
 *
 * Non-Windows callers build patterns from an already-canonical parent path.
 */
function normalizePathPattern(p: string): string {
  if (process.platform !== "win32") return p;
  if (p === "*" || p === "**") return p;
  const match = p.match(/^(.*)[\\/](\*{1,2})$/);
  if (!match) return normalizePath(p);
  const dir = /^[A-Za-z]:$/.test(match[1]) ? `${match[1]}\\` : match[1];
  return path.join(normalizePath(dir), match[2]);
}

export const _permissionsInternalsForTest = { containsPath, normalizePathPattern };

/**
 * Trigger OpenCode's host-side `external_directory` permission check when the
 * target path falls outside the current project's directory and worktree.
 * Mirrors `opencode/src/tool/external-directory.ts::assertExternalDirectoryEffect`.
 *
 * Why this exists: AFT hoisted tools previously only called `permission: "edit"`,
 * which bypassed OpenCode's separate `external_directory` rule (default `ask`).
 * That meant `/tmp/anything` writes routed through AFT silently bypassed the
 * prompt OpenCode native `write`/`edit`/`apply_patch`/`read` show. This helper
 * closes that gap so AFT's hoisted surface matches native behavior.
 *
 * Returns `undefined` on allow (or when target is inside project), or a
 * denial message string on deny so callers can wrap with
 * `permissionDeniedResponse(...)`.
 *
 * Always call this BEFORE the regular `askEditPermission` so the user sees the
 * external-directory prompt first (matching opencode native ordering). When the
 * external-directory rule is `allow` (e.g. for `${os.tmpdir()}/opencode/*`), the
 * call short-circuits and the regular permission flow continues normally.
 */
export async function assertExternalDirectoryPermission(
  ctx: PluginContext,
  context: ToolContext,
  target: string,
  options?: { kind?: "file" | "directory" },
): Promise<string | undefined> {
  if (!target) return undefined;

  const resolved = resolveAbsolutePath(context, target);
  // `tool_call` trusts this plugin-side permission decision. The Rust default
  // validator may keep lexical path strings, so canonicalize containment here
  // and assert the ask decision rather than plugin/server string equality.
  const absoluteTarget = normalizePath(resolved);

  const root = projectRootFor(context);
  const directory = root ? normalizePath(root) : root;
  const rawWorktree = (context as { worktree?: string }).worktree;
  const worktree = rawWorktree && rawWorktree !== "/" ? normalizePath(rawWorktree) : rawWorktree;

  if (directory && containsPath(directory, absoluteTarget)) return undefined;
  // Non-git projects set worktree to "/" which matches ANY absolute path.
  // Match opencode's behavior: skip the worktree check in that case so we
  // still ask for external paths.
  if (
    worktree &&
    worktree !== "/" &&
    worktree !== directory &&
    containsPath(worktree, absoluteTarget)
  ) {
    return undefined;
  }

  // restrict_to_project_root is AFT's full-isolation knob — deliberately NOT
  // conflated with OpenCode's external_directory permission. When it's on, an
  // out-of-root path is hard-blocked at the plugin layer: we do NOT bubble an
  // external_directory prompt (a grant could never override the Rust-side
  // boundary anyway — that produced the issue #125 "approved but still fails"
  // footgun). Instead the agent gets a clear denial and the user gets a
  // throttled informational panel explaining the restriction.
  if (ctx.config.restrict_to_project_root === true) {
    notifyRestrictBlocked(ctx, context, absoluteTarget);
    return restrictDenialMessage(absoluteTarget);
  }

  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;

  const kind = options?.kind ?? "file";
  const parentDir = kind === "directory" ? absoluteTarget : path.dirname(absoluteTarget);
  const rawGlob =
    process.platform === "win32"
      ? normalizePathPattern(path.join(parentDir, "*"))
      : path.join(parentDir, "*").replaceAll("\\", "/");

  try {
    await runAsk(
      context.ask({
        permission: "external_directory",
        patterns: [rawGlob],
        always: [rawGlob],
        metadata: {
          filepath: absoluteTarget,
          parentDir,
        },
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied (external directory).";
  }
}

/**
 * Trigger OpenCode's host-side `grep` permission check.
 *
 * Mirrors `opencode/src/tool/grep.ts` shape exactly so users with
 * `"permission": { "grep": { "*": "ask" } }` (or "deny") see the same
 * prompt regardless of whether they're using AFT's hoisted `grep` or
 * OpenCode's built-in.
 */
export async function askGrepPermission(
  context: ToolContext,
  pattern: string,
  metadata: { path?: string; include?: string } = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission: "grep",
        patterns: [pattern],
        always: ["*"],
        metadata: { pattern, ...metadata },
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied (grep).";
  }
}

/**
 * Trigger OpenCode's host-side `glob` permission check.
 *
 * Mirrors `opencode/src/tool/glob.ts` shape exactly so users with
 * `"permission": { "glob": { "*": "ask" } }` see the same prompt
 * regardless of which glob tool is used.
 */
export async function askGlobPermission(
  context: ToolContext,
  pattern: string,
  metadata: { path?: string } = {},
): Promise<string | undefined> {
  if (typeof context.ask !== "function") return UNSUPPORTED_ASK_HOST;
  try {
    await runAsk(
      context.ask({
        permission: "glob",
        patterns: [pattern],
        always: ["*"],
        metadata: { pattern, ...metadata },
      }),
    );
    return undefined;
  } catch (error) {
    if (error instanceof Error && error.message) {
      return error.message;
    }
    return "Permission denied (glob).";
  }
}

export function permissionDeniedResponse(message: string): string {
  return JSON.stringify({
    success: false,
    code: "permission_denied",
    message,
    error: message,
  });
}
