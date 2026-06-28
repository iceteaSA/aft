/**
 * Subc manifest tool schemas: bare manifest names → JSON Schema from agent tools.
 * Shared by scripts/build-tool-schemas.ts and subc-tool-schemas-fresh.test.ts.
 */

import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";

import { createBashTool } from "./tools/bash.js";
import { createReadTool, hoistedTools } from "./tools/hoisted.js";
import { inspectTools } from "./tools/inspect.js";
import { readingTools } from "./tools/reading.js";
import { searchTools } from "./tools/search.js";
import { semanticTools } from "./tools/semantic.js";
import type { PluginContext } from "./types.js";

const z = tool.schema;

const STATUS_SCHEMA = {
  type: "object",
  properties: {},
  additionalProperties: false,
} as const;

const BARE_TOOL_ORDER = [
  "bash",
  "edit",
  "grep",
  "inspect",
  "outline",
  "read",
  "search",
  "status",
  "write",
] as const;

export type SubcBareToolName = (typeof BARE_TOOL_ORDER)[number];

export function makeSubcSchemaStubCtx(): PluginContext {
  return {
    pool: {
      getBridge: () =>
        ({
          send: async () => ({ success: true }),
        }) as unknown as ReturnType<BridgePool["getBridge"]>,
    } as unknown as BridgePool,
    client: { lsp: {}, find: {} } as PluginContext["client"],
    config: { hoist_builtin_tools: true } as PluginContext["config"],
    storageDir: "/tmp/aft-subc-schema",
  };
}

function argsToJsonSchema(def: ToolDefinition): Record<string, unknown> {
  const wrapped = z.object(def.args);
  const jsonSchema = z.toJSONSchema(wrapped, { io: "input" });
  return jsonSchema as Record<string, unknown>;
}

/**
 * Build the bare-name → JSON Schema map for subc build_manifest.
 */
export function buildSubcToolSchemas(): Record<SubcBareToolName, Record<string, unknown>> {
  const ctx = makeSubcSchemaStubCtx();
  const bash = createBashTool(ctx);
  const read = createReadTool(ctx);
  const { write, edit } = hoistedTools(ctx);
  if (!write || !edit) {
    throw new Error("hoistedTools must expose write and edit");
  }
  const grepTools = searchTools(ctx);
  const grepTool = grepTools.grep ?? grepTools.aft_grep;
  if (!grepTool) {
    throw new Error("searchTools must expose grep or aft_grep");
  }
  const search = semanticTools(ctx).aft_search;
  const outline = readingTools(ctx).aft_outline;
  const inspect = inspectTools(ctx).aft_inspect;

  const bashSchema = argsToJsonSchema(bash);
  const bashProperties = (bashSchema.properties ??= {}) as Record<string, unknown>;
  bashProperties.foreground_orchestrate = {
    type: "boolean",
    description: "Consumer-set flag enabling server-side foreground orchestration.",
  };
  bashProperties.block_to_completion = {
    type: "boolean",
    description:
      "Consumer-set flag forcing foreground bash to wait until terminal instead of promoting.",
  };

  return {
    status: { ...STATUS_SCHEMA },
    bash: bashSchema,
    read: argsToJsonSchema(read),
    write: argsToJsonSchema(write),
    edit: argsToJsonSchema(edit),
    grep: argsToJsonSchema(grepTool),
    search: argsToJsonSchema(search),
    outline: argsToJsonSchema(outline),
    inspect: argsToJsonSchema(inspect),
  };
}

/**
 * Deterministic JSON bytes: top-level keys sorted, 2-space indent, trailing newline.
 */
export function serializeSubcToolSchemas(schemas: Record<string, Record<string, unknown>>): string {
  const sorted: Record<string, Record<string, unknown>> = {};
  for (const key of BARE_TOOL_ORDER) {
    if (schemas[key] !== undefined) {
      sorted[key] = schemas[key];
    }
  }
  for (const key of Object.keys(schemas).sort()) {
    if (sorted[key] === undefined) {
      sorted[key] = schemas[key];
    }
  }
  return `${JSON.stringify(sorted, null, 2)}\n`;
}

export function buildSubcToolSchemasJson(): string {
  return serializeSubcToolSchemas(buildSubcToolSchemas());
}

export const SUBC_BARE_TOOL_NAMES: readonly SubcBareToolName[] = BARE_TOOL_ORDER;
