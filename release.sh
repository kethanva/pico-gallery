#!/usr/bin/env bash
# ── release.sh ────────────────────────────────────────────────────────────────
# Automates the entire release process: version bumping, staging, tagging, and pushing.
# 
# Usage:
#   ./release.sh                  # Increments patch version, uses default message
#   ./release.sh "message"        # Increments patch version, uses custom message
#   ./release.sh 1.2.3 "message"  # Sets specific version and message
# ──────────────────────────────────────────────────────────────────────────────

set -euo pipefail

# ── Helpers ───────────────────────────────────────────────────────────────────
blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }
die()   { red "ERROR: $*" >&2; exit 1; }

# ── Environment Checks ───────────────────────────────────────────────────────
[[ -f Cargo.toml ]] || die "Cargo.toml not found. Run this from the project root."
git rev-parse --is-inside-work-tree &>/dev/null || die "Not a git repository."

# ── Sync with Remote ─────────────────────────────────────────────────────────
blue "Syncing with remote..."
git fetch --tags origin || blue "Warning: Could not fetch from origin. Continuing locally..."

# ── Determine Version ────────────────────────────────────────────────────────
# Check if first arg is a version number (X.Y.Z)
if [[ $# -gt 0 && "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    VERSION="$1"
    shift
else
    # Find the latest tag that looks like a version
    LAST_TAG=$(git tag -l "v*" --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n 1 || true)
    
    if [[ -n "$LAST_TAG" ]]; then
        # Increment patch version
        IFS='.' read -r major minor patch <<< "${LAST_TAG#v}"
        VERSION="$major.$minor.$((patch + 1))"
        blue "Auto-incrementing from last tag $LAST_TAG -> $VERSION"
    else
        # Fallback to current version in Cargo.toml
        VERSION=$(grep -m 1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
        blue "No prior tags found. Using version from Cargo.toml: $VERSION"
    fi
fi

TAG="v$VERSION"

# ── Bump Version in Cargo.toml ───────────────────────────────────────────────
blue "Updating Cargo.toml to version $VERSION..."
# Portable sed handles both macOS (BSD) and Linux (GNU)
sed -i.bak "/^\[package\]/,/^version/ s/^version[[:space:]]*=[[:space:]]*\".*\"/version = \"$VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

# Update Cargo.lock if possible
if command -v cargo &>/dev/null; then
    blue "Updating Cargo.lock..."
    cargo generate-lockfile &>/dev/null || true
fi

# ── Commit and Tag ────────────────────────────────────────────────────────────
blue "Staging changes..."
git add .

MSG="${*:-chore: release $TAG}"

# Only commit if there are changes (including the version bump)
if git diff --cached --quiet; then
    blue "No changes detected. Is the code already at $VERSION?"
else
    blue "Committing: $MSG"
    git commit -m "$MSG"
fi

# Check for existing tag
if git tag -l | grep -qx "$TAG"; then
    blue "Tag $TAG already exists locally. Skipping tag creation."
else
    blue "Creating tag $TAG..."
    git tag -a "$TAG" -m "$MSG"
fi

# ── Push to GitHub ────────────────────────────────────────────────────────────
BRANCH=$(git rev-parse --abbrev-ref HEAD)
blue "Pushing $BRANCH and tags to origin..."
git push origin "$BRANCH" --follow-tags

# ── Resolve repo slug (owner/name) from origin URL ────────────────────────────
ORIGIN_URL=$(git remote get-url origin)
REPO_SLUG=$(echo "$ORIGIN_URL" | sed -E 's|.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$|\1|')

# ── Wait for the CI workflow triggered by this tag ────────────────────────────
if ! command -v gh &>/dev/null; then
    blue "gh CLI not installed — skipping release verification."
    blue "Watch progress at: https://github.com/${REPO_SLUG}/actions"
    green "✅ Tag $TAG pushed. Release will be created by CI."
    exit 0
fi

blue "Locating CI workflow run for $TAG..."
RUN_ID=""
for _ in 1 2 3 4 5; do
    sleep 3
    RUN_ID=$(gh run list \
        --repo "$REPO_SLUG" \
        --event push \
        --limit 20 \
        --json databaseId,headBranch,status \
        --jq ".[] | select(.headBranch == \"$TAG\") | .databaseId" 2>/dev/null | head -1)
    [[ -n "$RUN_ID" ]] && break
done

if [[ -z "$RUN_ID" ]]; then
    blue "Could not find workflow run for $TAG. Check https://github.com/${REPO_SLUG}/actions"
    green "✅ Tag $TAG pushed."
    exit 0
fi

blue "Watching run $RUN_ID (this takes 5-10 min for ARM cross-builds)..."
if ! gh run watch "$RUN_ID" --repo "$REPO_SLUG" --exit-status; then
    die "CI workflow for $TAG failed. See https://github.com/${REPO_SLUG}/actions/runs/${RUN_ID}"
fi

# ── Verify the release has assets ─────────────────────────────────────────────
blue "Verifying release assets..."
ASSET_COUNT=$(gh release view "$TAG" --repo "$REPO_SLUG" --json assets --jq '.assets | length' 2>/dev/null || echo 0)
if [[ "$ASSET_COUNT" -gt 0 ]]; then
    green "✅ Release $TAG is live on GitHub with $ASSET_COUNT asset(s)."
else
    die "CI finished but release $TAG has no assets. Check https://github.com/${REPO_SLUG}/releases"
fi

echo
green "Install on Pi with:"
echo "  curl -sSL https://raw.githubusercontent.com/${REPO_SLUG}/main/install.sh | bash"
