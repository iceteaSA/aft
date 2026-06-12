import { describe, expect, test } from "bun:test";
import { maybeStripCompressorPipe } from "../pipe-strip.js";

describe("maybeStripCompressorPipe", () => {
  test("strips bun test piped through grep", () => {
    const result = maybeStripCompressorPipe("bun test | grep fail", true);
    expect(result).toEqual({
      command: "bun test",
      stripped: true,
      note: "[AFT dropped `| grep fail` (compressed:false to keep)]",
    });
  });

  test("strips multi-filter cargo test pipeline", () => {
    const result = maybeStripCompressorPipe("cargo test | grep -A3 FAILED | head", true);
    expect(result.command).toBe("cargo test");
    expect(result.stripped).toBe(true);
    expect(result.note).toContain("| grep -A3 FAILED | head");
  });

  // --- Data-loss guards: a dropped filter stage must be a pure stdin→stdout
  //     view. If it writes a file, reads a file (bypassing stdin), or
  //     backgrounds, dropping it would silently lose data — bail. ---
  describe("filter side-effect / file-operand guards", () => {
    const keepsVerbatim = (cmd: string) =>
      expect(maybeStripCompressorPipe(cmd, true)).toEqual({ command: cmd, stripped: false });

    test("awk with an internal redirect (quote-hidden write) is not stripped", () => {
      keepsVerbatim("bun test | awk '{ print > \"out.txt\" }'");
    });
    test("sort -o (write flag) is not stripped", () => {
      keepsVerbatim("bun test | sort -o results.txt");
    });
    test("sed -i (in-place write) is not stripped", () => {
      keepsVerbatim("bun test | sed -i 's/x/y/'");
    });
    test("cat reading a file (ignores stdin) is not stripped", () => {
      keepsVerbatim("bun test | cat saved.log");
    });
    test("head reading a file operand is not stripped", () => {
      keepsVerbatim("bun test | head other.log");
    });
    test("grep reading a file operand (pattern + file) is not stripped", () => {
      keepsVerbatim("bun test | grep fail other.log");
    });
    test("tee-style shell redirect on a filter stage is not stripped", () => {
      keepsVerbatim("bun test | grep fail > out.txt");
    });
    test("backgrounding the filter stage is not stripped", () => {
      keepsVerbatim("bun test | grep fail &");
    });
    test("plain head with a numeric -n operand still strips", () => {
      const r = maybeStripCompressorPipe("bun test | head -n 5", true);
      expect(r).toMatchObject({ command: "bun test", stripped: true });
    });
    test("plain transform chain with no operands still strips", () => {
      const r = maybeStripCompressorPipe("bun test | grep fail | sed 's/x//' | head", true);
      expect(r).toMatchObject({ command: "bun test", stripped: true });
    });
  });

  // --- Separator guards: a top-level newline or background `&` means the pipe
  //     belongs to a LATER command, not the runner. ---
  describe("command-separator guards", () => {
    test("newline separator: the pipe belongs to the second command", () => {
      const cmd = "bun test\necho ok | grep ok";
      expect(maybeStripCompressorPipe(cmd, true)).toEqual({ command: cmd, stripped: false });
    });
    test("background-& separator before another piped command", () => {
      const cmd = "bun test & echo ok | grep ok";
      expect(maybeStripCompressorPipe(cmd, true)).toEqual({ command: cmd, stripped: false });
    });
    test("runner-stage 2>&1 fd-dup still strips (not a background)", () => {
      const r = maybeStripCompressorPipe("bun test 2>&1 | grep fail", true);
      expect(r).toMatchObject({ command: "bun test 2>&1", stripped: true });
    });
  });

  // --- xcodebuild: match a real build/test ACTION, not a scheme/target named
  //     "test". ---
  describe("xcodebuild action vs scheme name", () => {
    test("a scheme literally named test does not trigger a strip", () => {
      const cmd = "xcodebuild -showBuildSettings -scheme test | grep BUILD";
      expect(maybeStripCompressorPipe(cmd, true)).toEqual({ command: cmd, stripped: false });
    });
    test("the real test action strips even with a scheme arg after it", () => {
      const r = maybeStripCompressorPipe("xcodebuild test -scheme MyApp | grep -i fail", true);
      expect(r).toMatchObject({ stripped: true });
    });
  });

  // --- Double-quoted command substitution must not be split. ---
  test("command substitution inside double quotes is not stripped", () => {
    const cmd = 'bun test "$(date)" | grep fail';
    expect(maybeStripCompressorPipe(cmd, true)).toEqual({ command: cmd, stripped: false });
  });

  test("does not strip when compression is disabled", () => {
    expect(maybeStripCompressorPipe("bun test | grep fail", false)).toEqual({
      command: "bun test | grep fail",
      stripped: false,
    });
  });

  test("does not strip count grep", () => {
    expect(maybeStripCompressorPipe("bun test | grep -c fail", true)).toEqual({
      command: "bun test | grep -c fail",
      stripped: false,
    });
  });

  test("does not strip when first stage is not a runner", () => {
    expect(maybeStripCompressorPipe("ls | grep foo", true)).toEqual({
      command: "ls | grep foo",
      stripped: false,
    });
  });

  test("strips text-transform filters (sed/awk/cut/sort/uniq/tr)", () => {
    // These reshape output for human reading and routinely hide test
    // failures/summaries — strip them so the bare runner output reaches the
    // compressor (which preserves failures). Previously only viewing filters
    // (grep/head/tail/...) were recognized, so one `sed` made pipe-strip bail
    // on the whole pipeline, leaking the failure-hiding `grep`.
    expect(maybeStripCompressorPipe("bun test | sed 's/x/y/'", true).command).toBe("bun test");
    expect(maybeStripCompressorPipe("cargo test | awk '{print $1}'", true).command).toBe(
      "cargo test",
    );
    expect(maybeStripCompressorPipe("npm test | sort | uniq", true).command).toBe("npm test");
  });

  test("strips a mixed view+transform chain — the real 'bun test | grep | sed | head' footgun", () => {
    // The exact shape that slipped through before: an unrecognized transform
    // stage in the middle of a chain made the whole pipeline non-strippable,
    // so the leading `grep` survived and hid the failures.
    const result = maybeStripCompressorPipe(
      'bun test 2>&1 | grep -E "fail" | sed -E "s/ ms//" | head -20',
      true,
    );
    expect(result.stripped).toBe(true);
    expect(result.command).toBe("bun test 2>&1");
    expect(result.note).toContain("| grep");
    expect(result.note).toContain("| sed");
    expect(result.note).toContain("| head");
  });

  test("still does not strip wc (collapses to a count the agent asked for)", () => {
    expect(maybeStripCompressorPipe("bun test | wc -l", true).stripped).toBe(false);
  });

  test("BAILS on filter-stage redirection — stripping would lose the written file", () => {
    // `> failures.txt` is a real side effect produced by the dropped filter
    // stage; silently dropping it is data loss.
    expect(maybeStripCompressorPipe("bun test | grep FAIL > failures.txt", true).stripped).toBe(
      false,
    );
    expect(maybeStripCompressorPipe("bun test | grep FAIL >> failures.txt", true).stripped).toBe(
      false,
    );
    expect(maybeStripCompressorPipe("cargo test | grep err 2> errs.log", true).stripped).toBe(
      false,
    );
    // `2>&1` on the RUNNER stage is fine — it survives the strip.
    expect(maybeStripCompressorPipe("bun test 2>&1 | grep FAIL", true).command).toBe(
      "bun test 2>&1",
    );
  });

  test("BAILS on backgrounding (`&`) — changes execution semantics", () => {
    expect(maybeStripCompressorPipe("bun test | grep FAIL &", true).stripped).toBe(false);
  });

  test("BAILS on command-substitution / backticks / process-substitution (misparse risk)", () => {
    // The naive pipe-splitter would carve at the INNER pipe and rebuild a
    // malformed runner; never strip when these constructs are present.
    expect(
      maybeStripCompressorPipe(
        "pytest $(find tests -name '*_test.py' | head -20) | grep FAILED",
        true,
      ).stripped,
    ).toBe(false);
    expect(maybeStripCompressorPipe("bun test | grep -f <(printf 'fail')", true).stripped).toBe(
      false,
    );
    expect(maybeStripCompressorPipe("bun test | grep `cat pat`", true).stripped).toBe(false);
  });

  test("TIGHTEN: xcodebuild strips only for test/build, not query subcommands", () => {
    expect(maybeStripCompressorPipe("xcodebuild -list | grep Schemes", true).stripped).toBe(false);
    expect(
      maybeStripCompressorPipe("xcodebuild -showBuildSettings | grep BUNDLE", true).stripped,
    ).toBe(false);
    expect(maybeStripCompressorPipe("xcodebuild test | tail -5", true).stripped).toBe(true);
  });

  test("TIGHTEN: make/gradle/mvn bail when a stateful goal rides along an allowed task", () => {
    expect(maybeStripCompressorPipe("make deploy test | tail", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("gradle publish test | grep FAIL", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("mvn deploy test | tail", true).stripped).toBe(false);
    // pure test/build invocations (incl. the idiomatic `clean test`) still strip
    expect(maybeStripCompressorPipe("make test | grep Error", true).stripped).toBe(true);
    expect(maybeStripCompressorPipe("gradle clean test | tail", true).stripped).toBe(true);
    expect(maybeStripCompressorPipe("gradle test --info | grep FAIL", true).stripped).toBe(true);
  });

  test("TIGHTEN: rake strips only exact test/spec tasks, not arbitrary project tasks", () => {
    expect(maybeStripCompressorPipe("rake test_db_reset | tail", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("rake test | tail", true).stripped).toBe(true);
    expect(maybeStripCompressorPipe("rake spec | grep fail", true).stripped).toBe(true);
  });

  test("BAILS on subshell / grouping parens — stripping would leave them unbalanced", () => {
    // `(cd d && bun test | tail)` → stripping `| tail` leaves `(cd d && bun test`
    // which is a syntax error. The splitter can't track paren balance, so bail.
    expect(maybeStripCompressorPipe("(cd packages/x && bun test | tail -4)", true).stripped).toBe(
      false,
    );
    expect(maybeStripCompressorPipe("(bun test | grep fail)", true).stripped).toBe(false);
  });

  test("footgun stays covered: a transform stage in the chain still strips (no regression)", () => {
    // The exact `grep | sed | head` shape that previously leaked failures.
    const r = maybeStripCompressorPipe(
      'bun test 2>&1 | grep -E "fail" | sed -E "s/ ms//" | head -20',
      true,
    );
    expect(r.stripped).toBe(true);
    expect(r.command).toBe("bun test 2>&1");
  });

  test("does not split on pipes inside quotes", () => {
    expect(maybeStripCompressorPipe('bun test --name "a|b"', true)).toEqual({
      command: 'bun test --name "a|b"',
      stripped: false,
    });
  });

  test("strips known runner forms", () => {
    expect(maybeStripCompressorPipe("npm run test:unit | tail -20", true).command).toBe(
      "npm run test:unit",
    );
    expect(maybeStripCompressorPipe("npx eslint src | head", true).command).toBe("npx eslint src");
  });

  test("peels a leading cd && prefix and strips the pipeline (#102 dogfood)", () => {
    // `cd dir && bun test | grep fail` is `cd dir && (bun test | grep fail)`
    // because `&&` binds looser than `|`. The prefix is reattached verbatim.
    const result = maybeStripCompressorPipe("cd packages/a && bun test | grep fail", true);
    expect(result.stripped).toBe(true);
    expect(result.command).toBe("cd packages/a && bun test");
    expect(result.note).toContain("| grep fail");
  });

  test("peels a multi-segment && prefix", () => {
    const result = maybeStripCompressorPipe(
      "cd packages/a && export CI=1 && cargo test | grep -A2 FAILED",
      true,
    );
    expect(result.stripped).toBe(true);
    expect(result.command).toBe("cd packages/a && export CI=1 && cargo test");
    expect(result.note).toContain("| grep -A2 FAILED");
  });

  test("does not strip when the &&-prefixed command is not a runner", () => {
    expect(maybeStripCompressorPipe("cd packages/a && ls | grep foo", true).stripped).toBe(false);
  });

  describe("top-level command chains", () => {
    test("strips every safe runner pipeline in the live chained miss", () => {
      const result = maybeStripCompressorPipe(
        "cd packages/opencode-plugin && bun test src/x.test.ts 2>&1 | tail -3 && bun run typecheck 2>&1 | tail -1",
        true,
      );

      expect(result).toEqual({
        command:
          "cd packages/opencode-plugin && bun test src/x.test.ts 2>&1 && bun run typecheck 2>&1",
        stripped: true,
        note: "[AFT dropped `| tail -3`, `| tail -1` (compressed:false to keep)]",
      });
    });

    test("strips safe pipelines on both sides of an && chain", () => {
      const result = maybeStripCompressorPipe(
        "bun test | grep x && cargo test | tail -1",
        true,
      );
      expect(result.command).toBe("bun test && cargo test");
      expect(result.stripped).toBe(true);
      expect(result.note).toContain("| grep x");
      expect(result.note).toContain("| tail -1");
    });

    test("strips a runner segment and leaves an unpiped echo segment untouched", () => {
      const result = maybeStripCompressorPipe("bun test | tail -3 && echo done", true);
      expect(result).toEqual({
        command: "bun test && echo done",
        stripped: true,
        note: "[AFT dropped `| tail -3` (compressed:false to keep)]",
      });
    });

    test("strips a runner segment before an || fallback", () => {
      const result = maybeStripCompressorPipe("bun test | tail -3 || fallback-cmd", true);
      expect(result.command).toBe("bun test || fallback-cmd");
      expect(result.stripped).toBe(true);
      expect(result.note).toContain("| tail -3");
    });

    test("keeps a backgrounded segment verbatim but strips later safe segments", () => {
      const result = maybeStripCompressorPipe(
        "bun test | tail -3 & echo done && cargo test | tail -1",
        true,
      );
      expect(result.command).toBe("bun test | tail -3 & echo done && cargo test");
      expect(result.stripped).toBe(true);
      expect(result.note).not.toContain("tail -3");
      expect(result.note).toContain("| tail -1");
    });

    test("does not treat a quoted && as a chain separator", () => {
      const cmd = 'echo "a && b" | head';
      expect(maybeStripCompressorPipe(cmd, true)).toEqual({ command: cmd, stripped: false });
    });

    test("does not split on && inside command substitution", () => {
      const result = maybeStripCompressorPipe(
        "echo $(printf a && printf b) | head && bun test | tail -1",
        true,
      );
      expect(result.command).toBe("echo $(printf a && printf b) | head && bun test");
      expect(result.stripped).toBe(true);
      expect(result.note).toContain("| tail -1");
      expect(result.note).not.toContain("head");
    });
  });

  test("strips top-level semicolon and || chain segments independently", () => {
    const semicolon = maybeStripCompressorPipe("cd a; bun test | grep fail", true);
    expect(semicolon.stripped).toBe(true);
    expect(semicolon.command).toBe("cd a; bun test");

    const orChain = maybeStripCompressorPipe("cd a || exit && bun test | grep fail", true);
    expect(orChain.stripped).toBe(true);
    expect(orChain.command).toBe("cd a || exit && bun test");
  });

  test("does not strip wc or intent-changing grep flags", () => {
    expect(maybeStripCompressorPipe("bun test | wc -l", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("bun test | rg --quiet fail", true).stripped).toBe(false);
    expect(maybeStripCompressorPipe("bun test | grep -n fail", true).stripped).toBe(true);
  });

  test("does not treat || as a pipe", () => {
    expect(maybeStripCompressorPipe("bun test || true | grep fail", true).stripped).toBe(false);
  });

  test("strips test/build runners across ecosystems (JS/Rust/Go/JVM/.NET/Ruby/PHP/Swift/Deno)", () => {
    const runners = [
      "yarn test:unit | grep FAIL",
      "deno test | grep fail",
      "gradle test | grep FAILED",
      "./gradlew clean test | tail -20",
      "mvn verify | grep ERROR",
      "./mvnw test | head",
      "dotnet test | grep Failed",
      "rspec | grep fail",
      "rake test | tail",
      "phpunit | tail",
      "./vendor/bin/phpunit | grep FAIL",
      "swift test | grep fail",
      "xcodebuild test | tail -5",
      "make test | grep Error",
      "tox | tail",
      "node_modules/.bin/jest | grep fail",
    ];
    for (const cmd of runners) {
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(true);
    }
  });

  test("does NOT strip log/search tools where the downstream filter is the intent", () => {
    // These are the false-positive guard: stripping them would change behavior.
    const keep = [
      "git log | grep fix",
      "docker logs app | tail -100",
      "kubectl logs pod | grep error",
      "make | grep error", // bare make = generic build, no test/lint target
      "cat app.log | grep ERROR",
      "journalctl -u svc | tail",
    ];
    for (const cmd of keep) {
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    }
  });

  // --- Gap 1: env-var-prefixed runners ---
  describe("env-var-prefixed runners", () => {
    test("strips CI=1 bun test piped through grep", () => {
      const result = maybeStripCompressorPipe("CI=1 bun test | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("CI=1 bun test");
    });

    test("strips FOO=bar npm test piped through tail", () => {
      const result = maybeStripCompressorPipe("FOO=bar npm test | tail -5", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("FOO=bar npm test");
    });

    test("strips multiple env-var assignments before runner", () => {
      const result = maybeStripCompressorPipe("CI=1 NODE_ENV=test bun test | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("CI=1 NODE_ENV=test bun test");
    });

    test("strips env-var prefix with cd && chain", () => {
      const result = maybeStripCompressorPipe("cd pkg && CI=1 bun test | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("cd pkg && CI=1 bun test");
    });

    test("strips env-var prefix for cargo test", () => {
      const result = maybeStripCompressorPipe("RUST_BACKTRACE=1 cargo test | grep FAILED", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("RUST_BACKTRACE=1 cargo test");
    });

    test("does NOT strip VAR=$(cmd) env-var with command substitution", () => {
      const cmd = "VAR=$(echo x) bun test | grep fail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });

    test("does NOT strip env-var prefix when filter has redirection", () => {
      const cmd = "CI=1 bun test | grep fail > out.txt";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });

    test("does NOT strip env-var prefix when runner is not recognized", () => {
      const cmd = "CI=1 ls | grep foo";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });

    test("does NOT strip when value contains backtick command substitution", () => {
      const cmd = "VAR=`date` bun test | grep fail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });
  });

  // --- Gap 2: bun --cwd ---
  describe("bun --cwd flag", () => {
    test("strips bun --cwd <pkg> test piped through grep", () => {
      const result = maybeStripCompressorPipe("bun --cwd packages/app test | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("bun --cwd packages/app test");
    });

    test("strips bun --cwd=<pkg> test piped through tail", () => {
      const result = maybeStripCompressorPipe("bun --cwd=packages/app test | tail -5", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("bun --cwd=packages/app test");
    });

    test("strips bun --cwd <pkg> run test:unit", () => {
      const result = maybeStripCompressorPipe("bun --cwd pkg run test:unit | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("bun --cwd pkg run test:unit");
    });

    test("does NOT strip bun --cwd without test subcommand", () => {
      const cmd = "bun --cwd packages/app build | grep fail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });
  });

  // --- Gap 3: mvn clean ---
  describe("mvn clean task", () => {
    test("strips mvn clean test piped through grep", () => {
      const result = maybeStripCompressorPipe("mvn clean test | grep ERROR", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("mvn clean test");
    });

    test("strips mvn clean verify", () => {
      const result = maybeStripCompressorPipe("mvn clean verify | tail -20", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("mvn clean verify");
    });

    test("strips ./mvnw clean test", () => {
      const result = maybeStripCompressorPipe("./mvnw clean test | grep FAIL", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("./mvnw clean test");
    });

    test("does NOT strip mvn clean deploy (stateful goal)", () => {
      const cmd = "mvn clean deploy | tail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });
  });

  // --- Gap 4: rake multi-task ---
  describe("rake multi-task", () => {
    test("strips rake db:setup test (multi-task with test)", () => {
      const result = maybeStripCompressorPipe("rake db:setup test | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("rake db:setup test");
    });

    test("strips rake db:migrate spec (multi-task with spec)", () => {
      const result = maybeStripCompressorPipe("rake db:migrate spec | tail -10", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("rake db:migrate spec");
    });

    test("strips rake with flags and test task", () => {
      const result = maybeStripCompressorPipe("rake --trace test | grep fail", true);
      expect(result.stripped).toBe(true);
      expect(result.command).toBe("rake --trace test");
    });

    test("does NOT strip rake deploy (no test/spec task)", () => {
      const cmd = "rake deploy | tail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });

    test("does NOT strip rake db:setup (no test/spec task)", () => {
      const cmd = "rake db:setup | tail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });

    test("does NOT strip rake with path-like operand", () => {
      const cmd = "rake test some/path | tail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });

    test("still does NOT strip rake test_db_reset", () => {
      const cmd = "rake test_db_reset | tail";
      expect(maybeStripCompressorPipe(cmd, true).stripped).toBe(false);
    });
  });
});
