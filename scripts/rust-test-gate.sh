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

run_phase "cargo nextest run --workspace -E kind(test) - binary(=watcher_integration)" \
  cargo nextest run --workspace -E 'kind(test) - binary(=watcher_integration)'

run_phase "cargo test -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1" \
  cargo test -p agent-file-tools --test watcher_integration --quiet -- --test-threads=1

# The main subc storm test asserts production-calibrated absolute latencies
# (2s bind headroom, the module's real 12s bind deadline). It is
# debug-ignored because an unoptimized build under load cannot honor those
# bounds even when the code is correct; the release profile is the
# authoritative calibration (measured ~14s for the whole storm suite).
run_phase "cargo nextest run --cargo-profile release -E 'test(subc_storm)' (release-calibrated latency bounds)" \
  cargo nextest run --cargo-profile release -p agent-file-tools --test integration -E 'test(subc_storm)'
