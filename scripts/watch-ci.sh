#!/usr/bin/env bash
# Watch a CI run and FAIL FAST: exit the moment any job concludes 'failure',
# without waiting for the rest of the run. Exit 0 only when the whole run
# succeeds. Prints the first failing job's failed-test lines on the way out.
#
# Usage:
#   scripts/watch-ci.sh            # newest run on main
#   scripts/watch-ci.sh <run-id>
set -uo pipefail

REPO="${REPO:-cortexkit/aft}"
RID="${1:-}"
if [ -z "$RID" ]; then
  RID=$(gh run list --repo "$REPO" --branch main --limit 1 --json databaseId --jq '.[0].databaseId')
fi
echo "watching run $RID (fail-fast)"

while true; do
  STATUS=$(gh run view "$RID" --repo "$REPO" --json status --jq '.status' 2>/dev/null || echo poll-error)
  FAILED_JOB=$(gh run view "$RID" --repo "$REPO" --json jobs \
    --jq '[.jobs[] | select(.conclusion=="failure")][0] | if . == null then "" else .name + "|" + (.databaseId|tostring) end' 2>/dev/null || echo "")

  if [ -n "$FAILED_JOB" ] && [ "$FAILED_JOB" != "null" ]; then
    NAME="${FAILED_JOB%%|*}"; JID="${FAILED_JOB##*|}"
    echo "CI_EARLY_FAIL job='$NAME' run=$RID"
    gh run view --repo "$REPO" --job "$JID" --log-failed 2>/dev/null \
      | grep -aE "FAIL \[|panicked at|error\[|bash startup failure" | head -8
    exit 1
  fi

  if [ "$STATUS" = "completed" ]; then
    CONC=$(gh run view "$RID" --repo "$REPO" --json conclusion --jq '.conclusion')
    echo "CI_DONE run=$RID conclusion=$CONC"
    [ "$CONC" = "success" ] && exit 0 || exit 1
  fi

  sleep 45
done
