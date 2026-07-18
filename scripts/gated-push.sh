#!/usr/bin/env bash
# Run a verification command BARE (no pipes — a pipe's exit code replaces the
# gated command's and has pushed red builds), then push only on success.
#
# Usage: scripts/gated-push.sh [--remote origin] [--branch main] -- <gate command...>
# Example: scripts/gated-push.sh -- cargo test -p agent-file-tools --lib
set -euo pipefail

remote="origin"
branch="main"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --remote) remote="$2"; shift 2 ;;
    --branch) branch="$2"; shift 2 ;;
    --) shift; break ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ $# -eq 0 ]]; then
  echo "no gate command given" >&2
  exit 2
fi

echo "gated-push: running gate: $*"
"$@"
rc=$?
if [[ $rc -ne 0 ]]; then
  echo "gated-push: gate FAILED (rc=$rc) — not pushing" >&2
  exit "$rc"
fi

echo "gated-push: gate green — pushing to $remote $branch"
git push "$remote" "$branch"
