import { realpathSync } from "node:fs";
import { homedir, userInfo } from "node:os";

export function escapeRegex(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function safeRealpath(p: string): string | null {
  try {
    return realpathSync(p);
  } catch {
    return null;
  }
}

const SECRET_PLACEHOLDER = "<REDACTED_SECRET>";
const URL_CREDENTIAL_PLACEHOLDER = "***";

function redactSecrets(content: string): string {
  let sanitized = content;

  sanitized = sanitized.replace(
    /\b(Authorization\s*:\s*Bearer\s+)[A-Za-z0-9._~+/-]+=*/gi,
    `$1${SECRET_PLACEHOLDER}`,
  );
  sanitized = sanitized.replace(/\bgh(?:p|o|s)_[A-Za-z0-9_]{16,}\b/g, SECRET_PLACEHOLDER);
  sanitized = sanitized.replace(
    /\bsk-(?:live-)?[A-Za-z0-9][A-Za-z0-9_-]{7,}\b/g,
    SECRET_PLACEHOLDER,
  );
  sanitized = sanitized.replace(
    /\b((?:api[_-]?key|token|secret|password)\s*[=:]\s*)(["'])([^"'\r\n]+)\2/gi,
    `$1$2${SECRET_PLACEHOLDER}$2`,
  );
  sanitized = sanitized.replace(
    /\b((?:api[_-]?key|token|secret|password)\s*[=:]\s*)([^\s,;&"']+)/gi,
    `$1${SECRET_PLACEHOLDER}`,
  );
  sanitized = sanitized.replace(
    /\b([a-z][a-z0-9+.-]*:\/\/)[^@\s/?#]+@/gi,
    `$1${URL_CREDENTIAL_PLACEHOLDER}@`,
  );
  sanitized = sanitized.replace(/\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b/gi, "<EMAIL>");

  return sanitized;
}

/**
 * Strip personally identifiable path segments and usernames from arbitrary
 * text. Used on log contents, diagnostic JSON blocks, and the final issue body
 * so reports never leak usernames or home-directory paths.
 */
export function sanitizeContent(content: string): string {
  const username = userInfo().username;
  const home = homedir();

  let sanitized = redactSecrets(content);

  // Redact the project/working-directory prefix first. It's the most specific
  // path and often the biggest leak in logs (it names the repo the user is
  // working on). Done before the home-dir pass because the cwd usually lives
  // under home; in-project relative structure is left intact for debugging.
  const cwd = process.cwd();
  for (const candidate of new Set([cwd, safeRealpath(cwd)])) {
    if (candidate && candidate !== "/" && candidate !== home) {
      sanitized = sanitized.replace(new RegExp(escapeRegex(candidate), "g"), "<PROJECT>");
    }
  }

  if (home) {
    sanitized = sanitized.replace(new RegExp(escapeRegex(home), "g"), "~");
  }
  sanitized = sanitized.replace(/\/Users\/[^/\s"']+/g, "/Users/<USER>");
  sanitized = sanitized.replace(/\/home\/[^/\s"']+/g, "/home/<USER>");
  sanitized = sanitized.replace(/C:\\\\Users\\\\[^\\\\"'\s]+/g, "C:\\\\Users\\\\<USER>");
  sanitized = sanitized.replace(/C:\\Users\\[^\\"'\s]+/g, "C:\\Users\\<USER>");
  if (username) {
    sanitized = sanitized.replace(new RegExp(escapeRegex(username), "g"), "<USER>");
  }
  return sanitized;
}

/**
 * Recursively sanitize any value by deep-walking objects/arrays. Strings are
 * passed through `sanitizeContent`; other primitives are preserved.
 */
export function sanitizeValue(value: unknown): unknown {
  if (typeof value === "string") {
    return sanitizeContent(value);
  }
  if (Array.isArray(value)) {
    return value.map((entry) => sanitizeValue(entry));
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, entry]) => [key, sanitizeValue(entry)]),
    );
  }
  return value;
}
