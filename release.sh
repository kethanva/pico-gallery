#!/usr/bin/env bash
# ── release.sh ────────────────────────────────────────────────────────────────
# Automates the release process: version bump, tag, push, and CI verification.
#
# Usage:
#   ./release.sh                         # patch bump from latest tag
#   ./release.sh "message"               # patch bump, custom message
#   ./release.sh 1.2.3 "message"       # explicit version
#   ./release.sh --prerelease            # feature-branch pre-release (vX.Y.Z-kva.N)
#   ./release.sh --prerelease 0.1.3-kva.1 "message"
#   ./release.sh --no-wait ...           # push tag and exit (don't wait for CI)
#
# From kva-revamparchitecture: every push auto-publishes vX.Y.Z-kva.N via CI.
# Manual pre-release (optional — same result as pushing the branch):
#   ./release.sh --prerelease
#   # then on the Pi:
#   sudo PICOGALLERY_VERSION=v0.1.3-kva.1 bash deploy.sh
# ──────────────────────────────────────────────────────────────────────────────

set -euo pipefail

# ── Parse Flags ──────────────────────────────────────────────────────────────
WAIT_FOR_CI=1
PRERELEASE=0
PRERELEASE_SUFFIX=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-wait|-n)     WAIT_FOR_CI=0; shift ;;
    --prerelease|-p)
      PRERELEASE=1
      shift
      # Optional explicit suffix: --prerelease kva.1
      if [[ $# -gt 0 && "$1" != --* && ! "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
        PRERELEASE_SUFFIX="$1"
        shift
      fi
      ;;
    *) break ;;
  esac
done

# ── Helpers ───────────────────────────────────────────────────────────────────
blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }
die()   { red "ERROR: $*" >&2; exit 1; }

# ── Environment Checks ───────────────────────────────────────────────────────
[[ -f Cargo.toml ]] || die "Cargo.toml not found. Run this from the project root."
git rev-parse --is-inside-work-tree &>/dev/null || die "Not a git repository."

BRANCH=$(git rev-parse --abbrev-ref HEAD)

# ── Sync with Remote ─────────────────────────────────────────────────────────
blue "Syncing with remote..."
git fetch --tags origin || blue "Warning: Could not fetch from origin. Continuing locally..."

# ── Determine Version ────────────────────────────────────────────────────────
if [[ $# -gt 0 && "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-.+)?$ ]]; then
    VERSION="$1"
    shift
elif [[ $PRERELEASE == "1" ]]; then
    BASE=$(grep -m 1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
    MAX=0
    while IFS= read -r t; do
      n="${t##*-kva.}"
      if [[ "$n" =~ ^[0-9]+$ ]] && (( n > MAX )); then
        MAX=$n
      fi
    done < <(git tag -l "v${BASE}-kva.*")
    if [[ -n "$PRERELEASE_SUFFIX" && "$PRERELEASE_SUFFIX" =~ ^kva\.([0-9]+)$ ]]; then
      N="${BASH_REMATCH[1]}"
      while git tag -l | grep -qx "v${BASE}-kva.${N}"; do
        N=$((N + 1))
      done
    else
      N=$((MAX + 1))
    fi
    VERSION="${BASE}-kva.${N}"
    blue "Pre-release on branch $BRANCH → $VERSION"
else
    LAST_TAG=$(git tag -l "v*" --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n 1 || true)

    if [[ -n "$LAST_TAG" ]]; then
        IFS='.' read -r major minor patch <<< "${LAST_TAG#v}"
        VERSION="$major.$minor.$((patch + 1))"
        blue "Auto-incrementing from last tag $LAST_TAG -> $VERSION"
    else
        VERSION=$(grep -m 1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
        blue "No prior tags found. Using version from Cargo.toml: $VERSION"
    fi
fi

TAG="v$VERSION"

# ── Bump Cargo.toml for plain releases (not pre-release suffix tags) ─────────
if [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  blue "Updating Cargo.toml to version $VERSION..."
  sed -i.bak "/^\[package\]/,/^version/ s/^version[[:space:]]*=[[:space:]]*\".*\"/version = \"$VERSION\"/" Cargo.toml
  rm -f Cargo.toml.bak

  if command -v cargo &>/dev/null; then
      blue "Updating Cargo.lock..."
      cargo generate-lockfile &>/dev/null || true
  fi
fi

# ── Commit and Tag ────────────────────────────────────────────────────────────
blue "Staging changes..."
git add .

MSG="${*:-chore: release $TAG}"

if git diff --cached --quiet; then
    blue "No staged changes."
else
    blue "Committing: $MSG"
    git commit -m "$MSG"
fi

if git tag -l | grep -qx "$TAG"; then
    blue "Tag $TAG already exists locally. Skipping tag creation."
else
    blue "Creating tag $TAG..."
    git tag -a "$TAG" -m "$MSG"
fi

# ── Push to GitHub ────────────────────────────────────────────────────────────
blue "Pushing $BRANCH and tag $TAG to origin..."
git push origin "$BRANCH" --follow-tags

# ── Resolve repo slug ─────────────────────────────────────────────────────────
ORIGIN_URL=$(git remote get-url origin)
REPO_SLUG=$(echo "$ORIGIN_URL" | sed -E 's|.*github\.com[:/]([^/]+/[^/.]+)(\.git)?$|\1|')

if [[ "$WAIT_FOR_CI" == "0" ]]; then
    green "✅ Tag $TAG pushed. Skipping CI wait (--no-wait)."
    echo "Watch progress at: https://github.com/${REPO_SLUG}/actions"
    echo
    green "Deploy on Pi (no local build):"
    echo "  sudo PICOGALLERY_VERSION=$TAG bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/${REPO_SLUG}/${BRANCH}/deploy.sh)\""
    exit 0
fi

if ! command -v gh &>/dev/null; then
    blue "gh CLI not installed — skipping release verification."
    green "✅ Tag $TAG pushed. Release will be created by CI."
    exit 0
fi

blue "Locating CI workflow run for $TAG..."
RUN_ID=""
for _ in 1 2 3 4 5 6 7 8 9 10; do
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

blue "Watching run $RUN_ID (ARM cross-builds take ~5-10 min)..."
if ! gh run watch "$RUN_ID" --repo "$REPO_SLUG" --exit-status; then
    die "CI workflow for $TAG failed. See https://github.com/${REPO_SLUG}/actions/runs/${RUN_ID}"
fi

blue "Verifying release assets..."
ASSET_COUNT=$(gh release view "$TAG" --repo "$REPO_SLUG" --json assets --jq '.assets | length' 2>/dev/null || echo 0)
if [[ "$ASSET_COUNT" -gt 0 ]]; then
    green "✅ Release $TAG is live on GitHub with $ASSET_COUNT asset(s)."
else
    die "CI finished but release $TAG has no assets. Check https://github.com/${REPO_SLUG}/releases"
fi

echo
green "Deploy on Pi (no local build):"
echo "  sudo PICOGALLERY_VERSION=$TAG bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/${REPO_SLUG}/${BRANCH}/deploy.sh)\""
