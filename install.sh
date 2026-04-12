#!/usr/bin/env bash
# ============================================================
# PicoGallery installer for Raspberry Pi OS Lite (arm/arm64)
# ============================================================
# Usage:
#   curl -sSL https://raw.githubusercontent.com/.../install.sh | bash
#   — or —
#   bash install.sh
#
# What this script does:
#   1. Installs the ONLY required system packages (libsdl2, ca-certs, Rust toolchain)
#   2. Compiles picogallery with --release (optimised for size)
#   3. Installs the binary to /usr/local/bin/picogallery
#   4. Creates a systemd service that runs on boot
#   5. Adds the current user to the 'video' and 'render' groups
#      (required to access /dev/dri/card0 without root)
#
# NO X server, NO desktop environment, NO display manager is installed.
# The only display stack needed is the kernel KMS/DRM driver, which is
# already loaded on every Raspberry Pi OS installation.
# ============================================================

set -euo pipefail
BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; RESET='\033[0m'

info()    { echo -e "${GREEN}[•]${RESET} $*"; }
warn()    { echo -e "${YELLOW}[!]${RESET} $*"; }
die()     { echo -e "${RED}[✗]${RESET} $*" >&2; exit 1; }
section() { echo -e "\n${BOLD}══ $* ══${RESET}"; }

# ── Detect architecture ────────────────────────────────────────────────────────
ARCH=$(uname -m)
case "$ARCH" in
  armv6l)  RUST_TARGET="arm-unknown-linux-gnueabihf"  ;;   # Pi Zero / Pi 1
  armv7l)  RUST_TARGET="armv7-unknown-linux-gnueabihf" ;;  # Pi 2 / 3 (32-bit OS)
  aarch64) RUST_TARGET="aarch64-unknown-linux-gnu"    ;;   # Pi 3/4/5 (64-bit OS)
  *)       die "Unsupported architecture: $ARCH" ;;
esac
info "Architecture: $ARCH → Rust target: $RUST_TARGET"

# ── Check we're on Raspberry Pi OS ────────────────────────────────────────────
if ! grep -qi "raspberry" /etc/os-release 2>/dev/null; then
  warn "This doesn't look like Raspberry Pi OS — proceeding anyway."
fi

# ── System packages ───────────────────────────────────────────────────────────
section "Installing system packages"
#
# Runtime libraries (the ONLY ones needed):
#   libsdl2-2.0-0   — SDL2 shared library (KMS/DRM backend, no X11)
#   libdrm2         — DRM/KMS display probing (find correct /dev/dri/cardN)
#   ca-certificates — HTTPS root certs for API calls
#
# Build-time only (can be purged after compiling):
#   libsdl2-dev  libdrm-dev — headers
#   clang                   — for sdl2 bindgen
#   pkg-config              — lets Cargo locate libsdl2 and libdrm
#
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
  libsdl2-dev \
  libsdl2-2.0-0 \
  libdrm-dev \
  libdrm2 \
  ca-certificates \
  clang \
  pkg-config \
  curl \
  build-essential
info "System packages installed."

# ── Rust toolchain ─────────────────────────────────────────────────────────────
section "Installing Rust"
if command -v rustup &>/dev/null; then
  info "rustup already present — updating."
  rustup update stable --no-self-update
else
  info "Installing rustup…"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal
  # shellcheck source=/dev/null
  source "$HOME/.cargo/env"
fi
info "Rust $(rustc --version)"

# ── Clone / update source ─────────────────────────────────────────────────────
section "Fetching source"
SRC_DIR="$HOME/picogallery"
if [ -d "$SRC_DIR/.git" ]; then
  info "Updating existing checkout…"
  git -C "$SRC_DIR" pull --ff-only
else
  git clone https://github.com/yourusername/picogallery "$SRC_DIR"
fi

# ── Build ─────────────────────────────────────────────────────────────────────
section "Building (this takes a few minutes on Pi Zero)"
cd "$SRC_DIR"
# Pi Zero: single thread to avoid OOM during LLVM codegen
if [ "$ARCH" = "armv6l" ]; then
  CARGO_FLAGS="--jobs 1"
  info "Pi Zero detected — building with 1 job to conserve RAM."
else
  CARGO_FLAGS=""
fi

~/.cargo/bin/cargo build \
  --release \
  --features "plugin-google-photos,plugin-local" \
  $CARGO_FLAGS

BINARY="$SRC_DIR/target/release/picogallery"
[ -f "$BINARY" ] || die "Build failed — binary not found."
info "Binary size: $(du -sh "$BINARY" | cut -f1)"

# ── Install binary ────────────────────────────────────────────────────────────
section "Installing binary"
sudo install -m 755 "$BINARY" /usr/local/bin/picogallery
info "Installed to /usr/local/bin/picogallery"

# ── User groups ───────────────────────────────────────────────────────────────
section "Configuring user groups"
USER="${SUDO_USER:-$(whoami)}"
for group in video render input; do
  if getent group "$group" &>/dev/null; then
    sudo usermod -aG "$group" "$USER"
    info "Added $USER to group: $group"
  fi
done
warn "You will need to log out and back in for group changes to take effect."
warn "(Or run:  newgrp video)"

# ── Config ────────────────────────────────────────────────────────────────────
section "Setting up config"
CONFIG_DIR="$HOME/.config/picogallery"
mkdir -p "$CONFIG_DIR"
if [ ! -f "$CONFIG_DIR/config.toml" ]; then
  picogallery --print-default-config > "$CONFIG_DIR/config.toml"
  info "Default config written to $CONFIG_DIR/config.toml"
  echo ""
  warn "Edit the config before starting:"
  warn "  nano $CONFIG_DIR/config.toml"
  warn ""
  warn "You will need to add your Google Photos OAuth2 credentials."
  warn "See the README for setup instructions."
else
  info "Config already exists at $CONFIG_DIR/config.toml — not overwriting."
fi

# ── systemd service ───────────────────────────────────────────────────────────
section "Installing systemd service"
SERVICE_FILE="/etc/systemd/system/picogallery.service"
sudo tee "$SERVICE_FILE" > /dev/null <<SERVICE
[Unit]
Description=PicoGallery photo slideshow
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${USER}
Group=video
Environment=SDL_VIDEODRIVER=kmsdrm
Environment=RUST_LOG=info
ExecStartPre=/bin/sleep 5
ExecStart=/usr/local/bin/picogallery
Restart=on-failure
RestartSec=10
StandardOutput=journal
StandardError=journal

# Allow /dev/dri access without root.
SupplementaryGroups=video render input

[Install]
WantedBy=multi-user.target
SERVICE

sudo systemctl daemon-reload
info "Service file written to $SERVICE_FILE"
info "Commands:"
info "  sudo systemctl enable picogallery   # start on boot"
info "  sudo systemctl start  picogallery   # start now"
info "  sudo journalctl -u picogallery -f   # watch logs"

# ── GPU memory split ──────────────────────────────────────────────────────────
section "GPU memory optimisation"
BOOT_CONFIG="/boot/config.txt"
if [ -f "$BOOT_CONFIG" ] && ! grep -q "gpu_mem=" "$BOOT_CONFIG"; then
  echo ""                          | sudo tee -a "$BOOT_CONFIG"
  echo "# PicoGallery: GPU memory" | sudo tee -a "$BOOT_CONFIG"
  echo "gpu_mem=64"                | sudo tee -a "$BOOT_CONFIG"
  info "Set gpu_mem=64 in $BOOT_CONFIG"
else
  info "Skipping gpu_mem (already set or /boot/config.txt not found)."
fi

# Also check for /boot/firmware/config.txt (Pi OS Bookworm)
BOOT_CONFIG2="/boot/firmware/config.txt"
if [ -f "$BOOT_CONFIG2" ] && ! grep -q "gpu_mem=" "$BOOT_CONFIG2"; then
  echo ""                          | sudo tee -a "$BOOT_CONFIG2"
  echo "# PicoGallery: GPU memory" | sudo tee -a "$BOOT_CONFIG2"
  echo "gpu_mem=64"                | sudo tee -a "$BOOT_CONFIG2"
  info "Set gpu_mem=64 in $BOOT_CONFIG2"
fi

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}${BOLD}════════════════════════════════════════${RESET}"
echo -e "${GREEN}${BOLD}  PicoGallery installed successfully!   ${RESET}"
echo -e "${GREEN}${BOLD}════════════════════════════════════════${RESET}"
echo ""
echo "  1. Edit your credentials:"
echo "     nano ~/.config/picogallery/config.toml"
echo ""
echo "  2. Test run (interactive terminal):"
echo "     SDL_VIDEODRIVER=kmsdrm picogallery"
echo ""
echo "  3. Enable on boot:"
echo "     sudo systemctl enable --now picogallery"
echo ""
