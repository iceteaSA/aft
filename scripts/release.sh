#!/usr/bin/env bash
set -euo pipefail

# release.sh — Tag and push a new AFT release
#
# Usage:
#   ./scripts/release.sh 0.2.0        # release v0.2.0
#   ./scripts/release.sh 0.2.0 --dry  # preview without committing/pushing
#
# What it does:
#   1. Validates the version is semver
#   2. Checks for clean working tree (no uncommitted changes)
#   3. Syncs version across all 7 package files
#   4. Commits the version bump
#   5. Creates a git tag (v0.2.0)
#   6. Pushes commit + tag to origin
#   7. CI takes over: test → build → publish npm + GitHub release

VERSION="${1:-}"
DRY="${2:-}"

if [[ -z "$VERSION" ]]; then
  echo "Usage: ./scripts/release.sh <version> [--dry]"
  echo "  e.g. ./scripts/release.sh 0.2.0"
  exit 1
fi

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.]+)?(\+[a-zA-Z0-9.]+)?$ ]]; then
  echo "Error: '$VERSION' is not valid semver (expected X.Y.Z)"
  exit 1
fi

TAG="v$VERSION"
CURRENT_HEAD=$(git rev-parse HEAD)
RESUME_RELEASE=0

# Check if tag already exists. If it points at HEAD, this is a resumable
# release: commit/tag creation already succeeded and only the push/CI handoff
# needs another attempt.
check_existing_tag_for_resume() {
  local source="$1"
  local tag_commit

  tag_commit=$(git rev-list -n 1 "$TAG")
  if [[ "$tag_commit" == "$CURRENT_HEAD" ]]; then
    echo "→ Tag '$TAG' already exists on $source at HEAD; resuming release push."
    RESUME_RELEASE=1
    return
  fi

  echo "Error: tag '$TAG' already exists on $source but points at $tag_commit"
  echo "       current HEAD is $CURRENT_HEAD"
  echo "       Refusing to reuse a release tag from a different commit."
  exit 1
}

if git show-ref --verify --quiet "refs/tags/$TAG"; then
  check_existing_tag_for_resume "local"
elif git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1; then
  echo "→ Tag '$TAG' exists on origin; fetching to check resume state..."
  git fetch --quiet origin "refs/tags/$TAG:refs/tags/$TAG"
  check_existing_tag_for_resume "origin"
fi

# Check for clean working tree
if [[ -n "$(git status --porcelain)" ]]; then
  echo "Error: working tree is not clean — commit or stash changes first"
  git status --short
  exit 1
fi

# Check we're on main
BRANCH=$(git branch --show-current)
if [[ "$BRANCH" != "main" ]]; then
  echo "Warning: releasing from '$BRANCH' (not main)"
  read -rp "Continue? [y/N] " confirm
  if [[ "$confirm" != "y" && "$confirm" != "Y" ]]; then
    echo "Aborted."
    exit 1
  fi
fi

echo ""
echo "  Releasing AFT $TAG"
echo "  ─────────────────────"
echo ""

push_release() {
  echo "→ Pushing to origin..."
  git push origin "$BRANCH"
  git push origin "$TAG"
  echo ""

  echo "  ✓ Released $TAG"
  echo "  → GitHub Actions will now: test → build → publish"
  echo "  → Watch: https://github.com/cortexkit/aft/actions"
}

if [[ "$RESUME_RELEASE" == "1" ]]; then
  echo "→ Resume mode: skipping local checks, version sync, commit, and tag creation."
  if [[ "$DRY" == "--dry" ]]; then
    echo "[DRY RUN] Would push branch '$BRANCH' and tag '$TAG' to origin."
    exit 0
  fi
  echo ""
  push_release
  exit 0
fi

# ─── Static release-content checks ───
#
# Run BEFORE the dry-run early-exit so `release.sh <ver> --dry` also catches
# missing release notes and stale announcement text. These checks don't
# mutate anything; they're pure read-only validation.
#
# We keep forgetting to update the in-plugin "what's new" dialog for new
# minor releases. The dialog text lives in `ANNOUNCEMENT_VERSION` and
# `ANNOUNCEMENT_FEATURES` constants in both plugin entry files, and it
# only re-fires when `ANNOUNCEMENT_VERSION` is bumped (PLUGIN_VERSION
# alone doesn't trigger it — that's by design so patch releases don't
# spam dismissed dialogs).
#
# Release notes drafts live at `.alfonso/release-notes/v$VERSION.md` per
# the standing workflow rule; CI uses this exact path as the GitHub
# release body. Missing notes mean we'd ship blank GitHub + Discord
# release announcements.
echo "→ Release-content checks..."

NOTES_FILE=".alfonso/release-notes/v$VERSION.md"
if [[ ! -s "$NOTES_FILE" ]]; then
  echo "Error: release notes draft missing or empty at $NOTES_FILE"
  echo "  → Drafts must exist BEFORE release.sh runs. See standing workflow rule."
  LATEST_PRIOR=$(find .alfonso/release-notes -name 'v*.md' 2>/dev/null | sed 's|.*/v||;s|\.md$||' | sort -V | tail -1 || true)
  if [[ -n "$LATEST_PRIOR" ]]; then
    echo "  → Copy a prior release's notes as a template and edit:"
    echo "    cp .alfonso/release-notes/v${LATEST_PRIOR}.md $NOTES_FILE"
  fi
  exit 1
fi

ANN_OC=$(grep -oE 'ANNOUNCEMENT_VERSION = "[^"]*"' packages/opencode-plugin/src/index.ts 2>/dev/null | head -1 | sed -E 's/.*"([^"]*)"/\1/')
ANN_PI=$(grep -oE 'ANNOUNCEMENT_VERSION = "[^"]*"' packages/pi-plugin/src/index.ts 2>/dev/null | head -1 | sed -E 's/.*"([^"]*)"/\1/')

if [[ -z "$ANN_OC" ]] || [[ -z "$ANN_PI" ]]; then
  echo "Error: could not find ANNOUNCEMENT_VERSION constant in one of:"
  echo "  packages/opencode-plugin/src/index.ts -> '$ANN_OC'"
  echo "  packages/pi-plugin/src/index.ts        -> '$ANN_PI'"
  echo "  → Either the constant was renamed or the file moved. Update this check."
  exit 1
fi

# Drift detection — both plugins must always agree, regardless of patch vs minor.
if [[ "$ANN_OC" != "$ANN_PI" ]]; then
  echo "Error: ANNOUNCEMENT_VERSION drift between plugins:"
  echo "  opencode-plugin: '$ANN_OC'"
  echo "  pi-plugin:       '$ANN_PI'"
  echo "  → Both plugins must show the same release announcement. Fix and retry."
  exit 1
fi

# For X.Y.0 minor (or major) releases, require ANNOUNCEMENT_VERSION == $VERSION.
# Patch releases keep the prior announcement so previously-dismissed dialogs
# don't re-fire — that's intentional.
PATCH_LEVEL="${VERSION##*.}"
if [[ "$PATCH_LEVEL" == "0" ]] && [[ "$ANN_OC" != "$VERSION" ]]; then
  echo "Error: cutting minor release v$VERSION but ANNOUNCEMENT_VERSION is still '$ANN_OC'"
  echo ""
  echo "  Update BOTH plugins to surface a fresh 'what's new' dialog for v$VERSION:"
  echo "    packages/opencode-plugin/src/index.ts  (ANNOUNCEMENT_VERSION + ANNOUNCEMENT_FEATURES)"
  echo "    packages/pi-plugin/src/index.ts        (ANNOUNCEMENT_VERSION + ANNOUNCEMENT_FEATURES)"
  echo ""
  echo "  ANNOUNCEMENT_FEATURES is a string[] of user-facing bullet points — short, no internal noise."
  echo "  ANNOUNCEMENT_FOOTER already carries the Discord invite, so no need to repeat it in features."
  echo ""
  echo "  To skip this check (e.g. emergency patch tagged as a minor by mistake):"
  echo "    SKIP_ANNOUNCEMENT_CHECK=1 ./scripts/release.sh $VERSION"
  if [[ "${SKIP_ANNOUNCEMENT_CHECK:-}" != "1" ]]; then
    exit 1
  fi
  echo "  (skipping because SKIP_ANNOUNCEMENT_CHECK=1)"
fi

echo "    ✓ release notes draft present ($NOTES_FILE)"
echo "    ✓ announcement version: $ANN_OC (both plugins)"
echo ""

# Step 1: Sync versions
if [[ "$DRY" == "--dry" ]]; then
  echo "→ Version sync (dry run):"
  bun scripts/version-sync.mjs "$VERSION" --dry-run
  echo ""
  echo "[DRY RUN] Would commit, tag $TAG, and push to origin."
  exit 0
fi

echo "→ Running pre-release checks..."
echo ""

# Fast static checks FIRST — they take seconds and catch the cheap mistakes
# (lint/format/types) that otherwise only surface AFTER the ~15min cargo test
# + docker e2e have already run. Fail fast on the cheap stuff.
echo "  bun lint..."
bun run lint 2>&1 || { echo "Error: Lint failed"; exit 1; }

echo "  bun typecheck..."
bun run typecheck 2>&1 || { echo "Error: Typecheck failed"; exit 1; }

echo "  cargo fmt --check..."
cargo fmt --check 2>&1 || { echo "Error: cargo fmt --check failed (run 'cargo fmt')"; exit 1; }

# Slow checks AFTER the fast ones pass.
#
# TRUST_CI=1: skip the local Rust/JS/docker test re-run when CI is already
# green on EXACTLY this commit. The local suite duplicates CI's coverage and
# on macOS randomly loses 30-90 minutes to exec-assessment wedges; CI on the
# same SHA is strictly more coverage (3 OS × unit + e2e). Verified, never
# assumed: gh must report a completed successful run for HEAD's SHA.
if [ "${TRUST_CI:-}" = "1" ]; then
  head_sha=$(git rev-parse HEAD)
  ci_ok=$(gh run list --commit "$head_sha" --workflow Tests --json status,conclusion \
    --jq '[.[] | select(.status == "completed" and .conclusion == "success")] | length' 2>/dev/null || echo 0)
  if [ "${ci_ok:-0}" -ge 1 ]; then
    echo "  TRUST_CI: CI is green on $head_sha — skipping local Rust/JS/docker test re-run"
    SKIP_RUST_TESTS=1
    SKIP_JS_TESTS=1
    SKIP_DOCKER_E2E=1
  else
    echo "Error: TRUST_CI=1 but no completed successful Tests run found for $head_sha"
    echo "  ↳ push the commit, let CI finish green, then retry (or drop TRUST_CI)."
    exit 1
  fi
fi

if [ "${SKIP_RUST_TESTS:-}" = "1" ]; then
  echo "  (skipping local cargo tests — SKIP_RUST_TESTS=1)"
  echo "  ↳ CI still runs the full suite on its own runners before publishing."
else
  echo "  rust test gate (unit on libtest, integration on nextest, watcher isolated)..."
  ./scripts/rust-test-gate.sh 2>&1 || { echo "Error: Rust tests failed"; exit 1; }
fi

if [ "${SKIP_JS_TESTS:-}" = "1" ]; then
  echo "  (skipping plugin tests — SKIP_JS_TESTS=1)"
else
  echo "  bun test..."
  bun run test 2>&1 || { echo "Error: Plugin tests failed"; exit 1; }
fi

if [ "${SKIP_DOCKER_E2E:-}" = "1" ]; then
  echo "  (skipping docker e2e — SKIP_DOCKER_E2E=1)"
elif command -v docker &>/dev/null && docker info &>/dev/null 2>&1; then
  echo "  docker e2e (Linux x64)..."
  cleanup_fixture() {
    rm -f tests/docker/fixtures/aft-linux-x64
    rm -rf tests/docker/fixtures/npm-packs
  }
  trap cleanup_fixture EXIT
  # Build Linux x64 binary in Docker
  docker build --platform linux/amd64 -t aft-build-linux -f tests/docker/Dockerfile.build-linux . --quiet 2>&1 || { echo "Error: Docker Linux build failed"; exit 1; }
  # Extract binary to fixtures
  CID=$(docker create --platform linux/amd64 aft-build-linux true)
  docker cp "$CID:/build/target/release/aft" tests/docker/fixtures/aft-linux-x64
  docker rm "$CID" > /dev/null
  # Pack local plugin artifacts so the E2E image installs the code under test
  # (real package.json + dist + deps) instead of the published @latest plugin.
  # The Dockerfile COPYs tests/docker/fixtures/npm-packs/*.tgz; CI does the same
  # in _e2e-suite.yml. Without this the E2E image build fails on a missing dir.
  rm -rf tests/docker/fixtures/npm-packs
  mkdir -p tests/docker/fixtures/npm-packs
  npm pack ./packages/aft-bridge --pack-destination tests/docker/fixtures/npm-packs >/dev/null 2>&1 || { echo "Error: npm pack aft-bridge failed"; exit 1; }
  npm pack ./packages/opencode-plugin --pack-destination tests/docker/fixtures/npm-packs >/dev/null 2>&1 || { echo "Error: npm pack opencode-plugin failed"; exit 1; }
  # Build E2E test image
  docker build --platform linux/amd64 -t aft-e2e-linux-x64 -f tests/docker/Dockerfile.linux-x64 . --quiet 2>&1 || { echo "Error: Docker E2E image build failed"; exit 1; }
  # Run E2E test
  docker run --rm --platform linux/amd64 aft-e2e-linux-x64 2>&1 || { echo "Error: Docker E2E tests failed"; exit 1; }
  # Clean up extracted binary + packed tarballs (don't commit them)
  rm -f tests/docker/fixtures/aft-linux-x64
  rm -rf tests/docker/fixtures/npm-packs
  trap - EXIT
  echo "  ✓ Docker E2E passed"
else
  echo "  (skipping docker e2e — Docker not available)"
fi

echo "  ✓ All checks passed"
echo ""

echo "→ Syncing versions to $VERSION..."
bun scripts/version-sync.mjs "$VERSION"
echo ""

# Regenerate the JSON Schema asset so editor `$schema` URLs always resolve
# to a schema that matches the shipped config surface for this release.
echo "→ Rebuilding aft.schema.json..."
bun packages/opencode-plugin/scripts/build-schema.ts
echo ""

# Refresh bun.lock so workspace package versions match the bumped
# package.json files. Without this, CI's `bun install --frozen-lockfile`
# fails because the lockfile still pins the old versions.
echo "→ Refreshing bun.lock..."
bun install --silent 2>&1 || { echo "Error: bun install failed after version sync"; exit 1; }
echo ""

# Step 2: Commit (skip if versions were already at target)
echo "→ Committing version bump..."
git add -A
if git diff --cached --quiet; then
  echo "  (no changes — versions already at $VERSION)"
else
  git commit -m "release: $TAG"
fi

# Step 3: Tag
echo "→ Rebuilding local binary with new version..."
cargo build --release -p agent-file-tools --quiet 2>&1 || { echo "Error: Release build failed"; exit 1; }

# Update versioned cache only — never write to the flat cache path because
# other instances may be running a binary from there.
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/aft/bin"
if mkdir -p "$CACHE_DIR/$TAG" && cp target/release/aft "$CACHE_DIR/$TAG/aft" 2>/dev/null; then
  # macOS Sequoia SIGKILLs a freshly-built linker-signed-only binary on exec
  # (Gatekeeper/library validation). cargo's output is only linker-signed, so
  # ad-hoc re-sign it — mirroring dev-rebuild.sh — or the running session's
  # next bridge spawn dies with "Binary killed by SIGKILL".
  if [[ "$(uname -s)" == "Darwin" ]]; then
    codesign --force --sign - "$CACHE_DIR/$TAG/aft" 2>/dev/null || true
  fi
  echo "  Updated $CACHE_DIR/$TAG/aft"
fi

echo "→ Creating tag $TAG..."
git tag -a "$TAG" -m "Release $TAG"
echo ""

# Step 4: Push
push_release
