# brush-parser vs. tree-sitter-bash scanner spike

Date: 2026-07-17

## Executive result

**Recommendation: do not adopt `brush-parser` as the AFT permission-scanner
substrate, and do not adopt it for pipe-strip/compress dispatch parsing.**

The standalone harness exercised 108 commands: 40 commands harvested from the
permission integration tests, 5 scanner-unit-test operand shapes, 32 commands
from the rewrite integration/parser tests, and 31 additional adversarial
commands. It calls AFT's actual `scan_with_project_root` function through a
path dependency; it does not reimplement the scanner verdict.

| Bucket | Count | Meaning in this corpus |
| --- | ---: | --- |
| AGREE | 86 | Same statically identifiable command names; this includes AFT's fail-closed wildcard for commandless side-effect forms and malformed input. |
| TS-BLIND | 2 | Brush exposes a command word that AFT's tree-sitter walk does not expose. Both cases still receive AFT's fail-closed wildcard, so neither is an observed bypass. |
| BRUSH-GAP | 1 | AFT exposes a nested command that brush's AST does not expose. |
| BOTH-LIMITED | 19 | Dynamic command construction, sourced-file execution, command substitution as a command word, or `eval`/`sh -c`-style evaluation prevents either syntax tree from proving the final command set. |

The important outcome is not that brush parses ordinary pipelines better—it
does—but that it is not a strictly more faithful permission substrate. It has
a concrete blind spot for command substitutions embedded in arithmetic
commands, while the current AFT scanner sees that nested command. The two
tree-sitter-blind declaration shapes are already protected by AFT's
zero-command-node fail-closed branch.

## Reproduction and harness

The committed probe is an independent Cargo project at
`spikes/brush-parser-spike/`. Its manifest contains an empty `[workspace]`
table, so Cargo does not attach it to AFT's root workspace. It has these
dependencies:

```toml
aft = { package = "agent-file-tools", path = "../../crates/aft" }
brush-parser = "0.4.0"
```

The harness parses every corpus entry with `brush_parser::Parser`, recursively
walks `Program`, `AndOrList`, `Pipeline`, simple/compound commands, process
substitutions, command substitutions, and redirections, and prints one record
per entry containing:

- brush command words and arguments, with `Word.loc` start/end indexes;
- pipeline stages and `&&`/`||` structure;
- file redirects, here-docs, here-strings, output-and-error redirects, and
  process substitutions;
- brush parse status/error;
- the actual AFT `PermissionAsk` list; and
- the bucket and reason.

Run it from the repository root with:

```text
cargo run --manifest-path spikes/brush-parser-spike/Cargo.toml
```

The run prints `corpus_entries=108` and ends with:

```text
BUCKET_COUNTS  AGREE=86  TS-BLIND=2  BRUSH-GAP=1  BOTH-LIMITED=19
```

The temporary paths in generated integration tests were normalized to stable
`/tmp/...` paths. This changes no shell syntax and avoids embedding a machine's
temporary directory in the corpus. No command is executed by the harness.

## Corpus coverage

The harvested cases include the following regression families:

| Source | Cases | Coverage |
| --- | ---: | --- |
| `bash_permissions_test.rs` | 40 | Echo-prefix regression, external paths, redirects, dynamic operands, source/dot, `cd &&`, pipes/xargs, grants/background, zero-command-node probes, heredoc, malformed input, Unicode whitespace. |
| `scan.rs` unit-test shapes | 5 | `$PWD`, device redirects, numeric/non-numeric `/dev/fd`, and the redirect operand forms used by the scanner. |
| `bash_rewrite_test.rs` plus parser tests | 32 | Grep/rg/find/cat/sed/ls commands, rejected chains/flags, quoting/escaping, append redirects, heredocs, substitutions, and variable expansion. |
| Adversarial additions | 31 | `${IFS}` command construction, parameter defaults/transforms/indirection, nested `$()` and backticks, assignment substitutions, arrays, `eval`, `bash -c`/`sh -c`, loops, conditionals, groups, `&&`/`||`/`;` chains, process substitutions, redirect substitutions, here-strings, arithmetic, and Windows-style arguments. |

The corpus output also records the full ordinary AGREE set. The non-AGREE
entries are detailed below because they are the cases that affect the
recommendation.

## TS-BLIND cases

These are scanner-node differences, not observed permission bypasses: AFT
returns `PermissionKind::Bash`, pattern `"*"` for both.

### `perm-readonly`: `readonly FOO=bar`

Brush produces a `SimpleCommand` with command word `readonly` and argument
`FOO=bar`. AFT's tree-sitter command walk finds no ordinary `command` node for
this declaration shape, so `scan_with_project_root` takes the
zero-command-node fail-closed path and asks for `*`. Brush is more specific
about the builtin, but AFT still blocks execution under a deny-all rule.

### `perm-declare-array`: `declare -A map=()`

Brush produces `declare` with `-A` and `map=()` arguments. Tree-sitter exposes
this as a declaration form rather than an ordinary command node, so AFT again
returns the fail-closed wildcard. This is a useful AST fidelity difference for
future diagnostics, but it is not a new bypass class in the hardened scanner.

The same zero-command-node protection was observed for pure redirects,
assignment-only input, arithmetic commands, and `[[ ... ]]`; those are counted
as AGREE because both sides produce no concrete command name and AFT explicitly
falls back to a wildcard.

## Brush-gap case

### `adv-arithmetic`: `(( $(id) ))`

Brush parses the outer construct as an `ArithmeticCommand` whose expression is
an `UnexpandedArithmeticExpr` containing raw text. Its public AST does not
expose the nested command substitution as a child command, so the harness sees
no brush command. AFT's tree-sitter walk descends into the substitution and
emits a bash ask for `id`.

This is the strongest negative result for replacing the scanner: a real
command-execution path is less visible through brush's public AST than through
the existing tree-sitter tree. A brush-based implementation would need a
second scanner for raw arithmetic-expression text, which recreates the parser
boundary the spike was intended to avoid.

Malformed `echo 'unterminated` is not counted as a brush gap: brush reports a
typed parse error and AFT also rejects the tree and returns its wildcard ask.

## BOTH-LIMITED cases

The 19 cases are:

```text
perm-source
perm-dot-source
perm-dollar-subst
perm-backtick-subst
perm-eval
perm-bash-c
perm-pwd
adv-echo-subst
adv-echo-ifs
adv-ifs-command
adv-variable-command
adv-indirect-command
adv-eval
adv-bash-c
adv-sh-c
adv-array-invoke
adv-array-declare
adv-parameter-default
adv-parameter-transform
```

For `source`/`.`, the source file can contain arbitrary commands. For
`eval`, `bash -c`, and `sh -c`, the string being evaluated is a second program
boundary. For `$CMD`, `${!CMD}`, `${CMD:-id}`, arrays, and `${IFS}` command
construction, the eventual command word depends on shell state and expansion
semantics. Brush and tree-sitter both identify useful syntax and nested static
commands where available, but neither can prove the final runtime command set
without evaluating the shell.

The echo-prefix additions were especially useful: `echo$(id)` and
`echo${IFS}id` are not treated as a safe literal echo by either implementation;
the current scanner emits asks for the dynamic outer word and nested `id` when
the nested command is an explicit substitution.

## Brush API and spans

The 0.4 AST is pleasant for structural inspection:

- `Program.complete_commands` contains compound lists;
- `AndOrList` exposes the first pipeline and `&&`/`||` continuations;
- `Pipeline.seq` exposes stages directly;
- `SimpleCommand.word_or_name` and suffix items expose raw `Word` values;
- `IoRedirect` distinguishes file, heredoc, herestring, and output/error
  redirects; and
- process substitutions and nested compound commands are represented rather
  than flattened into a generic node.

Command and argument words generally have `Word.loc`, and the harness prints
those locations. The location indexes are character indexes, not guaranteed
byte offsets: `git` followed by a two-byte non-breaking space is reported with
the character-count span. A permission prompt that slices the original UTF-8
source by bytes would need an explicit character-to-byte conversion.

There are also important prompt/rewrite gaps:

- `IoRedirect::location()` returns `None` in 0.4. A filename target often has a
  `Word.loc`, but the redirect operator, descriptor, and complete redirect do
  not have one unified source span.
- `Word.value` is raw syntax, not shell-expanded text. This is correct for a
  conservative scanner but does not resolve dynamic paths or command words.
- The arithmetic command expression is intentionally raw and is the source of
  the `adv-arithmetic` brush gap.
- Parse errors return an error rather than a recoverable tree; AFT's current
  fail-closed behavior can be layered on that, but the same security policy
  still has to be implemented by the caller.

These APIs are enough to inspect a pipeline, but not enough to safely rewrite
the original command while preserving every byte. In particular, pipe
operator spans are not exposed as a first-class AST item, and AST `Display`
round-tripping is not a source-preserving contract.

## oh-my-pi reference

The local oh-my-pi checkout was inspected at revision
`48241afcc49b28b5ca45a8d028ec5968df3ad29b`.

It does use brush in production, but the relevant standalone parser consumer is
not a permission scanner. `crates/pi-shell/src/minimizer/plan.rs` uses
`brush-parser` to classify output-minimizer inputs as single commands, safe
`&&`/`;` chains, pipelines, or opaque compound forms. It deliberately treats
pipes as opaque, rejects substitutions/process substitutions for reconstruction,
and reparses `Display` output before using a segmented command. It reads
`Word.value` and pipeline structure; it does not use command-word spans for
permission prompts.

The checkout also demonstrates version/API separation rather than a simple
drop-in substrate:

- the workspace's direct `brush-parser` dependency is `0.3`, and the minimizer
  calls the three-argument `Parser::new(..., &SourceInfo)` API;
- the vendored `brush-core` is package version `0.5.0` and depends on
  `brush-parser ^0.4.0`; its own parser wrapper calls the two-argument 0.4 API;
- the vendored `brush-core/Cargo.toml` is Cargo-generated and says the original
  `Cargo.toml.orig` is omitted. No `Cargo.toml.orig` is tracked in this clone,
  so there is no local original/fork diff to apply as an AFT patch; and
- no oh-my-pi source inspected implements a brush-parser permission-like command
  scanner.

The production integration is therefore evidence that brush can support a
careful, conservative structural minimizer when its limitations are explicitly
encoded—not evidence that its parser alone replaces a permission scanner.

## Dependency, MSRV, and platform assessment

### crates.io and git

`cargo info brush-parser` resolved crates.io `0.4.0`, which declares Rust
`1.88.0`. The reference git checkout at `e46b4ae` has the same `0.4.0` parser
crate under a workspace declaring edition 2024 and `rust-version = "1.88.0"`.
AFT's stated Rust floor is 1.82, so both the current crates.io release and the
current git source are a **hard MSRV blocker** without pinning/forking an older
compatible parser or raising AFT's floor.

The crates.io package also has a surprising runtime dependency footprint:
`cargo tree` shows these direct dependencies:

```text
bon cached indenter insta peg thiserror tracing utf8-chars
```

`insta` is a 996 KiB source checkout and pulls console/pest/tempfile-related
transitives. The reference git manifest lists `insta` under dev-dependencies,
so a git build may avoid that runtime edge, but it does not avoid the 1.88
MSRV/edition requirement. The isolated crates.io parser-only check resolved 85
packages and took 26.32 seconds from a clean target on this machine. The full
spike (including the AFT path dependency) took 28.04 seconds from a clean
target; this is a comparison point, not a claim that adding brush would add all
of AFT's existing cost.

### Windows

The committed spike plus AFT path dependency passed:

```text
cargo check --target x86_64-pc-windows-gnu
```

The same full command with `x86_64-pc-windows-msvc` was attempted but could not
compile the host's `ring` C dependency because this macOS environment has no
MSVC headers (`assert.h` missing). An isolated brush-parser-only project passed
the MSVC target check, so this failure is an AFT/toolchain cross-compilation
limitation rather than a brush-parser portability failure. The GNU result is
the meaningful repository gate requested for this report.

## Recommendation

1. **Scanner: do not adopt now.** The current tree-sitter scanner already
   agrees with brush on the ordinary and adversarial statically identifiable
   command set in this corpus, and its known zero-node shapes fail closed. The
   arithmetic-substitution gap means a brush port would not be a monotonic
   security improvement.
2. **Pipe-strip/compress dispatch: do not adopt now.** Brush has excellent
   pipeline/boolean structure, but the required source-preserving rewrite
   contract is not provided by `Display`, pipe operators do not have dedicated
   spans, and oh-my-pi's minimizer has to treat pipes/substitutions/compound
   syntax as opaque and reparse reconstructions. That is a stronger
   conservative policy than a direct parser replacement, not a drop-in API.
3. **Future option:** if AFT raises its MSRV or maintains a compatible fork,
   brush could be retained as an optional syntax/diagnostics oracle or a
   shadow-parser test oracle. Any security adoption would first need explicit
   arithmetic-expansion traversal, byte-span conversion, redirect-span
   handling, and differential tests for every current scanner regression.
