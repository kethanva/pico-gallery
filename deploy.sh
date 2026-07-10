#!/usr/bin/env bash
# ============================================================
# PicoGallery — deploy pre-built release (PhotoPrism mode)
# ============================================================
# Downloads a GitHub Release binary (no Rust compile on the Pi), provisions
# the PhotoPrism plugin config, clears caches, and restarts the service.
#
# First deploy on the Pi:
#   sudo PICOGALLERY_VERSION=v0.1.3-kva.1 bash deploy.sh
#
# Re-deploy after a new release is published:
#   sudo PICOGALLERY_VERSION=v0.1.3-kva.2 bash deploy.sh
#
# Override settings inline:
#   sudo PICOGALLERY_VERSION=v0.1.3-kva.1 PHOTOPRISM_PASS='s3cret' bash deploy.sh
#
# Copy-paste one-liner (no on-device build):
#
#   sudo bash -c '
#   set -euo pipefail
#   VERSION=v0.1.3-kva.1
#   BRANCH=kva-revamparchitecture
#   KIOSK=picokiosk
#   systemctl stop picogallery 2>/dev/null || true
#   curl -fsSL -o /tmp/picogallery-install.sh \
#     https://raw.githubusercontent.com/kethanva/pico-gallery/${BRANCH}/install.sh
#   chmod +x /tmp/picogallery-install.sh
#   rm -f "/home/${KIOSK}/.config/picogallery/config.toml"
#   PICOGALLERY_VERSION="$VERSION" /tmp/picogallery-install.sh --mode download -y --user "$KIOSK" \
#     --photoprism-url http://192.168.68.71:2342 \
#     --photoprism-user admin \
#     --photoprism-pass Password
#   rm -rf "/home/${KIOSK}/.cache" "/home/${KIOSK}/.local"
#   systemctl restart picogallery
#   '
# ============================================================

set -euo pipefail

BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; RESET='\033[0m'
info() { echo -e "${GREEN}[+]${RESET} $*"; }
warn() { echo -e "${YELLOW}[!]${RESET} $*"; }
die()  { echo -e "${RED}[x]${RESET} $*" >&2; exit 1; }

# ── Settings (edit or override via env) ──────────────────────────────────────
REPO_SLUG="${REPO_SLUG:-kethanva/pico-gallery}"
REPO_URL="${REPO_URL:-https://github.com/${REPO_SLUG}.git}"
BRANCH="${BRANCH:-kva-revamparchitecture}"
KIOSK_USER="${KIOSK_USER:-picokiosk}"

PHOTOPRISM_URL="${PHOTOPRISM_URL:-http://192.168.68.71:2342}"
PHOTOPRISM_USER="${PHOTOPRISM_USER:-admin}"
PHOTOPRISM_PASS="${PHOTOPRISM_PASS:-Password}"

# Required: GitHub release tag with pre-built ARM binaries for this branch.
PICOGALLERY_VERSION="${PICOGALLERY_VERSION:-}"

RESET_CONFIG="${RESET_CONFIG:-1}"
CLEAR_USER_CACHE="${CLEAR_USER_CACHE:-1}"
INSTALL_SCRIPT="${INSTALL_SCRIPT:-/tmp/picogallery-install.sh}"

# ── Pre-flight ───────────────────────────────────────────────────────────────
[[ $EUID -eq 0 ]] || die "Run as root:  sudo bash deploy.sh"
id "$KIOSK_USER" &>/dev/null || die "User '$KIOSK_USER' does not exist (set KIOSK_USER)."
[[ -n "$PICOGALLERY_VERSION" ]] || die "Set PICOGALLERY_VERSION to a GitHub release tag (e.g. v0.1.3-kva.1). Run ./release.sh --prerelease on your dev machine first."

CONFIG_FILE="$(getent passwd "$KIOSK_USER" | cut -d: -f6)/.config/picogallery/config.toml"
[[ -n "$CONFIG_FILE" && "$CONFIG_FILE" != "/.config"* ]] || die "Could not resolve home for $KIOSK_USER"
KIOSK_HOME="${CONFIG_FILE%/.config/picogallery/config.toml}"

# ── Stop the running slideshow ───────────────────────────────────────────────
info "Stopping picogallery service"
systemctl stop picogallery 2>/dev/null || true

# ── Fetch install.sh from the branch (lightweight — no full repo clone) ──────
info "Fetching install.sh from ${REPO_SLUG}@${BRANCH}"
curl -fsSL -o "$INSTALL_SCRIPT" \
  "https://raw.githubusercontent.com/${REPO_SLUG}/${BRANCH}/install.sh"
chmod +x "$INSTALL_SCRIPT"

# ── Fresh config ─────────────────────────────────────────────────────────────
if [[ "$RESET_CONFIG" == "1" && -f "$CONFIG_FILE" ]]; then
  info "Removing $CONFIG_FILE"
  rm -f "$CONFIG_FILE"
fi

# ── Install pre-built binary + provision PhotoPrism (no compile on Pi) ───────
info "Installing release $PICOGALLERY_VERSION (download mode — no local build)"
PICOGALLERY_VERSION="$PICOGALLERY_VERSION" "$INSTALL_SCRIPT" --mode download -y \
  --user "$KIOSK_USER" \
  --version "$PICOGALLERY_VERSION" \
  --photoprism-url  "$PHOTOPRISM_URL" \
  --photoprism-user "$PHOTOPRISM_USER" \
  --photoprism-pass "$PHOTOPRISM_PASS"

# ── Clear regenerable caches ─────────────────────────────────────────────────
if [[ "$CLEAR_USER_CACHE" == "1" ]]; then
  info "Clearing $KIOSK_USER caches"
  rm -rf "${KIOSK_HOME}/.cache" "${KIOSK_HOME}/.local"
fi

# ── Start ────────────────────────────────────────────────────────────────────
info "Starting picogallery service"
systemctl restart picogallery
sleep 1
systemctl --no-pager status picogallery | head -n 12

echo
echo -e "${BOLD}Deployed${RESET} $PICOGALLERY_VERSION → PhotoPrism $PHOTOPRISM_URL (user: $PHOTOPRISM_USER)"
echo "Logs:  journalctl -u picogallery -f"
