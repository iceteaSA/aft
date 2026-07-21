#!/usr/bin/env bash
set -euo pipefail

runner="${AFT_RUST_TEST_RUNNER:-nextest}"

if [[ "$runner" == "cargo" ]]; then
  exec cargo test --workspace --quiet
fi

if [[ "$runner" != "nextest" ]]; then
  echo "Unsupported AFT_RUST_TEST_RUNNER='$runner' (expected 'nextest' or 'cargo')" >&2
  exit 2
fi

if ! command -v cargo-nextest >/dev/null 2>&1; then
  echo "cargo-nextest is required; install it with: cargo install cargo-nextest --locked" >&2
  exit 127
fi

run_phase() {
  local label="$1"
  shift
  local started=$SECONDS

  echo "==> $label"
  "$@"
  echo "    ok ($((SECONDS - started))s)"
}

# `cargo test --workspace -- --list` currently reports zero doctests for both
# workspace crates (`aft` and `aft_tokenizer`), so the split gate omits
# `cargo test --workspace --doc` until doctests actually exist.
run_phase "cargo test --workspace --lib --bins --quiet" \
  cargo test --workspace --lib --bins --quiet

# macOS: the first exec of a freshly-linked binary pays a one-time
# syspolicyd assessment + XProtect scan tax (measured 1-4s cold, ~25ms once
# cached per inode). nextest exec's EVERY test-harness binary in
# target/*/deps, and the integration tests additionally spawn
# target/debug/aft. Without warming, the first wave of those cold execs
# queues behind the busy scanner during the TIMED run and dies together at
# the per-test timeout (the 16-test SIGTERM-at-400s storm). Pay the tax HERE,
# untimed: ad-hoc sign (cuts the cold tax ~3.5x) + exec-once every test
# binary so the timed run hits the warm 25ms path.
#
# NOTE: an earlier version tried `pkill XprotectService/syspolicyd` on a slow
# probe — that is a no-op without sudo (both run as root; pkill returns
# "Operation not permitted"). The real, sudo-free lever is sign+warm below.
# Opt out with AFT_GATE_NO_XPROTECT_REMEDIATION=1.
warm_macos_test_binaries() {
  # Ask cargo for the EXACT set of test-harness executables it built (the
  # `executable` field in the build JSON — ~24 binaries, not the thousands of
  # incremental fragments under deps/). Ad-hoc sign each (cuts the cold tax
  # ~3.5x) and exec `--list` once (pays + caches the assessment without
  # running tests). $@ = the cargo build args that define the profile/scope.
  local bins
  bins="$(cargo test "$@" --no-run --message-format=json 2>/dev/null | python3 -c "
import sys, json
seen = set()
for line in sys.stdin:
    try: o = json.loads(line)
    except Exception: continue
    e = o.get('executable')
    if e: seen.add(e)
for p in sorted(seen): print(p)
")"
  local bin
  while IFS= read -r bin; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    codesign -f -s - "$bin" 2>/dev/null || true
    "$bin" --list >/dev/null 2>&1 || true
  done <<< "$bins"
  # The CLI binary is spawned as a subprocess by integration tests but is not
  # a test harness, so it never appears as an `executable`; warm it explicitly.
  if [[ -x target/debug/aft ]]; then
    codesign -f -s - target/debug/aft 2>/dev/null || true
    target/debug/aft --version >/dev/null 2>&1 || true
  fi
}
if [[ "$(uname)" == "Darwin" && "${AFT_GATE_NO_XPROTECT_REMEDIATION:-}" != "1" ]]; then
  run_phase "warm macOS exec assessment: sign + warm every debug test binary" \
    bash -c "$(declare -f warm_macos_test_binaries)
      warm_macos_test_binaries --workspace"
fi

run_phase "cargo nextest run --workspace -E kind(test) - binary(=watcher_integration)" \
  cargo nextest run --workspace -E 'kind(test) - binary(=watcher_integration)'

run_phase "cargo test -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1" \
  cargo test -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1

# The main subc storm test asserts production-calibrated absolute latencies
# (2s bind headroom, the module's real 12s bind deadline). It is
# debug-ignored because an unoptimized build under load cannot honor those
# bounds even when the code is correct; the release profile is the
# authoritative calibration (measured ~14s for the whole storm suite).
# Skippable because the 2-core Windows CI runner can neither afford the
# cold release-profile build inside the job timeout nor honor absolute
# latency bounds — Linux and macOS CI remain the release-storm arbiters.
if [[ "${AFT_GATE_SKIP_RELEASE_STORM:-}" == "1" ]]; then
  echo "==> release-storm phase skipped (AFT_GATE_SKIP_RELEASE_STORM=1)"
else
  run_phase "cargo nextest run --cargo-profile release -E 'test(subc_storm)' (release-calibrated latency bounds)" \
    cargo nextest run --cargo-profile release -p agent-file-tools --test integration -E 'test(subc_storm)'
fi
