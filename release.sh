#!/usr/bin/env bash
# release.sh — stage, commit, push, and tag a patch release (0.0.Z only)
#
# Usage:
#   ./release.sh            # auto-increments patch, prompts for message
#   ./release.sh "my msg"   # auto-increments patch, uses given message

set -euo pipefail

# ── Helpers ───────────────────────────────────────────────────────────────────

red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
blue()  { printf '\033[0;34m%s\033[0m\n' "$*"; }
die()   { red "ERROR: $*" >&2; exit 1; }

# ── Require clean working tree (except for tracked modifications) ─────────────

if ! git rev-parse --is-inside-work-tree &>/dev/null; then
  die "Not inside a git repository."
fi

# ── Auto-increment patch version (0.0.Z) ─────────────────────────────────────
# Source of truth is the latest v0.0.Z git tag, not Cargo.toml,
# so the version always advances correctly regardless of what Cargo.toml says.

LAST_TAG=$(git tag | grep -E '^v0\.0\.[0-9]+$' | sort -t. -k3 -n | tail -1)
if [[ -z "$LAST_TAG" ]]; then
  NEXT_PATCH=1
else
  CURRENT_PATCH="${LAST_TAG##*.}"
  NEXT_PATCH=$(( CURRENT_PATCH + 1 ))
fi

VERSION="0.0.${NEXT_PATCH}"
TAG="v${VERSION}"

blue "Last tag     : ${LAST_TAG:-none}"
blue "Next version : $VERSION"

# Abort if tag already exists
if git tag | grep -qx "$TAG"; then
  die "Tag $TAG already exists. Something is out of sync."
fi

# ── Resolve commit message ────────────────────────────────────────────────────

MSG="${1:-}"
if [[ -z "$MSG" ]]; then
  read -rp "Commit message (default: 'chore: release $TAG'): " MSG
  MSG="${MSG:-chore: release $TAG}"
fi

# ── Bump version in Cargo.toml / Cargo.lock ──────────────────────────────────

blue "Bumping version to $VERSION in Cargo.toml..."
sed -i.bak "0,/^version = \".*\"/s//version = \"$VERSION\"/" Cargo.toml
rm -f Cargo.toml.bak

# Regenerate Cargo.lock without building
cargo generate-lockfile 2>/dev/null || true

# ── Stage important files ─────────────────────────────────────────────────────

blue "Staging files..."

STAGE=(
  Cargo.toml
  Cargo.lock
  build.rs
  Cross.toml
  config.example.toml
  install.sh
  picogallery.service
  README.md
  src/
  plugins/
)

for f in "${STAGE[@]}"; do
  if [[ -e "$f" ]]; then
    git add "$f"
  fi
done

# Show what's staged
STAGED=$(git diff --cached --name-only)
if [[ -z "$STAGED" ]]; then
  die "Nothing staged to commit."
fi
blue "Staged files:"
echo "$STAGED" | sed 's/^/  /'

# ── Commit ────────────────────────────────────────────────────────────────────

blue "Committing: $MSG"
git commit -m "$MSG"

# ── Push branch ───────────────────────────────────────────────────────────────

BRANCH=$(git rev-parse --abbrev-ref HEAD)
blue "Pushing branch '$BRANCH' to origin..."
git push origin "$BRANCH"

# ── Create and push tag ───────────────────────────────────────────────────────

blue "Creating tag $TAG..."
git tag "$TAG"
git push origin "$TAG"

green "Done — $TAG released on branch $BRANCH."
