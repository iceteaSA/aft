const HERMETIC_GIT_CONFIG_PATH = process.platform === "win32" ? "NUL" : "/dev/null";

export const HERMETIC_GIT_CHILD_ENV = {
  GIT_CONFIG_GLOBAL: HERMETIC_GIT_CONFIG_PATH,
  GIT_CONFIG_SYSTEM: HERMETIC_GIT_CONFIG_PATH,
} as const;

export function hermeticGitChildEnv(extra: Record<string, string> = {}): Record<string, string> {
  return {
    ...extra,
    ...HERMETIC_GIT_CHILD_ENV,
  };
}

export function withHermeticGitEnv(
  env: NodeJS.ProcessEnv | Record<string, string | undefined> = process.env,
): NodeJS.ProcessEnv {
  return {
    ...env,
    ...HERMETIC_GIT_CHILD_ENV,
  };
}
