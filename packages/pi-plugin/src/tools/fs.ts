/**
 * aft_delete + aft_move — filesystem ops with per-file backup.
 * Both go through Rust so backups and checkpoint rollback work the same way.
 */

import { coerceBoolean, coerceStringArray } from "@cortexkit/aft-bridge";
import type { AgentToolResult, ExtensionAPI, Theme } from "@earendil-works/pi-coding-agent";
import { type Static, Type } from "typebox";
import type { PluginContext } from "../types.js";
import { bridgeFor, callBridge, textResult } from "./_shared.js";
import { assertExternalDirectoryPermission, resolvePathArg } from "./hoisted.js";
import {
  accentPath,
  type RenderContextLike,
  renderErrorResult,
  renderSections,
  renderToolCall,
  shortenPath,
} from "./render-helpers.js";

const DeleteParams = Type.Object({
  files: Type.Array(Type.String(), {
    description: "Paths to delete (one or more). May include directories when recursive=true.",
    minItems: 1,
  }),
  recursive: Type.Optional(
    Type.Boolean({
      description:
        "Required to delete a directory and its contents. Defaults to false; passing a directory without this returns an error.",
    }),
  ),
});

const MoveParams = Type.Object({
  filePath: Type.String({
    description: "Source file path to move (absolute or relative to project root)",
  }),
  destination: Type.String({
    description: "Destination file path (absolute or relative to project root)",
  }),
});

export interface FsSurface {
  delete: boolean;
  move: boolean;
}

/** Exported for renderer unit tests. */
export function renderFsCall(
  toolName: "aft_delete" | "aft_move",
  args: Static<typeof DeleteParams> | Static<typeof MoveParams>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (toolName === "aft_delete") {
    const files = (args as Static<typeof DeleteParams>).files;
    const summary =
      files.length === 1
        ? accentPath(theme, files[0])
        : `${theme.fg("accent", String(files.length))} ${theme.fg("muted", "files")}`;
    return renderToolCall("delete", summary, theme, context);
  }

  const moveArgs = args as Static<typeof MoveParams>;
  return renderToolCall(
    "move",
    `${accentPath(theme, moveArgs.filePath)} ${theme.fg("muted", "→")} ${accentPath(theme, moveArgs.destination)}`,
    theme,
    context,
  );
}

/** Exported for renderer unit tests. */
export function renderFsResult(
  toolName: "aft_delete" | "aft_move",
  args: Static<typeof DeleteParams> | Static<typeof MoveParams>,
  result: AgentToolResult<unknown>,
  theme: Theme,
  context: RenderContextLike,
) {
  if (context.isError) {
    return renderErrorResult(result, `${toolName} failed`, theme, context);
  }

  if (toolName === "aft_delete") {
    const files = (args as Static<typeof DeleteParams>).files;
    const data = (result?.details ?? {}) as {
      deleted?: string[];
      skipped_files?: Array<{ file: string; reason: string }>;
      complete?: boolean;
    };
    const deletedPaths = data.deleted ?? files;
    const skipped = data.skipped_files ?? [];
    const lines: string[] = [];
    for (const entry of deletedPaths) {
      lines.push(`${theme.fg("success", "✓ deleted")} ${theme.fg("accent", shortenPath(entry))}`);
    }
    for (const entry of skipped) {
      lines.push(
        `${theme.fg("error", "✗ skipped")} ${theme.fg("accent", shortenPath(entry.file))} ${theme.fg("muted", `(${entry.reason})`)}`,
      );
    }
    if (lines.length === 0) {
      lines.push(theme.fg("muted", "(no files deleted)"));
    }
    return renderSections([lines.join("\n")], context);
  }

  const moveArgs = args as Static<typeof MoveParams>;
  return renderSections(
    [
      `${theme.fg("success", "✓ moved")} ${theme.fg("accent", shortenPath(moveArgs.filePath))}`,
      `${theme.fg("muted", "to")} ${theme.fg("accent", shortenPath(moveArgs.destination))}`,
    ],
    context,
  );
}

export function registerFsTools(pi: ExtensionAPI, ctx: PluginContext, surface: FsSurface): void {
  const backupsDisabled = ctx.config.backup?.enabled === false;
  if (surface.delete) {
    pi.registerTool({
      name: "aft_delete",
      label: "delete",
      description:
        "Delete one or more files (or directories). " +
        (backupsDisabled
          ? "Backup capture is disabled by user config, so this tool does not create undo snapshots. "
          : "Each file is backed up before deletion — use `aft_safety undo` to recover any of them. For directories, every file inside is individually backed up before removal. ") +
        "Directory deletion requires recursive: true. " +
        "Returns { success, complete, deleted, skipped_files }: partial success is allowed; files that fail are reported in skipped_files.",
      parameters: DeleteParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof DeleteParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        // Coerce at the boundary: some hosts deliver `files` as a bare string
        // or a JSON-stringified array despite the schema, which would crash the
        // unchecked `.map` below before any validation runs.
        const inputs = coerceStringArray(params.files);
        if (inputs.length === 0) {
          throw new Error("delete: `files` must be a non-empty array of paths");
        }
        const files = await Promise.all(inputs.map((file) => resolvePathArg(extCtx.cwd, file)));
        const checked = new Set<string>();
        for (const file of files) {
          if (checked.has(file)) continue;
          checked.add(file);
          await assertExternalDirectoryPermission(extCtx, file, {
            restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
          });
        }

        const bridge = bridgeFor(ctx, extCtx.cwd);
        // Single batched call so every file shares one op_id; one
        // `aft_safety undo` then restores the whole delete atomically.
        const response = await callBridge(
          bridge,
          "delete_file",
          {
            files,
            // Coerce at the boundary, like `files`: a stringified "true" from the
            // model must not silently drop the flag (see coerceBoolean).
            recursive: coerceBoolean(params.recursive),
          },
          extCtx,
        );
        const deletedEntries = (response.deleted as Array<{ file: string }> | undefined) ?? [];
        const skipped =
          (response.skipped_files as Array<{ file: string; reason: string }> | undefined) ?? [];
        const deleted = deletedEntries.map((entry) => entry.file);
        // Refuse a fully-failed batch with an error so renderers don't show
        // "completed" for nothing-actually-deleted.
        if (deleted.length === 0 && skipped.length > 0) {
          throw new Error(
            `delete failed for all ${skipped.length} file(s):\n` +
              skipped.map((entry) => `  ${entry.file}: ${entry.reason}`).join("\n"),
          );
        }
        const summary =
          deleted.length === 1 && skipped.length === 0
            ? `Deleted ${deleted[0]}`
            : `Deleted ${deleted.length}/${inputs.length} file(s)`;
        return textResult(summary, {
          success: true,
          complete: skipped.length === 0,
          deleted,
          skipped_files: skipped,
        });
      },
      renderCall(args, theme, context) {
        return renderFsCall("aft_delete", args, theme, context);
      },
      renderResult(result, _options, theme, context) {
        return renderFsResult("aft_delete", context.args, result, theme, context);
      },
    });
  }

  if (surface.move) {
    pi.registerTool({
      name: "aft_move",
      label: "move",
      description:
        "Move or rename a file. " +
        (backupsDisabled
          ? "Backup capture is disabled by user config. "
          : "Creates an undo backup before moving. ") +
        "Creates parent directories for the destination automatically. This operates on files at the OS level — to relocate a code symbol between files, use `aft_refactor` with op='move' instead.",
      parameters: MoveParams,
      async execute(
        _toolCallId: string,
        params: Static<typeof MoveParams>,
        _signal,
        _onUpdate,
        extCtx,
      ) {
        const filePath = await resolvePathArg(extCtx.cwd, params.filePath);
        const destination = await resolvePathArg(extCtx.cwd, params.destination);
        const checked = new Set([filePath, destination]);
        for (const file of checked) {
          await assertExternalDirectoryPermission(extCtx, file, {
            restrictToProjectRoot: ctx.config.restrict_to_project_root ?? false,
          });
        }

        const bridge = bridgeFor(ctx, extCtx.cwd);
        const response = await callBridge(
          bridge,
          "move_file",
          {
            file: filePath,
            destination,
          },
          extCtx,
        );
        return textResult(`Moved ${params.filePath} → ${params.destination}`, response);
      },
      renderCall(args, theme, context) {
        return renderFsCall("aft_move", args, theme, context);
      },
      renderResult(result, _options, theme, context) {
        return renderFsResult("aft_move", context.args, result, theme, context);
      },
    });
  }
}
