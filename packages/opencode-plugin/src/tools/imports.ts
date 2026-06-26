import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import type { PluginContext } from "../types.js";
import { callBridge, isEmptyParam, resolvePathArg } from "./_shared.js";
import {
  askEditPermission,
  assertExternalDirectoryPermission,
  permissionDeniedResponse,
  resolveRelativePattern,
} from "./permissions.js";

const z = tool.schema;

/**
 * Tool definitions for import management commands: add_import, remove_import, organize_imports.
 */
export function importTools(ctx: PluginContext): Record<string, ToolDefinition> {
  const organizeRecovery =
    ctx.config.backup?.enabled === false
      ? "Backup capture is disabled by user config; review broad cleanup changes before proceeding."
      : "Use aft_safety checkpoint/undo for recovery before broad cleanup.";
  return {
    aft_import: {
      description:
        "Language-aware import management. Supports TS, JS, TSX, Python, Rust, Go, Solidity, Java, C#, PHP, Kotlin, Scala, Swift, Ruby, Lua, C, C++, Perl, and Vue.\n\n" +
        "Ops:\n" +
        "- 'add': Add an import. Auto-detects group (stdlib/external/internal), deduplicates. Requires 'module'. Optional 'names', 'defaultImport', 'typeOnly'.\n" +
        "- 'remove': Remove an import or a specific named import. Requires 'module'. Provide 'removeName' to remove a single named import; omit to remove the entire import.\n" +
        `- 'organize': Re-sort and re-group all imports by language convention, deduplicate. Requires only 'filePath'. ${organizeRecovery}`,
      // Parameters are Zod-optional because different ops need different subsets.
      // Runtime guards below validate per-op requirements and give clear errors.
      args: {
        op: z.enum(["add", "remove", "organize"]).describe("Import operation"),
        filePath: z.string().describe("Path to the file (absolute or relative to project root)"),
        module: z
          .string()
          .optional()
          .describe("Module path (required for add, remove — e.g. 'react', './utils', 'std::fmt')"),
        names: z
          .array(z.string())
          .optional()
          .describe(
            "Named imports to add. Each entry uses the language's native named-import text, " +
              "including per-name aliasing where the language uses `as` (e.g. ['useState', 'debounce as db'], " +
              "Solidity ['ERC20', 'IERC20 as IToken']).",
          ),
        defaultImport: z
          .string()
          .optional()
          .describe("Default import name, ES only (e.g. 'React')"),
        namespace: z
          .string()
          .optional()
          .describe(
            "Namespace binding: `import * as ns from 'mod'` (ES), `import * as N from \"./X.sol\"` (Solidity).",
          ),
        alias: z
          .string()
          .optional()
          .describe(
            'Whole-module alias. Solidity: `import "./X.sol" as X` (module=path, alias=X).',
          ),
        modifiers: z
          .array(z.string())
          .optional()
          .describe(
            "Statement-level modifier tokens, language-validated: Java/C# 'static'; C# 'global'/'unsafe'; " +
              "Java/Kotlin/Scala 'wildcard'; Swift '@testable'. Unsupported tokens for the file's language return a clear error.",
          ),
        importKind: z
          .string()
          .optional()
          .describe(
            "Symbol-kind-specific import: PHP 'function'/'const'; Swift 'struct'/'class'/'enum'/'protocol'/'func'; Scala 'given'.",
          ),
        typeOnly: z.boolean().optional().describe("Type-only import (TS only, default: false)"),
        removeName: z
          .string()
          .optional()
          .describe("Named import to remove for 'remove' op; omit to remove entire import"),
        validate: z
          .enum(["syntax", "full"])
          .optional()
          .describe(
            "Validation level: 'syntax' (default) or 'full'. Syntax = tree-sitter parse check only. Full = also runs LSP type-checking (slower, catches more errors)",
          ),
      },
      execute: async (args, context): Promise<string> => {
        const op = args.op as string;

        if ((op === "add" || op === "remove") && isEmptyParam(args.module)) {
          throw new Error(`'module' is required for '${op}' op`);
        }

        const filePath = await resolvePathArg(ctx, context, args.filePath as string);

        // External-directory check first (mirrors opencode-native edit.ts:68).
        {
          const denial = await assertExternalDirectoryPermission(ctx, context, filePath);
          if (denial) return permissionDeniedResponse(denial);
        }

        const permissionError = await askEditPermission(
          context,
          [resolveRelativePattern(context, filePath)],
          { filepath: filePath },
        );
        if (permissionError) return permissionDeniedResponse(permissionError);

        const commandMap: Record<string, string> = {
          add: "add_import",
          remove: "remove_import",
          organize: "organize_imports",
        };
        const params: Record<string, unknown> = { file: filePath };
        if (args.module !== undefined) params.module = args.module;
        if (args.names !== undefined) params.names = args.names;
        if (args.defaultImport !== undefined) params.default_import = args.defaultImport;
        if (args.namespace !== undefined) params.namespace = args.namespace;
        if (args.alias !== undefined) params.alias = args.alias;
        if (args.modifiers !== undefined) params.modifiers = args.modifiers;
        if (args.importKind !== undefined) params.import_kind = args.importKind;
        if (args.typeOnly !== undefined) params.type_only = args.typeOnly;
        if (args.removeName !== undefined) params.name = args.removeName;
        if (args.validate !== undefined) params.validate = args.validate;
        const response = await callBridge(ctx, context, commandMap[op], params);
        if (response.success === false) {
          throw new Error((response.message as string) || `${op} failed`);
        }
        return JSON.stringify(response);
      },
    },
  };
}
