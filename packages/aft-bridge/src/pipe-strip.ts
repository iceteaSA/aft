export interface PipeStripResult {
  command: string;
  stripped: boolean;
  note?: string;
}

// Filters that only *view* or *reshape* a command's output for human reading.
// When a test/build runner is piped entirely through these, we can drop the
// pipeline and run the bare command — the output compressor reduces the full
// output while preserving failures/summaries, which these filters routinely
// strip away. Two families:
//   - viewing:   grep, rg, head, tail, cat, less, more
//   - transform: sed, awk, cut, sort, uniq, tr, column, fold
// `wc` is deliberately excluded below: it collapses output to a scalar count
// the agent explicitly asked for, so stripping it would be surprising.
const NOISE_FILTERS = new Set([
  "grep",
  "rg",
  "head",
  "tail",
  "cat",
  "less",
  "more",
  "sed",
  "awk",
  "cut",
  "sort",
  "uniq",
  "tr",
  "column",
  "fold",
]);
const GREP_GUARD_FLAGS = new Set([
  "c",
  "count",
  "q",
  "quiet",
  "o",
  "only-matching",
  "l",
  "files-with-matches",
]);

type ChainSeparator = "&&" | "||" | ";" | "";

interface ChainSegment {
  segment: string;
  separator: ChainSeparator;
}

type SegmentStripResult =
  | { segment: string; stripped: false }
  | { segment: string; stripped: true; filters: string };

export function maybeStripCompressorPipe(
  command: string,
  compressionEnabled: boolean,
): PipeStripResult {
  if (!compressionEnabled) return { command, stripped: false };

  const chain = splitTopLevelCommandChain(command);
  if (chain === null) return { command, stripped: false };

  let stripped = false;
  const droppedFilterChains: string[] = [];
  const rebuilt = chain
    .map(({ segment, separator }) => {
      const result = stripSinglePipelineSegment(segment);
      if (result.stripped) {
        stripped = true;
        droppedFilterChains.push(result.filters);
      }
      return `${result.segment}${separator}`;
    })
    .join("");

  if (!stripped) return { command, stripped: false };

  return {
    command: rebuilt,
    stripped: true,
    note: formatStripNote(droppedFilterChains),
  };
}

function stripSinglePipelineSegment(segment: string): SegmentStripResult {
  const leading = /^\s*/.exec(segment)?.[0] ?? "";
  const trailing = /\s*$/.exec(segment)?.[0] ?? "";
  const coreStart = leading.length;
  const coreEnd = segment.length - trailing.length;
  if (coreEnd <= coreStart) return { segment, stripped: false };

  const core = segment.slice(coreStart, coreEnd);

  // Bail on shell constructs our lightweight pipe-splitter cannot reason about
  // safely. Command substitution / backticks / process substitution can embed
  // their own pipes, so naive top-level splitting would carve the segment at an
  // INNER pipe and rebuild a malformed runner (e.g.
  // `pytest $(find . | head) | grep FAIL` → `pytest $(find .`).
  if (containsUnsplittableConstruct(core)) return { segment, stripped: false };

  // Standalone backgrounding changes command grouping/exit semantics. Keep this
  // segment verbatim, but let other top-level chain segments strip independently.
  if (hasUnquotedBackground(core)) return { segment, stripped: false };

  const stages = splitTopLevelPipeline(core);
  if (stages.length < 2) return { segment, stripped: false };

  const firstStage = stages[0]?.trim() ?? "";
  if (!isCompressorHandledRunner(firstStage)) return { segment, stripped: false };

  const filterStages = stages.slice(1).map((stage) => stage.trim());
  for (const stage of filterStages) {
    // A dropped filter stage must be a pure stdin→stdout view. If it writes a
    // file, reads a file (bypassing stdin), or backgrounds, dropping it would
    // silently lose data or change intent — bail and run the segment verbatim.
    if (!filterStageIsSafeToDrop(stage)) return { segment, stripped: false };
  }

  return {
    segment: `${leading}${firstStage}${trailing}`,
    stripped: true,
    filters: filterStages.join(" | "),
  };
}

function formatStripNote(droppedFilterChains: string[]): string {
  const filters = droppedFilterChains.map((filter) => `\`| ${filter}\``).join(", ");
  return `[AFT dropped ${filters} (compressed:false to keep)]`;
}

/**
 * Split a command into top-level chain segments, respecting quotes, escapes,
 * backticks, and parenthesis/substitution depth. Separators inside quotes,
 * `$()`, backticks, process substitution, or grouping parens are left inside the
 * segment. Returns `null` when the top-level structure is not parseable with
 * confidence (unclosed quotes/substitutions/parens, stray closing parens, or a
 * top-level newline separator).
 */
function splitTopLevelCommandChain(command: string): ChainSegment[] | null {
  const segments: ChainSegment[] = [];
  let start = 0;
  let quote: "'" | '"' | null = null;
  let escaped = false;
  let inBacktick = false;
  let parenDepth = 0;

  for (let index = 0; index < command.length; index++) {
    const char = command[index];
    const next = command[index + 1];

    if (escaped) {
      escaped = false;
      continue;
    }
    if (inBacktick) {
      if (char === "\\") escaped = true;
      else if (char === "`") inBacktick = false;
      continue;
    }
    if (char === "\\" && quote !== "'") {
      escaped = true;
      continue;
    }
    if (quote) {
      if (char === quote) quote = null;
      continue;
    }
    if (char === "'" || char === '"') {
      quote = char;
      continue;
    }
    if (char === "`") {
      inBacktick = true;
      continue;
    }
    if (char === "(") {
      parenDepth++;
      continue;
    }
    if (char === ")") {
      if (parenDepth === 0) return null;
      parenDepth--;
      continue;
    }
    if (parenDepth > 0) continue;

    // Top-level newline is a command separator, but not one this conservative
    // rewriter reassembles; keep the whole command verbatim.
    if (char === "\n" || char === "\r") return null;

    if (char === "&" && next === "&") {
      segments.push({ segment: command.slice(start, index), separator: "&&" });
      start = index + 2;
      index++;
      continue;
    }
    if (char === "|" && next === "|") {
      segments.push({ segment: command.slice(start, index), separator: "||" });
      start = index + 2;
      index++;
      continue;
    }
    if (char === ";") {
      segments.push({ segment: command.slice(start, index), separator: ";" });
      start = index + 1;
    }
  }

  if (escaped || quote || inBacktick || parenDepth !== 0) return null;

  segments.push({ segment: command.slice(start), separator: "" });
  return segments;
}

function splitTopLevelPipeline(command: string): string[] {
  const stages: string[] = [];
  let start = 0;
  let quote: "'" | '"' | null = null;
  let escaped = false;

  for (let index = 0; index < command.length; index++) {
    const char = command[index];
    const next = command[index + 1];
    const previous = command[index - 1];

    if (escaped) {
      escaped = false;
      continue;
    }

    if (char === "\\" && quote !== "'") {
      escaped = true;
      continue;
    }

    if (quote) {
      if (char === quote) quote = null;
      continue;
    }

    if (char === "'" || char === '"') {
      quote = char;
      continue;
    }

    if (char === "|" && previous !== "|" && next !== "|") {
      stages.push(command.slice(start, index));
      start = index + 1;
    }
  }

  stages.push(command.slice(start));
  return stages;
}

/**
 * Is the first stage a test/build/lint/typecheck runner whose full output the
 * agent actually needs (i.e. failures)? Those are the commands where a
 * downstream viewing filter silently hides failures, so stripping the filter
 * and letting the compressor reduce the bare output is strictly better.
 *
 * IMPORTANT — this list is intentionally NARROW. It must only contain commands
 * you run to learn "did it pass / build / typecheck cleanly". It must NOT
 * include log-emitting or search tools (git, docker, kubectl, ls, find, cat,
 * journalctl, …) where a downstream `| grep`/`| tail` is the agent's GENUINE
 * intent (e.g. `git log | grep fix`, `docker logs app | tail`). Stripping those
 * would change behavior and surprise the agent. When in doubt, leave it out.
 */
function isCompressorHandledRunner(stage: string): boolean {
  const tokens = tokenizeStage(stage);
  if (tokens.length === 0) return false;
  if (tokens.some((token) => token === "&&" || token === "||" || token.includes(";"))) {
    return false;
  }

  // Peel leading POSIX env-var assignments (`VAR=value`, possibly several) that
  // prefix the runner (e.g. `CI=1 bun test`, `FOO=bar BAZ=qux npm test`).
  // `containsUnsplittableConstruct` already rejects `VAR=$(cmd)` (command
  // substitution), so values here are safe literals or simple expansions.
  let tokenOffset = 0;
  while (tokenOffset < tokens.length && isEnvAssignment(tokens[tokenOffset])) {
    tokenOffset++;
  }

  // Basename the launcher so `./gradlew`, `./mvnw`, `node_modules/.bin/jest`,
  // and `./vendor/bin/phpunit` resolve to their tool name.
  const first = runnerName(tokens[tokenOffset]);
  const runnerArgs = tokens.slice(tokenOffset + 1);
  const second = runnerArgs[0];
  const third = runnerArgs[1];
  const rest = runnerArgs;
  if (!first) return false;

  // --- JavaScript / TypeScript ---
  if (first === "bun") {
    // Skip `--cwd <dir>` / `--cwd=<dir>` before the subcommand.
    let args = rest;
    if (args[0] === "--cwd") args = args.slice(2);
    else if (args[0]?.startsWith("--cwd=")) args = args.slice(1);
    const sub = args[0];
    const subNext = args[1];
    return sub === "test" || (sub === "run" && isJsVerificationScript(subNext));
  }
  if (first === "npm" || first === "pnpm") {
    return second === "test" || (second === "run" && isJsVerificationScript(third));
  }
  if (first === "yarn") {
    // yarn berry runs a script by name directly (`yarn test:unit`) and also
    // supports `yarn run <script>`.
    return isJsVerificationScript(second) || (second === "run" && isJsVerificationScript(third));
  }
  if (first === "deno") return ["test", "lint", "check", "bench"].includes(second ?? "");
  if (first === "npx") {
    return ["tsc", "eslint", "vitest", "jest", "playwright", "biome"].includes(second ?? "");
  }
  if (first === "playwright") return second === "test";

  // --- Rust ---
  if (first === "cargo") {
    return ["test", "build", "check", "clippy", "nextest"].includes(second ?? "");
  }

  // --- Go ---
  if (first === "go") return ["test", "build", "vet"].includes(second ?? "");

  // --- Java / JVM (tasks can appear anywhere: `gradle clean test`) ---
  // `clean` is allowed — `clean test`/`clean build` is the canonical fresh run
  // and only removes build output, unlike stateful goals (publish/deploy).
  if (first === "gradle" || first === "gradlew") {
    return hasBuildTask(rest, ["test", "check", "build", "assemble", "clean"]);
  }
  if (first === "mvn" || first === "mvnw") {
    return hasBuildTask(rest, ["test", "verify", "package", "install", "clean"]);
  }

  // --- .NET ---
  if (first === "dotnet") return ["test", "build"].includes(second ?? "");

  // --- Ruby ---
  if (first === "rspec") return true;
  // Allow multiple task words when all are plain task names (no flags/paths)
  // and at least one is `test` or `spec` — so `rake db:setup test` strips but
  // `rake test_db_reset` or `rake deploy` does not.
  if (first === "rake") {
    const positionals = rest.filter((a) => !a.startsWith("-"));
    if (positionals.length === 0) return false;
    if (positionals.some((a) => a.includes("/") || a.includes(".") || a.includes("=")))
      return false;
    return positionals.some((a) => a === "test" || a === "spec");
  }

  // --- PHP ---
  if (first === "phpunit" || first === "pest") return true;

  // --- Apple / Swift ---
  // Only real build/test ACTIONS — NOT query commands (`xcodebuild -list`,
  // `-showBuildSettings`) and NOT a scheme/target merely NAMED "test"
  // (`xcodebuild -showBuildSettings -scheme test`). Actions are bare positional
  // tokens, distinct from the values that follow `-scheme`/`-target`/etc.
  if (first === "xcodebuild") return xcodebuildHasBuildAction(rest);
  if (first === "swift") return second === "test" || second === "build";

  // --- Make (require an explicit test/lint target — bare `make` is a generic
  //     build that may legitimately be grepped for errors) ---
  if (first === "make" || first === "gmake") {
    return hasBuildTask(rest, ["test", "check", "lint", "clean"]);
  }

  // --- Bare test / lint / typecheck runners ---
  return [
    "vitest",
    "jest",
    "pytest",
    "tsc",
    "eslint",
    "biome",
    "ruff",
    "mypy",
    "tox",
    "nox",
  ].includes(first);
}

/**
 * Is this token a POSIX env-var assignment (`NAME=value`)? Name must start with
 * a letter or underscore, followed by alphanumerics/underscores, then `=`.
 * Rejects `--flag=value`, `path/cmd`, and `$()` values (the latter already
 * caught by `containsUnsplittableConstruct` on the whole command).
 */
function isEnvAssignment(token: string | undefined): boolean {
  if (!token) return false;
  return /^[a-zA-Z_][a-zA-Z0-9_]*=/.test(token);
}

/** Last path segment of a launcher token (`./gradlew` → `gradlew`, `jest` → `jest`). */
function runnerName(token: string | undefined): string {
  if (!token) return "";
  const slash = token.lastIndexOf("/");
  return slash === -1 ? token : token.slice(slash + 1);
}

/**
 * Should a make/gradle/mvn invocation be treated as a pure test/build run that
 * is safe to strip? Requires (1) at least one of the allowed tasks present, and
 * (2) EVERY positional (non-flag) arg to be an allowed task — so a mixed
 * invocation like `make deploy test` or `gradle publish test` bails, because
 * the stateful goal (deploy/publish) is the real intent whose output matters.
 *
 * Allowed-task match accepts the bare task (`test`) and qualified Gradle forms
 * (`:app:test`), but not substrings (`my-test-module` ≠ `test`). Flags
 * (`-x`, `--info`, `-Dkey=val`) and `key=value` make/property args are ignored.
 */
function hasBuildTask(args: string[], tasks: string[]): boolean {
  const isAllowedTask = (arg: string): boolean =>
    tasks.some((task) => arg === task || arg.endsWith(`:${task}`));
  const isFlagOrProperty = (arg: string): boolean => arg.startsWith("-") || arg.includes("=");

  let sawAllowed = false;
  for (const arg of args) {
    if (isFlagOrProperty(arg)) continue;
    if (!isAllowedTask(arg)) return false; // a non-flag positional that isn't an allowed task
    sawAllowed = true;
  }
  return sawAllowed;
}

function isJsVerificationScript(token: string | undefined): boolean {
  if (!token) return false;
  return token.startsWith("test") || token === "typecheck" || token.startsWith("typecheck:");
}

// xcodebuild options that take a value (the following token is NOT an action).
const XCODEBUILD_VALUE_FLAGS = new Set([
  "-scheme",
  "-target",
  "-project",
  "-workspace",
  "-configuration",
  "-sdk",
  "-destination",
  "-arch",
  "-derivedDataPath",
  "-resultBundlePath",
  "-xcconfig",
  "-toolchain",
]);
const XCODEBUILD_BUILD_ACTIONS = new Set([
  "build",
  "test",
  "build-for-testing",
  "test-without-building",
  "analyze",
]);

/**
 * True only when an actual build/test ACTION appears as a bare positional,
 * skipping the value token after a value-taking flag so a scheme/target named
 * "test" isn't mistaken for the `test` action.
 */
function xcodebuildHasBuildAction(args: string[]): boolean {
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg.startsWith("-")) {
      if (XCODEBUILD_VALUE_FLAGS.has(arg)) i++; // skip its value
      continue;
    }
    if (XCODEBUILD_BUILD_ACTIONS.has(arg)) return true;
  }
  return false;
}

/**
 * Does the command contain a shell construct that can embed its own pipe and so
 * break naive top-level pipe-splitting? Command substitution `$(...)`,
 * backticks, process substitution `<(...)`/`>(...)`, and any subshell/grouping
 * parentheses `( ... )`. The splitter tracks neither nesting nor paren balance,
 * so a pipe inside (or a paren spanning) any of these would be mis-split or
 * leave unbalanced parens after the strip (e.g. `(cd d && bun test | tail)`
 * → `(cd d && bun test`). Quote-aware so a literal `(` inside quotes is fine.
 */
function containsUnsplittableConstruct(command: string): boolean {
  let quote: "'" | '"' | null = null;
  let escaped = false;
  for (let i = 0; i < command.length; i++) {
    const char = command[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (char === "\\" && quote !== "'") {
      escaped = true;
      continue;
    }
    if (quote === "'") {
      // Single quotes are literal — nothing expands inside them.
      if (char === "'") quote = null;
      continue;
    }
    if (quote === '"') {
      // Double quotes still allow `$(...)` and backtick command substitution,
      // so keep scanning for those even while inside double quotes.
      if (char === '"') quote = null;
      else if (char === "`") return true;
      else if (char === "$" && command[i + 1] === "(") return true;
      continue;
    }
    if (char === "'" || char === '"') {
      quote = char;
      continue;
    }
    if (char === "`") return true;
    // Any unquoted paren — command/process substitution, subshell, or grouping.
    if (char === "(" || char === ")") return true;
  }
  return false;
}

// Filters that, given ANY bare (non-flag) operand, read that operand as a FILE
// instead of stdin — so a stage like `cat saved.log` or `head file` ignores the
// piped output entirely. Dropping them would replace the runner's output with
// the file's contents.
const READS_FILE_OPERAND = new Set(["cat", "tac", "nl", "less", "more"]);

/**
 * Is a (to-be-dropped) filter stage a pure stdin→stdout view that can be safely
 * removed? It must be a recognized viewing/transform filter invoked with no file
 * write, no file read that bypasses stdin, and no backgrounding. Conservative:
 * anything ambiguous bails (we just don't optimize — never lose data).
 */
function filterStageIsSafeToDrop(stage: string): boolean {
  const head = tokenizeStage(stage)[0];
  if (!head) return false;
  if (head === "wc") return false; // collapses to a scalar the agent asked for
  if (!NOISE_FILTERS.has(head)) return false;

  // Any unquoted redirect / process-sub / backgrounding metacharacter, OR a
  // redirect hidden INSIDE quotes (awk `'{ print > "f" }'`, sed `'w file'`).
  // We scan the raw stage for `>`/`<` ANYWHERE (quote-blind) because a redirect
  // inside a filter's program is still a write; over-bailing on a literal `>`
  // search pattern is safe (we just skip the optimization).
  if (/[<>]/.test(stage)) return false;
  if (hasUnquotedBackground(stage)) return false;

  const args = tokenizeStage(stage).slice(1);
  const hasFlag = (...names: string[]): boolean =>
    args.some((a) => names.some((n) => a === n || a.startsWith(`${n}=`)));

  // grep/rg: a count/list/quiet flag changes the output the agent wanted, AND a
  // second bare operand means it reads a FILE not stdin (`grep PAT file`).
  // (`-i` here is case-insensitive, NOT in-place — must not bail.)
  if (head === "grep" || head === "rg") {
    if (hasIntentChangingGrepFlag(args)) return false;
    // pattern is one bare operand; more than one means a file argument.
    if (countBareOperands(args) > 1) return false;
    return true;
  }

  // head/tail: bare operands are files unless consumed by -n/-c. Any bare
  // non-numeric operand is a filename → reads the file, bypassing stdin.
  if (head === "head" || head === "tail") {
    if (bareOperands(args).some((op) => !/^\d+$/.test(op))) return false;
    return true;
  }

  // cat/tac/nl/less/more: any bare operand is a file to read instead of stdin.
  if (READS_FILE_OPERAND.has(head)) {
    if (countBareOperands(args) > 0) return false;
    return true;
  }

  // sed: `-i`/`--in-place` writes the file in place; the first bare operand is
  // the script, a SECOND is an input file. (Internal `w`/`>` caught by `[<>]`.)
  if (head === "sed") {
    if (hasFlag("-i", "--in-place")) return false;
    if (countBareOperands(args) > 1) return false;
    return true;
  }
  // awk: first bare operand is the program, a SECOND is an input file.
  if (head === "awk") {
    if (countBareOperands(args) > 1) return false;
    return true;
  }

  // sort: `-o`/`--output` writes a file.
  if (head === "sort" && hasFlag("-o", "--output")) return false;

  // sort/uniq/cut/tr/column/fold: a bare path-like operand reads a file.
  // (`-o` already caught.) tr's operands are sets, not files; cut/sort can take
  // a trailing file. Conservatively bail on any operand that looks like a path.
  if (bareOperands(args).some((op) => op.includes("/") || op.includes("."))) return false;
  return true;
}

/** Bare (non-flag, non-flag-value) operands of a tokenized arg list. */
function bareOperands(args: string[]): string[] {
  const out: string[] = [];
  let afterDoubleDash = false;
  for (const arg of args) {
    if (!afterDoubleDash && arg === "--") {
      afterDoubleDash = true;
      continue;
    }
    if (!afterDoubleDash && arg.startsWith("-") && arg !== "-") continue;
    out.push(arg);
  }
  return out;
}

function countBareOperands(args: string[]): number {
  return bareOperands(args).length;
}

/** A standalone background `&` (not `&&`, not the `&` in a `>&`/`&>` fd dup). */
function hasUnquotedBackground(stage: string): boolean {
  let quote: "'" | '"' | null = null;
  let escaped = false;
  for (let i = 0; i < stage.length; i++) {
    const char = stage[i];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (char === "\\" && quote !== "'") {
      escaped = true;
      continue;
    }
    if (quote) {
      if (char === quote) quote = null;
      continue;
    }
    if (char === "'" || char === '"') {
      quote = char;
      continue;
    }
    if (char === "&") {
      const prev = stage[i - 1];
      const next = stage[i + 1];
      // `&&` (handled elsewhere), `>&`/`&>`/`2>&1` fd dup → not a background.
      if (prev === "&" || next === "&" || prev === ">" || next === ">") continue;
      return true;
    }
  }
  return false;
}

function hasIntentChangingGrepFlag(args: string[]): boolean {
  for (const arg of args) {
    if (arg === "--") return false;
    if (!arg.startsWith("-") || arg === "-") continue;
    if (arg.startsWith("--")) {
      const flag = arg.slice(2).split("=", 1)[0];
      if (GREP_GUARD_FLAGS.has(flag)) return true;
      continue;
    }
    for (const flag of arg.slice(1)) {
      if (GREP_GUARD_FLAGS.has(flag)) return true;
    }
  }
  return false;
}

function tokenizeStage(stage: string): string[] {
  const tokens: string[] = [];
  let current = "";
  let quote: "'" | '"' | null = null;
  let escaped = false;

  for (let index = 0; index < stage.length; index++) {
    const char = stage[index];

    if (escaped) {
      current += char;
      escaped = false;
      continue;
    }

    if (char === "\\" && quote !== "'") {
      escaped = true;
      continue;
    }

    if (quote) {
      if (char === quote) {
        quote = null;
      } else {
        current += char;
      }
      continue;
    }

    if (char === "'" || char === '"') {
      quote = char;
      continue;
    }

    if (/\s/.test(char)) {
      if (current.length > 0) {
        tokens.push(current);
        current = "";
      }
      continue;
    }

    current += char;
  }

  if (current.length > 0) tokens.push(current);
  return tokens;
}
