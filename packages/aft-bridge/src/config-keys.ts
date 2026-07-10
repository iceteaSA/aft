/**
 * Top-level config keys owned by one host plugin rather than the shared config
 * loader. Both harnesses read the same CortexKit config file, so each schema
 * strips the other harness's declared keys before strict validation.
 */
export const OPENCODE_ONLY_KEYS = ["hoist_builtin_tools", "auto_update"] as const;

/** Pi currently has no top-level keys that OpenCode does not understand. */
export const PI_ONLY_KEYS = [] as const;

/** Strip only known top-level keys owned by the other harness. */
export function stripHarnessSpecificConfigKeys(value: unknown, keys: readonly string[]): unknown {
  if (!value || typeof value !== "object" || Array.isArray(value)) return value;
  const stripped = { ...(value as Record<string, unknown>) };
  for (const key of keys) delete stripped[key];
  return stripped;
}
