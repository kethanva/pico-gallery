#!/usr/bin/env bash
# ============================================================
# PicoGallery uninstaller for Raspberry Pi OS (arm/arm64)
# ============================================================
# Usage:
#   curl -sSL https://raw.githubusercontent.com/kethanva/pico-gallery/main/uninstall.sh | bash
#   — or —
#   bash uninstall.sh
# ============================================================

set -euo pipefail

BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'
CYAN='\033[0;36m'; RESET='\033[0m'

info()    { echo -e "${GREEN}[+]${RESET} $*"; }
warn()    { echo -e "${YELLOW}[!]${RESET} $*"; }
die()     { echo -e "${RED}[x]${RESET} $*" >&2; exit 1; }
section() { echo -e "\n${BOLD}${CYAN}== $* ==${RESET}"; }

# ── CLI flags ─────────────────────────────────────────────────────────────────
ASSUME_YES=0
USER_FLAG=""
PURGE=0

print_usage() {
  cat <<'USAGE'
PicoGallery uninstaller

Usage: sudo ./uninstall.sh [options]

Options:
  --user <name>   uninstall for this user (default: the sudo user).
  --purge         also remove the photo directory and cache without prompting.
  -y, --yes       non-interactive; assume yes to every prompt.
  -h, --help      show this help and exit.

Env: PICOGALLERY_AUTO_UNINSTALL=1 is equivalent to -y (photos kept unless --purge).
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user)     USER_FLAG="${2:-}"; shift 2 ;;
    --user=*)   USER_FLAG="${1#*=}"; shift ;;
    --purge)    PURGE=1; shift ;;
    -y|--yes)   ASSUME_YES=1; shift ;;
    -h|--help)  print_usage; exit 0 ;;
    *) die "Unknown option: $1  (run with --help)" ;;
  esac
done

[[ "${PICOGALLERY_AUTO_UNINSTALL:-0}" == "1" ]] && ASSUME_YES=1

# ── Pre-flight checks ────────────────────────────────────────────────────────

[[ "$(uname -s)" == "Linux" ]] || die "This script only supports Linux (Raspberry Pi OS)."

TARGET_USER="${USER_FLAG:-${SUDO_USER:-$(whoami)}}"

echo -e "${BOLD}"
echo "  ╔═══════════════════════════════════════════╗"
echo "  ║  PicoGallery — Raspberry Pi Uninstaller   ║"
echo "  ╚═══════════════════════════════════════════╝"
echo -e "${RESET}"

if [[ "$ASSUME_YES" != "1" ]]; then
    read -p "This will remove PicoGallery, its configuration, and (optionally) sample photos. Are you sure? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        die "Uninstallation cancelled."
    fi
fi

# ── systemd service ──────────────────────────────────────────────────────────

section "Stopping and removing systemd service"

if systemctl is-active --quiet picogallery 2>/dev/null; then
    sudo systemctl stop picogallery
    info "Stopped picogallery service."
fi

if systemctl is-enabled --quiet picogallery 2>/dev/null; then
    sudo systemctl disable picogallery
    info "Disabled picogallery service."
fi

SERVICE_FILE="/etc/systemd/system/picogallery.service"
if [[ -f "$SERVICE_FILE" ]]; then
    sudo rm "$SERVICE_FILE"
    sudo systemctl daemon-reload
    info "Removed $SERVICE_FILE"
else
    info "Service file not found, skipping."
fi

# ── Binary ───────────────────────────────────────────────────────────────────

section "Removing binary"

if [[ -f /usr/local/bin/picogallery ]]; then
    sudo rm /usr/local/bin/picogallery
    info "Removed /usr/local/bin/picogallery"
else
    info "Binary not found, skipping."
fi

# ── Config and Photos ────────────────────────────────────────────────────────

section "Removing configuration and photo directories"

CONFIG_DIR="/home/${TARGET_USER}/.config/picogallery"
if [[ -d "$CONFIG_DIR" ]]; then
    sudo rm -rf "$CONFIG_DIR"
    info "Removed configuration directory: $CONFIG_DIR"
else
    info "Configuration directory not found, skipping."
fi

# On-disk image cache is always regenerable — remove it unconditionally.
CACHE_DIR="/home/${TARGET_USER}/.cache/picogallery"
if [[ -d "$CACHE_DIR" ]]; then
    sudo rm -rf "$CACHE_DIR"
    info "Removed cache directory: $CACHE_DIR"
else
    info "Cache directory not found, skipping."
fi

PHOTO_DIR="/home/${TARGET_USER}/Pictures/PicoGallery"
if [[ -d "$PHOTO_DIR" ]]; then
    REMOVE_PHOTOS=0
    if [[ "$PURGE" == "1" ]]; then
        REMOVE_PHOTOS=1
    elif [[ "$ASSUME_YES" == "1" ]]; then
        info "Non-interactive — keeping photo directory (use --purge to remove): $PHOTO_DIR"
    else
        read -p "Do you want to remove the photo directory ($PHOTO_DIR) and all its contents? (y/N) " -n 1 -r
        echo
        [[ $REPLY =~ ^[Yy]$ ]] && REMOVE_PHOTOS=1
    fi
    if [[ "$REMOVE_PHOTOS" == "1" ]]; then
        sudo rm -rf "$PHOTO_DIR"
        info "Removed photo directory: $PHOTO_DIR"
    else
        info "Kept photo directory: $PHOTO_DIR"
    fi
else
    info "Photo directory not found, skipping."
fi

# ── Boot Config ──────────────────────────────────────────────────────────────

section "Cleaning up boot config (/boot/firmware/config.txt & /boot/config.txt)"

cleanup_boot_config() {
    local cfg="$1"
    if [[ -f "$cfg" ]]; then
        local changed=0
        
        if grep -q "PicoGallery: enable KMS DRM" "$cfg"; then
            # Remove the exact blocks added by installer using sed
            sudo sed -i '/# PicoGallery: enable KMS DRM so \/dev\/dri\/cardN exists (SDL kmsdrm)/{N;N;d;}' "$cfg"
            changed=1
        fi
        
        if grep -q "PicoGallery: GPU memory" "$cfg"; then
            sudo sed -i '/# PicoGallery: GPU memory/{N;d;}' "$cfg"
            changed=1
        fi
        
        # Uncomment fkms if it was commented out by the installer
        if grep -qE '^#dtoverlay=vc4-fkms-v3d' "$cfg"; then
            sudo sed -i 's|^#dtoverlay=vc4-fkms-v3d|dtoverlay=vc4-fkms-v3d|' "$cfg"
            info "Re-enabled legacy vc4-fkms-v3d in $cfg"
            changed=1
        fi
        
        if [[ "$changed" == "1" ]]; then
            info "Cleaned up $cfg"
        fi
    fi
}

for cfg in /boot/firmware/config.txt /boot/config.txt; do
    cleanup_boot_config "$cfg"
done

# ── User groups ──────────────────────────────────────────────────────────────

section "User groups"
warn "The installer may have added $TARGET_USER to 'video', 'render', and 'input' groups."
warn "These groups might be needed by other applications, so they are NOT automatically removed."
warn "If you want to remove them manually, run:"
warn "  sudo gpasswd -d $TARGET_USER video"
warn "  sudo gpasswd -d $TARGET_USER render"
warn "  sudo gpasswd -d $TARGET_USER input"

# ── Dependencies ─────────────────────────────────────────────────────────────

section "System dependencies"
warn "The installer installed several apt dependencies (libsdl2, rclone, etc.)."
warn "To remove them, you can manually run:"
warn "  sudo apt-get remove libsdl2-2.0-0 libdrm2 libgbm1 libegl1 rclone"
warn "  sudo apt-get autoremove"

# ── Done ─────────────────────────────────────────────────────────────────────

echo ""
echo -e "${GREEN}${BOLD}════════════════════════════════════════════${RESET}"
echo -e "${GREEN}${BOLD}  PicoGallery uninstalled successfully!     ${RESET}"
echo -e "${GREEN}${BOLD}════════════════════════════════════════════${RESET}"
echo "A reboot may be required if boot config was changed."
echo ""
