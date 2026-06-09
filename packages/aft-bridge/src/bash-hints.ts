// Pure helpers for the bash-output hint nudges appended to bash tool results.
//
// Shared across harnesses (OpenCode applies it in `tool.execute.after`; Pi
// applies it inside its hoisted bash tool). Returns the new output string (or
// the original when no hint should fire). The appended "[Hint] ..." line is
// agent-visible and persists in the tool result.

const CONFLICT_HINT =
  "\n\n[Hint] Use aft_conflicts to see all conflict regions across files in a single call.";

const GREP_SEARCH_AFT_SEARCH_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";

const GREP_SEARCH_GREP_HINT =
  "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

const GREP_SEARCH_HINT_PREFIX = "DO NOT search code by running grep/rg in bash —";

type Quote = "none" | "single" | "double";

interface TokenResult {
  token: string;
  end: number;
}

/**
 * Append the `aft_conflicts` hint when the output indicates a real git merge
 * or rebase produced conflicts.
 *
 * Gated on BOTH:
 *  - the "Automatic merge failed; fix conflicts" marker, AND
 *  - a git-conflict signal (`CONFLICT (...)` line or `error: could not apply`)
 *
 * Both conditions are required because `aft_conflicts` calls `git ls-files -u`,
 * which fails with "not a git repository" outside a git working tree. The
 * marker string can legitimately appear in docs, READMEs, test fixtures, and
 * grep output, so we cannot key off it alone — a false-positive hint sends
 * agents into a confusing error.
 */
export function maybeAppendConflictsHint(output: string): string {
  if (!output.includes("Automatic merge failed; fix conflicts")) return output;
  // git merge prints "CONFLICT (content|file|...): ..." per file.
  // git rebase / git am print "error: could not apply <sha>" per failed pick.
  if (!/^CONFLICT \(|^error: could not apply /m.test(output)) return output;
  return output + CONFLICT_HINT;
}

/**
 * Return true when the command itself leads with a code-search command.
 *
 * This deliberately ignores grep/rg used as downstream filters (for example,
 * `bun test | grep fail`). It only inspects the first pipeline stage's first
 * quote/escape-aware token, after peeling a leading `cd <dir> &&` prefix.
 * Ambiguous shell syntax (notably unmatched quotes) returns false so the nudge
 * never fires spuriously.
 */
export function commandLeadsWithCodeSearch(command: string): boolean {
  const trimmed = command.trim();
  if (!trimmed) return false;

  const afterCd = peelLeadingCdAnd(trimmed);
  if (afterCd === null) return false;

  const firstStage = firstPipelineStage(afterCd);
  if (firstStage === null) return false;

  const firstToken = readShellToken(firstStage, skipSpaces(firstStage, 0));
  if (firstToken === null) return false;
  return firstToken.token === "grep" || firstToken.token === "rg";
}

/**
 * Append the grep/rg code-search nudge for native bash output that did not go
 * through the Rust grep rewrite footer path.
 */
export function maybeAppendGrepSearchHint(
  output: string,
  command: string,
  aftSearchRegistered: boolean,
): string {
  if (output === "") return output;
  if (!commandLeadsWithCodeSearch(command)) return output;
  if (output.includes(GREP_SEARCH_HINT_PREFIX)) return output;

  const hint = aftSearchRegistered ? GREP_SEARCH_AFT_SEARCH_HINT : GREP_SEARCH_GREP_HINT;
  return `${output}\n\n${hint}`;
}

function peelLeadingCdAnd(command: string): string | null {
  const first = readShellToken(command, skipSpaces(command, 0));
  if (first === null) return null;
  if (first.token !== "cd") return command;

  const dir = readShellToken(command, skipSpaces(command, first.end));
  if (dir === null) return null;
  if (!dir.token) return command;

  const afterDir = skipSpaces(command, dir.end);
  if (!command.startsWith("&&", afterDir)) return command;
  return command.slice(afterDir + 2).trim();
}

function firstPipelineStage(command: string): string | null {
  let quote: Quote = "none";
  let firstPipeIndex: number | undefined;

  for (let index = 0; index < command.length; index++) {
    const ch = command[index];
    if (quote === "single") {
      if (ch === "'") quote = "none";
      continue;
    }
    if (quote === "double") {
      if (ch === '"') {
        quote = "none";
      } else if (ch === "\\") {
        index++;
      } else if (ch === "`") {
        return null;
      }
      continue;
    }

    if (ch === "'") {
      quote = "single";
    } else if (ch === '"') {
      quote = "double";
    } else if (ch === "\\") {
      index++;
    } else if (ch === "`") {
      return null;
    } else if (ch === "|") {
      if (command[index + 1] === "|") {
        index++;
      } else if (firstPipeIndex === undefined) {
        firstPipeIndex = index;
      }
    }
  }

  if (quote !== "none") return null;
  return command.slice(0, firstPipeIndex ?? command.length).trim();
}

function readShellToken(command: string, start: number): TokenResult | null {
  let quote: Quote = "none";
  let token = "";
  let index = start;

  for (; index < command.length; index++) {
    const ch = command[index];
    if (quote === "single") {
      if (ch === "'") {
        quote = "none";
      } else {
        token += ch;
      }
      continue;
    }
    if (quote === "double") {
      if (ch === '"') {
        quote = "none";
      } else if (ch === "\\") {
        index++;
        token += command[index] ?? "\\";
      } else if (ch === "`") {
        return null;
      } else {
        token += ch;
      }
      continue;
    }

    if (/\s/.test(ch)) break;
    if (isTokenBoundary(ch)) break;
    if (ch === "'") {
      quote = "single";
    } else if (ch === '"') {
      quote = "double";
    } else if (ch === "\\") {
      index++;
      token += command[index] ?? "\\";
    } else if (ch === "`") {
      return null;
    } else {
      token += ch;
    }
  }

  if (quote !== "none") return null;
  return { token, end: index };
}

function isTokenBoundary(ch: string): boolean {
  return ch === "|" || ch === ";" || ch === "&" || ch === "<" || ch === ">";
}

function skipSpaces(input: string, start: number): number {
  let index = start;
  while (index < input.length && /\s/.test(input[index])) index++;
  return index;
}
