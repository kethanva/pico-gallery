#!/usr/bin/env bash
# ============================================================
# PicoGallery installer for Raspberry Pi OS (arm/arm64)
# ============================================================
# Usage:
#   curl -sSL https://raw.githubusercontent.com/kethanva/PicoGallery/main/install.sh | bash
#   — or —
#   bash install.sh
#   — or (pin a specific version) —
#   PICOGALLERY_VERSION=v0.1.0 bash install.sh
#   — or (force build from source) —
#   PICOGALLERY_BUILD=1 bash install.sh
#
# What this script does:
#   1. Detects architecture (aarch64 or armv7)
#   2. Tries to download the pre-built binary from GitHub Releases
#   3. Falls back to building from source if no release is available
#   4. Installs runtime dependencies (libsdl2, libdrm, rclone)
#   5. Installs the binary to /usr/local/bin/picogallery
#   6. Creates a systemd service that runs on boot
#   7. Adds the current user to video/render/input groups
#   8. Writes a default config if none exists
#   9. Configures GPU memory split
#
# NO X server, NO desktop environment, NO display manager.
# ============================================================

set -euo pipefail

REPO="kethanva/PicoGallery"
REPO_URL="https://github.com/${REPO}"
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
  aarch64)       ARTIFACT_ARCH="aarch64"; RUST_TARGET="aarch64-unknown-linux-gnu" ;;
  armv7l)        ARTIFACT_ARCH="armv7";   RUST_TARGET="armv7-unknown-linux-gnueabihf" ;;
  armv6l)        ARTIFACT_ARCH="armv6";   RUST_TARGET="arm-unknown-linux-gnueabihf" ;;
  *)             die "Unsupported architecture: $ARCH (need aarch64 or armv7l)" ;;
esac
info "Architecture: $ARCH -> artifact: linux-${ARTIFACT_ARCH}"

# ── Temp directory ───────────────────────────────────────────────────────────

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# ── Resolve version & download strategy ──────────────────────────────────────

section "Resolving version"

INSTALL_MODE="download"   # "download" or "build"
VERSION=""
EXTRACT_DIR=""

if [[ "${PICOGALLERY_BUILD:-}" == "1" ]]; then
  INSTALL_MODE="build"
  info "PICOGALLERY_BUILD=1 — will build from source."
fi

if [[ "$INSTALL_MODE" == "download" ]]; then
  # Determine which version to download
  if [[ -n "${PICOGALLERY_VERSION:-}" ]]; then
    VERSION="$PICOGALLERY_VERSION"
    info "Using pinned version: $VERSION"
  else
    info "Fetching latest release from GitHub..."
    RELEASE_JSON=$(curl -sSL -w "\n%{http_code}" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null) || true

    HTTP_CODE=$(echo "$RELEASE_JSON" | tail -1)
    RELEASE_BODY=$(echo "$RELEASE_JSON" | sed '$d')

    if [[ "$HTTP_CODE" == "200" ]]; then
      VERSION=$(echo "$RELEASE_BODY" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
    fi

    if [[ -z "$VERSION" ]]; then
      warn "No 'Latest Release' found on GitHub yet (HTTP $HTTP_CODE)."

      # Try to find the latest tag instead
      TAGS_JSON=$(curl -sSL \
        -H "Accept: application/vnd.github+json" \
        "https://api.github.com/repos/${REPO}/tags?per_page=1" 2>/dev/null) || true
      TAG_NAME=$(echo "$TAGS_JSON" | grep '"name"' | head -1 | cut -d'"' -f4)

      if [[ -n "$TAG_NAME" ]]; then
        info "Found tag: $TAG_NAME — will attempt to download from this tag."
        VERSION="$TAG_NAME"
      else
        warn "No tags found either. Falling back to building from source..."
        INSTALL_MODE="build"
      fi
    else
      info "Latest release: $VERSION"
    fi
  fi
fi

# ── Try downloading pre-built binary ─────────────────────────────────────────

if [[ "$INSTALL_MODE" == "download" ]]; then
  section "Downloading PicoGallery ${VERSION}"

  TARBALL="picogallery-${VERSION}-linux-${ARTIFACT_ARCH}.tar.gz"
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"
  SHA_URL="${DOWNLOAD_URL}.sha256"

  info "URL: $DOWNLOAD_URL"
  HTTP_CODE=$(curl -sSL -w "%{http_code}" -o "${TMPDIR}/${TARBALL}" "$DOWNLOAD_URL" 2>/dev/null) || true

  if [[ "$HTTP_CODE" == "200" ]] && [[ -s "${TMPDIR}/${TARBALL}" ]]; then
    info "Downloaded: $TARBALL"

    # Verify checksum if available
    if curl -sSL -o "${TMPDIR}/${TARBALL}.sha256" "$SHA_URL" 2>/dev/null; then
      SAVED_DIR=$(pwd)
      cd "$TMPDIR"
      if sha256sum -c "${TARBALL}.sha256" &>/dev/null; then
        info "SHA-256 checksum verified."
      else
        warn "Checksum mismatch — continuing anyway."
      fi
      cd "$SAVED_DIR"
    fi

    # Extract
    info "Extracting..."
    tar xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}"
    EXTRACT_DIR=$(find "$TMPDIR" -maxdepth 1 -type d -name "picogallery-*" | head -1)

    if [[ -d "$EXTRACT_DIR" ]] && [[ -f "${EXTRACT_DIR}/picogallery" ]]; then
      info "Binary extracted successfully."
    else
      warn "Archive did not contain expected binary."
      warn "Falling back to building from source..."
      INSTALL_MODE="build"
    fi
  else
    warn "Download failed (HTTP $HTTP_CODE). No pre-built binary for ${VERSION} / linux-${ARTIFACT_ARCH}."
    warn "Falling back to building from source..."
    INSTALL_MODE="build"
  fi
fi

# ── Build from source (fallback) ────────────────────────────────────────────

if [[ "$INSTALL_MODE" == "build" ]]; then
  section "Building from source"
  info "This takes 5-15 minutes on a Raspberry Pi (longer on Pi Zero)."

  # Install build dependencies
  info "Installing build dependencies..."
  sudo apt-get update -qq
  sudo apt-get install -y --no-install-recommends \
    libsdl2-dev \
    libsdl2-2.0-0 \
    libdrm-dev \
    libdrm2 \
    ca-certificates \
    clang \
    pkg-config \
    cmake \
    curl \
    git \
    build-essential \
    rclone

  # Install Rust if needed
  if command -v rustup &>/dev/null; then
    info "Rust already installed — updating."
    rustup update stable --no-self-update
  else
    info "Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
      sh -s -- -y --default-toolchain stable --profile minimal
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
  fi
  info "Rust $(rustc --version)"

  # Clone or update source
  SRC_DIR="${TMPDIR}/picogallery-src"
  info "Cloning repository..."
  git clone --depth 1 "${REPO_URL}.git" "$SRC_DIR"

  # Build
  info "Compiling (release mode)..."
  cd "$SRC_DIR"

  # Pi Zero / armv6l: single thread to avoid OOM
  CARGO_JOBS=""
  if [[ "$ARCH" == "armv6l" ]]; then
    CARGO_JOBS="--jobs 1"
    info "Pi Zero detected — building with 1 job to conserve RAM."
  fi

  "$HOME/.cargo/bin/cargo" build \
    --release \
    --features "plugin-google-photos,plugin-local,plugin-directory" \
    $CARGO_JOBS

  BUILT_BINARY="$SRC_DIR/target/release/picogallery"
  [[ -f "$BUILT_BINARY" ]] || die "Build failed — binary not found."
  info "Build complete. Binary size: $(du -h "$BUILT_BINARY" | cut -f1)"

  # Set up extract dir to match the download path
  EXTRACT_DIR="${TMPDIR}/picogallery-built"
  mkdir -p "$EXTRACT_DIR"
  cp "$BUILT_BINARY" "$EXTRACT_DIR/picogallery"
  cp "$SRC_DIR/config.example.toml" "$EXTRACT_DIR/" 2>/dev/null || true
  cp "$SRC_DIR/picogallery.service" "$EXTRACT_DIR/" 2>/dev/null || true

  VERSION=$(grep '^version' "$SRC_DIR/Cargo.toml" | head -1 | cut -d'"' -f2)
  VERSION="v${VERSION}"
  cd /
fi

# ── Install runtime dependencies (if not already installed by build) ─────────

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
info "Version: $(picogallery --version 2>/dev/null || echo "${VERSION:-unknown}")"
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
    sudo -u "$TARGET_USER" tee "${CONFIG_DIR}/config.toml" > /dev/null <<'TOML'
# PicoGallery configuration
# See: https://github.com/kethanva/PicoGallery

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
echo "  Version : ${VERSION:-source build}"
echo "  Mode    : ${INSTALL_MODE}"
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
