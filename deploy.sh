#!/usr/bin/env bash
# ============================================================
# PicoGallery — deploy / update from source (PhotoPrism mode)
# ============================================================
# Pulls a branch, builds from the local checkout, provisions the PhotoPrism
# plugin, clears caches, and restarts the slideshow service.
#
# First-time setup on the Pi (clone + deploy):
#   sudo git clone --branch kva-revamparchitecture --single-branch \
#     https://github.com/kethanva/pico-gallery.git /opt/picogallery
#   sudo bash /opt/picogallery/deploy.sh
#
# Re-deploy after pushing changes:
#   sudo bash /opt/picogallery/deploy.sh
#
# Override settings inline:
#   sudo PHOTOPRISM_PASS='s3cret' KIOSK_USER=pi bash deploy.sh
#
# Copy-paste one-liner (matches the pico-gallery-photoprism deploy flow):
#
#   sudo bash -c '
#   set -euo pipefail
#   REPO=/opt/picogallery
#   BRANCH=kva-revamparchitecture
#   KIOSK=picokiosk
#   systemctl stop picogallery 2>/dev/null || true
#   #rm -rf "$REPO"
#   #git clone --branch "$BRANCH" --single-branch https://github.com/kethanva/pico-gallery.git "$REPO"
#   cd "$REPO"
#   rm -f "/home/${KIOSK}/.config/picogallery/config.toml"
#   git checkout "$BRANCH"
#   git pull
#   ./install.sh --mode all -y --user "$KIOSK" \
#     --photoprism-url http://192.168.68.71:2342 \
#     --photoprism-user admin \
#     --photoprism-pass Password
#   rm -rf "/home/${KIOSK}/.cache" "/home/${KIOSK}/.local"
#   systemctl restart picogallery
#   '
# ============================================================

set -euo pipefail

BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RESET='\033[0m'
info() { echo -e "${GREEN}[+]${RESET} $*"; }
warn() { echo -e "${YELLOW}[!]${RESET} $*"; }

# ── Settings (edit or override via env) ──────────────────────────────────────
REPO_DIR="${REPO_DIR:-/opt/picogallery}"
REPO_URL="${REPO_URL:-https://github.com/kethanva/pico-gallery.git}"
BRANCH="${BRANCH:-kva-revamparchitecture}"
KIOSK_USER="${KIOSK_USER:-picokiosk}"

PHOTOPRISM_URL="${PHOTOPRISM_URL:-http://192.168.68.71:2342}"
PHOTOPRISM_USER="${PHOTOPRISM_USER:-admin}"
PHOTOPRISM_PASS="${PHOTOPRISM_PASS:-Password}"

# Drop stale config / user caches before install (set to 0 to keep them).
RESET_CONFIG="${RESET_CONFIG:-1}"
CLEAR_USER_CACHE="${CLEAR_USER_CACHE:-1}"

# ── Pre-flight ───────────────────────────────────────────────────────────────
[[ $EUID -eq 0 ]] || { echo "Run as root:  sudo bash deploy.sh" >&2; exit 1; }
id "$KIOSK_USER" &>/dev/null || { echo "User '$KIOSK_USER' does not exist (set KIOSK_USER)." >&2; exit 1; }

CONFIG_FILE="/home/${KIOSK_USER}/.config/picogallery/config.toml"

# ── Stop the running slideshow ───────────────────────────────────────────────
info "Stopping picogallery service"
systemctl stop picogallery 2>/dev/null || true

# ── Fetch the branch (clone on first run, pull on subsequent deploys) ────────
if [[ -d "$REPO_DIR/.git" ]]; then
  info "Updating $REPO_DIR (branch $BRANCH)"
  git -C "$REPO_DIR" fetch origin
  git -C "$REPO_DIR" checkout "$BRANCH"
  git -C "$REPO_DIR" pull origin "$BRANCH"
else
  warn "No git checkout at $REPO_DIR — cloning"
  rm -rf "$REPO_DIR"
  git clone --branch "$BRANCH" --single-branch "$REPO_URL" "$REPO_DIR"
fi

# ── Fresh config (install.sh also rewrites when PhotoPrism flags are passed) ─
if [[ "$RESET_CONFIG" == "1" && -f "$CONFIG_FILE" ]]; then
  info "Removing $CONFIG_FILE"
  rm -f "$CONFIG_FILE"
fi

# ── Build + install + provision PhotoPrism ───────────────────────────────────
cd "$REPO_DIR"
chmod +x install.sh uninstall.sh
info "Running install.sh (source build + PhotoPrism provisioning)"
./install.sh --mode all -y \
  --user "$KIOSK_USER" \
  --photoprism-url  "$PHOTOPRISM_URL" \
  --photoprism-user "$PHOTOPRISM_USER" \
  --photoprism-pass "$PHOTOPRISM_PASS"

# ── Clear regenerable caches so refreshed tokens / thumbnails take effect ────
if [[ "$CLEAR_USER_CACHE" == "1" ]]; then
  info "Clearing $KIOSK_USER caches"
  rm -rf "/home/${KIOSK_USER}/.cache" "/home/${KIOSK_USER}/.local"
fi

# ── Start ────────────────────────────────────────────────────────────────────
info "Starting picogallery service"
systemctl restart picogallery
sleep 1
systemctl --no-pager status picogallery | head -n 12

echo
echo -e "${BOLD}Deployed${RESET} branch '$BRANCH' → PhotoPrism $PHOTOPRISM_URL (user: $PHOTOPRISM_USER)"
echo "Logs:  journalctl -u picogallery -f"
