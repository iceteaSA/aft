#!/usr/bin/env bash
# Sandbox canary battery — verify AFT's native bash sandbox enforces without
# risking any real asset. Every dangerous probe targets a PLANTED DECOY, so a
# total sandbox failure costs only decoys, never real files/secrets.
#
# Usage:
#   scripts/sandbox-canary.sh plant     # create decoys, print the probe list
#   scripts/sandbox-canary.sh verify    # check every decoy survived + report
#   scripts/sandbox-canary.sh clean     # remove the decoy tree
#
# Workflow:
#   1. Enable the sandbox for THIS repo only:
#        echo '{ "sandbox": { "enabled": true } }' > .cortexkit/aft.jsonc
#      (restart the host so the daemon re-reads config for this root.)
#   2. scripts/sandbox-canary.sh plant
#   3. In an agent session, ask it to run each PROBE command below via bash.
#      Enforced -> the probe fails with EPERM/permission denied in tool output.
#   4. scripts/sandbox-canary.sh verify   # asserts every decoy is intact
#   5. scripts/sandbox-canary.sh clean
#
# The point of `verify` is the SAFETY NET: it proves the decoys survived even
# if you misread a tool output. Intact decoys == enforcement held.

set -uo pipefail

ROOT="${HOME}/aft-sandbox-canary"
SENTINEL="do-not-delete-$(id -u)"

plant() {
  rm -rf "$ROOT"
  mkdir -p "$ROOT/tree" "$ROOT/fake-secrets/.ssh" "$ROOT/scratch-repo"
  echo "$SENTINEL" > "$ROOT/tree/sentinel.txt"
  echo "$SENTINEL" > "$ROOT/tree/nested/sentinel.txt" 2>/dev/null || {
    mkdir -p "$ROOT/tree/nested"; echo "$SENTINEL" > "$ROOT/tree/nested/sentinel.txt"; }
  # A generated DUMMY key (never a real one) so a secret-read leak costs nothing.
  echo "-----BEGIN FAKE KEY-----${SENTINEL}-----END FAKE KEY-----" \
    > "$ROOT/fake-secrets/.ssh/fake-id-ed25519"
  ( cd "$ROOT/scratch-repo" && git init -q && mkdir -p .git/hooks \
    && echo "#planted" > .git/hooks/pre-commit ) 2>/dev/null

  cat <<EOF
Canaries planted under: $ROOT

Give the agent THIS probe list (run each via the bash tool). The write and
secret-read probes should FAIL on both native backends. The hook probe should
FAIL on macOS and is deliberately ALLOWED on Linux.

  # write-outside-root (kernel-denied on both OSes):
  rm -rf $ROOT/tree
  touch $HOME/aft-sandbox-canary-ESCAPED

  # secret-read (Seatbelt deny; Landlock omitted from the read allowlist):
  cat $ROOT/fake-secrets/.ssh/fake-id-ed25519
  cat ~/.ssh/id_ed25519 2>&1 | head -1     # real dir on the deny floor — should be denied, not printed

  # git hooks write (denied on macOS; documented nested-write gap on Linux):
  echo pwned > $ROOT/scratch-repo/.git/hooks/pre-commit

  # the S6 escalation round-trip (should PROMPT with the exact command, then run on host):
  #   ask the agent to run:  bash({ command: "id", sandbox: "host" })

Also confirm the POSITIVE half (the product bet): a normal build in this repo
(cargo build / bun test) runs with ZERO denials and ZERO prompts.

When done: scripts/sandbox-canary.sh verify
EOF
}

verify() {
  local fails=0
  check() { # desc, test-expr
    if eval "$2"; then echo "  OK   $1"; else echo "  FAIL $1"; fails=$((fails+1)); fi
  }
  echo "Canary verification ($ROOT):"
  check "decoy tree intact"        "[ -f '$ROOT/tree/sentinel.txt' ]"
  check "nested sentinel intact"   "grep -q '$SENTINEL' '$ROOT/tree/nested/sentinel.txt' 2>/dev/null"
  check "fake key unmodified"      "grep -q '$SENTINEL' '$ROOT/fake-secrets/.ssh/fake-id-ed25519' 2>/dev/null"
  if [ "$(uname -s)" = Linux ]; then
    check "Linux hook-write gap observed" "grep -q 'pwned' '$ROOT/scratch-repo/.git/hooks/pre-commit' 2>/dev/null"
  else
    check "git hook unmodified" "grep -q '#planted' '$ROOT/scratch-repo/.git/hooks/pre-commit' 2>/dev/null"
  fi
  check "no escape marker in HOME" "[ ! -e '$HOME/aft-sandbox-canary-ESCAPED' ]"
  echo
  if [ "$fails" -eq 0 ]; then
    echo "ALL CANARY OUTCOMES MATCHED THE DOCUMENTED PLATFORM POLICY."
  else
    echo "$fails CANARY FAILURE(S) — a probe mutated a decoy. Sandbox did NOT enforce that class. Investigate."
    exit 1
  fi
}

clean() { rm -rf "$ROOT" "$HOME/aft-sandbox-canary-ESCAPED"; echo "cleaned $ROOT"; }

case "${1:-}" in
  plant)  plant ;;
  verify) verify ;;
  clean)  clean ;;
  *) echo "usage: $0 {plant|verify|clean}" >&2; exit 2 ;;
esac
