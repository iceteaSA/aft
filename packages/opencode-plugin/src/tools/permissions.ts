import * as path from "node:path";
import type { ToolContext } from "@opencode-ai/plugin";
import { Effect } from "effect";

/**
 * Execute a `ctx.ask(...)` result.
 *
 * Why this exists: OpenCode's plugin contract returns `Effect.Effect<void>`
 * from `ask()` (since v1.14). Plain `await effect` resolves silently to the
 * Effect object without ever executing it — meaning the deny/ask evaluation
 * never runs and the user's `bash: { "*": deny }` (and edit/external_directory)
 * rules are silently ignored. The Effect must be run via `Effect.runPromise`.
 *
 * `effect` is marked external in our bun build and listed as a peerDependency,
 * so this import resolves at runtime to the same `effect` runtime that
 * `@opencode-ai/plugin` is using to construct the Effect. Bundling our own
 * `effect` would create a runtime instance mismatch where
 * `Effect.runPromise(...)` rejects with "Not a valid effect".
 *
 * On deny, `Effect.runPromise` rejects with the underlying defect
 * (DeniedError / RejectedError) so callers can rely on `try/catch` to
 * detect denial.
 */
export async function runAsk(maybe: Effect.Effect<void>): Promise<void> {
  await Effect.runPromise(maybe);
}

export function resolveAbsolutePath(context: ToolContext, target: string): string {
  return path.isAbsolute(target) ? target : path.resolve(context.directory, target);
}

export function resolveRelativePattern(context: ToolContext, target: string): string {
  return path.relative(context.worktree, resolveAbsolutePath(context, target)) || ".";
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
  return rel === "" || !rel.startsWith("..");
}

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
  context: ToolContext,
  target: string,
  options?: { kind?: "file" | "directory" },
): Promise<string | undefined> {
  if (!target) return undefined;

  const absoluteTarget = path.isAbsolute(target) ? target : path.resolve(context.directory, target);

  const directory = context.directory;
  const worktree = (context as { worktree?: string }).worktree;
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

  const kind = options?.kind ?? "file";
  const parentDir = kind === "directory" ? absoluteTarget : path.dirname(absoluteTarget);
  const glob = path.join(parentDir, "*").replaceAll("\\", "/");

  try {
    await runAsk(
      context.ask({
        permission: "external_directory",
        patterns: [glob],
        always: [glob],
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

export function permissionDeniedResponse(message: string): string {
  return JSON.stringify({
    success: false,
    code: "permission_denied",
    message,
    error: message,
  });
}
