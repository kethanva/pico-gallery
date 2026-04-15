#!/usr/bin/env bash
# ============================================================
# PicoGallery installer for Raspberry Pi OS (arm/arm64)
# ============================================================
# Usage:
#   curl -sSL https://raw.githubusercontent.com/kethanva/opentinyphotoapp/main/install.sh | bash
#   — or —
#   bash install.sh
#   — or (pin a specific version) —
#   PICOGALLERY_VERSION=v0.2.0 bash install.sh
#
# What this script does:
#   1. Detects your architecture (aarch64 or armv7)
#   2. Downloads the pre-built binary from GitHub Releases
#   3. Installs runtime dependencies (libsdl2, libdrm, rclone)
#   4. Installs the binary to /usr/local/bin/picogallery
#   5. Creates a systemd service that runs on boot
#   6. Adds the current user to video/render/input groups
#   7. Writes a default config if none exists
#   8. Configures GPU memory split
#
# NO compilation needed. NO Rust toolchain required.
# NO X server, NO desktop environment, NO display manager.
# ============================================================

set -euo pipefail

REPO="kethanva/opentinyphotoapp"
BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'
CYAN='\033[0;36m'; RESET='\033[0m'

info()    { echo -e "${GREEN}[+]${RESET} $*"; }
warn()    { echo -e "${YELLOW}[!]${RESET} $*"; }
die()     { echo -e "${RED}[x]${RESET} $*" >&2; exit 1; }
section() { echo -e "\n${BOLD}${CYAN}== $* ==${RESET}"; }

# ── Pre-flight checks ────────────────────────────────────────────────────────

[[ "$(uname -s)" == "Linux" ]] || die "This installer only supports Linux (Raspberry Pi OS)."

command -v curl &>/dev/null || die "curl is required. Install with: sudo apt-get install -y curl"

echo -e "${BOLD}"
echo "  ╔═══════════════════════════════════════════╗"
echo "  ║   PicoGallery — Raspberry Pi Installer    ║"
echo "  ╚═══════════════════════════════════════════╝"
echo -e "${RESET}"

# ── Detect architecture ──────────────────────────────────────────────────────

ARCH=$(uname -m)
case "$ARCH" in
  aarch64)       ARTIFACT_ARCH="aarch64" ;;
  armv7l|armv6l) ARTIFACT_ARCH="armv7"   ;;
  *)             die "Unsupported architecture: $ARCH (need aarch64 or armv7l)" ;;
esac
info "Architecture: $ARCH -> artifact: linux-${ARTIFACT_ARCH}"

# ── Resolve version ──────────────────────────────────────────────────────────

section "Resolving version"

if [[ -n "${PICOGALLERY_VERSION:-}" ]]; then
  VERSION="$PICOGALLERY_VERSION"
  info "Using pinned version: $VERSION"
else
  info "Fetching latest release from GitHub..."
  VERSION=$(curl -sSL \
    -H "Accept: application/vnd.github+json" \
    "https://api.github.com/repos/${REPO}/releases/latest" \
    | grep '"tag_name"' | head -1 | cut -d'"' -f4)

  [[ -n "$VERSION" ]] || die "Could not determine latest release. Set PICOGALLERY_VERSION manually."
  info "Latest release: $VERSION"
fi

# ── Download binary ──────────────────────────────────────────────────────────

section "Downloading PicoGallery ${VERSION}"

TARBALL="picogallery-${VERSION}-linux-${ARTIFACT_ARCH}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"
SHA_URL="${DOWNLOAD_URL}.sha256"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading: $TARBALL"
HTTP_CODE=$(curl -sSL -w "%{http_code}" -o "${TMPDIR}/${TARBALL}" "$DOWNLOAD_URL")
[[ "$HTTP_CODE" == "200" ]] || die "Download failed (HTTP $HTTP_CODE). Check that ${VERSION} has a linux-${ARTIFACT_ARCH} artifact."

# Verify checksum if available
if curl -sSL -o "${TMPDIR}/${TARBALL}.sha256" "$SHA_URL" 2>/dev/null; then
  cd "$TMPDIR"
  if sha256sum -c "${TARBALL}.sha256" &>/dev/null; then
    info "SHA-256 checksum verified."
  else
    warn "Checksum mismatch! Continuing anyway — verify manually if concerned."
  fi
  cd - >/dev/null
else
  warn "No checksum file found — skipping verification."
fi

# Extract
info "Extracting..."
tar xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}"
EXTRACT_DIR=$(find "$TMPDIR" -maxdepth 1 -type d -name "picogallery-*" | head -1)
[[ -d "$EXTRACT_DIR" ]] || die "Extraction failed — archive structure unexpected."

# ── Install runtime dependencies ─────────────────────────────────────────────

section "Installing runtime dependencies"

sudo apt-get update -qq

sudo apt-get install -y --no-install-recommends \
  libsdl2-2.0-0 \
  libdrm2 \
  ca-certificates \
  rclone

info "Runtime dependencies installed."

# ── Install binary ───────────────────────────────────────────────────────────

section "Installing binary"

sudo install -m 755 "${EXTRACT_DIR}/picogallery" /usr/local/bin/picogallery
info "Installed /usr/local/bin/picogallery"
info "Version: $(picogallery --version 2>/dev/null || echo "$VERSION")"
info "Binary size: $(du -h /usr/local/bin/picogallery | cut -f1)"

# ── User groups ──────────────────────────────────────────────────────────────

section "Configuring user groups"

TARGET_USER="${SUDO_USER:-$(whoami)}"
for group in video render input; do
  if getent group "$group" &>/dev/null; then
    sudo usermod -aG "$group" "$TARGET_USER"
    info "Added $TARGET_USER to group: $group"
  fi
done

# ── Config ───────────────────────────────────────────────────────────────────

section "Setting up configuration"

CONFIG_DIR="/home/${TARGET_USER}/.config/picogallery"
sudo -u "$TARGET_USER" mkdir -p "$CONFIG_DIR"

if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
  if [[ -f "${EXTRACT_DIR}/config.example.toml" ]]; then
    sudo -u "$TARGET_USER" cp "${EXTRACT_DIR}/config.example.toml" "${CONFIG_DIR}/config.toml"
  else
    # Generate minimal default config
    sudo -u "$TARGET_USER" tee "${CONFIG_DIR}/config.toml" > /dev/null <<'TOML'
# PicoGallery configuration
# See: https://github.com/kethanva/opentinyphotoapp

[display]
slide_duration_secs = 10
transition          = "fade"
transition_ms       = 800
fill_screen         = false
fps                 = 15

[cache]
max_mb         = 256
prefetch_count = 3

# Enable the directory plugin and point it at your photos:
[[plugins]]
name    = "directory"
enabled = true
path    = "/home/pi/Photos"
order   = "shuffle"
recursive = true

[[plugins]]
name    = "local"
enabled = false
paths   = ["/home/pi/Pictures"]

[[plugins]]
name    = "google-photos"
enabled = false
sync_dir = "/tmp/picogallery-gdrive"

[[plugins]]
name    = "amazon-photos"
enabled = false
client_id     = ""
client_secret = ""
TOML
  fi
  info "Default config written to ${CONFIG_DIR}/config.toml"
else
  info "Config already exists at ${CONFIG_DIR}/config.toml — not overwriting."
fi

# ── systemd service ──────────────────────────────────────────────────────────

section "Installing systemd service"

SERVICE_FILE="/etc/systemd/system/picogallery.service"
sudo tee "$SERVICE_FILE" > /dev/null <<SERVICE
[Unit]
Description=PicoGallery photo slideshow
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${TARGET_USER}
Group=video
Environment=SDL_VIDEODRIVER=kmsdrm
Environment=RUST_LOG=info
ExecStartPre=/bin/sleep 5
ExecStart=/usr/local/bin/picogallery
Restart=on-failure
RestartSec=10
StandardOutput=journal
StandardError=journal

# Allow /dev/dri access without root
SupplementaryGroups=video render input

[Install]
WantedBy=multi-user.target
SERVICE

sudo systemctl daemon-reload
info "Service installed: picogallery.service"

# ── GPU memory ───────────────────────────────────────────────────────────────

section "GPU memory optimisation"

set_gpu_mem() {
  local cfg="$1"
  if [[ -f "$cfg" ]] && ! grep -q "^gpu_mem=" "$cfg"; then
    echo ""                          | sudo tee -a "$cfg" > /dev/null
    echo "# PicoGallery: GPU memory" | sudo tee -a "$cfg" > /dev/null
    echo "gpu_mem=64"                | sudo tee -a "$cfg" > /dev/null
    info "Set gpu_mem=64 in $cfg"
  fi
}

set_gpu_mem "/boot/config.txt"
set_gpu_mem "/boot/firmware/config.txt"

# ── Enable & start ───────────────────────────────────────────────────────────

section "Enabling service"

sudo systemctl enable picogallery
info "PicoGallery will start automatically on boot."

# ── Done ─────────────────────────────────────────────────────────────────────

echo ""
echo -e "${GREEN}${BOLD}════════════════════════════════════════════${RESET}"
echo -e "${GREEN}${BOLD}  PicoGallery installed successfully!       ${RESET}"
echo -e "${GREEN}${BOLD}════════════════════════════════════════════${RESET}"
echo ""
echo "  Version : ${VERSION}"
echo "  Binary  : /usr/local/bin/picogallery"
echo "  Config  : ${CONFIG_DIR}/config.toml"
echo "  Service : picogallery.service"
echo ""
echo "  Next steps:"
echo ""
echo "  1. Edit your config (set your photo source):"
echo "     nano ${CONFIG_DIR}/config.toml"
echo ""
echo "  2. Test run (interactive):"
echo "     SDL_VIDEODRIVER=kmsdrm picogallery"
echo ""
echo "  3. Start the service now:"
echo "     sudo systemctl start picogallery"
echo ""
echo "  4. Watch logs:"
echo "     sudo journalctl -u picogallery -f"
echo ""
echo "  5. Reboot to apply GPU memory & group changes:"
echo "     sudo reboot"
echo ""
