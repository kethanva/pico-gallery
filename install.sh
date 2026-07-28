#!/usr/bin/env bash
# ============================================================
# PicoGallery installer for Raspberry Pi OS (arm/arm64)
# ============================================================
# Usage:
#   curl -sSL https://raw.githubusercontent.com/kethanva/pico-gallery/main/install.sh | bash
#   — or —
#   bash install.sh
#   — or (pin a specific version) —
#   PICOGALLERY_VERSION=v0.1.0 bash install.sh
#   — or (force build from source) —
#   PICOGALLERY_BUILD=1 bash install.sh
#   — or (slower build with every plugin) —
#   PICOGALLERY_FEATURES="plugin-google-photos,plugin-local,plugin-directory" bash install.sh
#   — or (maximum-speed install, no optimisation) —
#   PICOGALLERY_PROFILE=release-fast bash install.sh   # default on low-RAM Pis
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

REPO="kethanva/pico-gallery"
REPO_URL="https://github.com/${REPO}"
BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'
CYAN='\033[0;36m'; RESET='\033[0m'

info()    { echo -e "${GREEN}[+]${RESET} $*"; }
warn()    { echo -e "${YELLOW}[!]${RESET} $*"; }
die()     { echo -e "${RED}[x]${RESET} $*" >&2; exit 1; }
section() { echo -e "\n${BOLD}${CYAN}== $* ==${RESET}"; }

# ── CLI flags ─────────────────────────────────────────────────────────────────
# The PICOGALLERY_* env vars (see header) still work; these flags override them
# and add one-shot PhotoPrism provisioning. With the default download mode the
# pre-built release binary is installed — no on-device Rust compile:
#
#   sudo ./install.sh --mode all -y \
#     --photoprism-url http://photoprism.local:2342 \
#     --photoprism-user admin --photoprism-pass 'YOUR_PASSWORD'
#
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || pwd)"

MODE_FLAG=""            # download | build | all
ASSUME_YES=0
USER_FLAG=""
VERSION_FLAG=""
PP_URL=""
PP_USER=""
PP_PASS=""
PP_APP_PASS=""

print_usage() {
  cat <<'USAGE'
PicoGallery installer

Usage: sudo ./install.sh [options]

Options:
  --mode <download|build|all>    download = prebuilt release (default);
                                 build/all = compile from source (all = every plugin).
  --user <name>                  install for this user (default: the sudo user).
  --photoprism-url <url>         provision the PhotoPrism plugin against this server
                                 (implies a source build so the plugin is compiled in).
  --photoprism-user <name>       PhotoPrism username (default: admin).
  --photoprism-pass <pass>       PhotoPrism password.
  --photoprism-app-password <p>  PhotoPrism app password (alternative to --photoprism-pass).
  --version <tag>                pin a GitHub release tag (e.g. v0.1.3 or v0.1.3-kva.1).
  -y, --yes                      non-interactive; assume yes.
  -h, --help                     show this help and exit.

Env vars (PICOGALLERY_VERSION / _BUILD / _FEATURES / _PROFILE / _RESET_CONFIG)
still apply; explicit flags win.

PhotoPrism provisioning (--photoprism-url) writes config only. With --mode download
(the default) the pre-built release binary is used — no on-device Rust compile.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)                       MODE_FLAG="${2:-}"; shift 2 ;;
    --mode=*)                     MODE_FLAG="${1#*=}"; shift ;;
    --user)                       USER_FLAG="${2:-}"; shift 2 ;;
    --user=*)                     USER_FLAG="${1#*=}"; shift ;;
    --photoprism-url)             PP_URL="${2:-}"; shift 2 ;;
    --photoprism-url=*)           PP_URL="${1#*=}"; shift ;;
    --photoprism-user)            PP_USER="${2:-}"; shift 2 ;;
    --photoprism-user=*)          PP_USER="${1#*=}"; shift ;;
    --photoprism-pass)            PP_PASS="${2:-}"; shift 2 ;;
    --photoprism-pass=*)          PP_PASS="${1#*=}"; shift ;;
    --photoprism-app-password)    PP_APP_PASS="${2:-}"; shift 2 ;;
    --photoprism-app-password=*)  PP_APP_PASS="${1#*=}"; shift ;;
    --version)                    VERSION_FLAG="${2:-}"; shift 2 ;;
    --version=*)                  VERSION_FLAG="${1#*=}"; shift ;;
    -y|--yes)                     ASSUME_YES=1; shift ;;
    -h|--help)                    print_usage; exit 0 ;;
    *) die "Unknown option: $1  (run with --help)" ;;
  esac
done

# Normalise --mode. `all` also flips the feature set to every plugin.
MODE_ALL=0
case "$MODE_FLAG" in
  ""|download)  : ;;                              # keep default (download)
  all)          MODE_ALL=1; INSTALL_MODE="build" ;;
  build)        INSTALL_MODE="build" ;;
  *) die "Invalid --mode '$MODE_FLAG'  (want: download | build | all)" ;;
esac

# Pin a release tag from the CLI flag.
if [[ -n "$VERSION_FLAG" ]]; then
  PICOGALLERY_VERSION="$VERSION_FLAG"
fi

# Pre-built GitHub releases include plugin-photoprism, so --photoprism-url only
# provisions config when --mode download (default). Use --mode build|all to compile.

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

# ── Detect RAM & device model (used to size on-device builds) ────────────────

MEM_KB=$(awk '/MemTotal/ {print $2; exit}' /proc/meminfo 2>/dev/null || echo 0)
MEM_MB=$(( MEM_KB / 1024 ))
PI_MODEL=""
if [[ -r /proc/device-tree/model ]]; then
  PI_MODEL=$(tr -d '\0' < /proc/device-tree/model)
fi
[[ -n "$PI_MODEL" ]] && info "Device: $PI_MODEL"
info "RAM: ${MEM_MB} MB"

LOW_RAM=0
if [[ "$MEM_MB" -lt 900 ]] || [[ "$PI_MODEL" == *"Zero"* ]]; then
  LOW_RAM=1
fi

# ── Temp directory ───────────────────────────────────────────────────────────

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

# ── Resolve version & download strategy ──────────────────────────────────────

# Default to download unless user forces source build, or a prior step
# (eg. low-RAM detection) already switched us to build mode.
if [[ "${PICOGALLERY_BUILD:-0}" == "1" ]]; then
  INSTALL_MODE="build"
else
  INSTALL_MODE="${INSTALL_MODE:-download}"
fi

section "Resolving version"

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

    VERSION=""
    if [[ "$HTTP_CODE" == "200" ]]; then
      VERSION=$(echo "$RELEASE_BODY" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
    fi

    if [[ -z "$VERSION" ]]; then
      die "No official 'Latest Release' found on GitHub (HTTP $HTTP_CODE). Please check $REPO_URL/releases"
    else
      info "Latest release: $VERSION"
    fi
  fi
fi

# ── Try downloading pre-built binary ─────────────────────────────────────────

if [[ "$INSTALL_MODE" == "download" ]]; then
  section "Downloading PicoGallery ${VERSION}"

  # GitHub release tags keep a leading "v" (v0.1.3-kva.7) but CI artifact
  # names drop it (picogallery-0.1.3-kva.7-linux-aarch64.tar.gz).
  RELEASE_TAG="$VERSION"
  ARTIFACT_VERSION="${VERSION#v}"
  TARBALL="picogallery-${ARTIFACT_VERSION}-linux-${ARTIFACT_ARCH}.tar.gz"
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${TARBALL}"
  SHA_URL="${DOWNLOAD_URL}.sha256"

  info "URL: $DOWNLOAD_URL"
  HTTP_CODE=$(curl -sSL -w "%{http_code}" -o "${TMPDIR}/${TARBALL}" "$DOWNLOAD_URL" 2>/dev/null) || true

  if [[ "$HTTP_CODE" == "200" ]] && [[ -s "${TMPDIR}/${TARBALL}" ]]; then
    info "Downloaded: $TARBALL"

    # Verify checksum if available
    if curl -fsSL -o "${TMPDIR}/${TARBALL}.sha256" "$SHA_URL" 2>/dev/null; then
      SAVED_DIR=$(pwd)
      cd "$TMPDIR"
      if sha256sum -c "${TARBALL}.sha256" &>/dev/null; then
        info "SHA-256 checksum verified."
      else
        die "SHA-256 checksum mismatch for ${TARBALL}; refusing to install."
      fi
      cd "$SAVED_DIR"
    else
      die "Could not download checksum for ${TARBALL}; refusing to install."
    fi

    # Extract
    info "Extracting..."
    if tar tzf "${TMPDIR}/${TARBALL}" | awk '
      /^\// || /(^|\/)\.\.($|\/)/ { exit 1 }
      { next }
    '; then
      :
    else
      die "Archive contains an unsafe path; refusing to extract."
    fi
    if tar tvzf "${TMPDIR}/${TARBALL}" | awk '$1 ~ /^[lh]/ { exit 1 }'; then
      :
    else
      die "Archive contains a symlink or hardlink; refusing to extract."
    fi
    tar xzf "${TMPDIR}/${TARBALL}" -C "${TMPDIR}" --no-same-owner --no-same-permissions
    EXTRACT_DIR=$(find "$TMPDIR" -maxdepth 1 -type d -name "picogallery-*" | head -1)

    if [[ -d "$EXTRACT_DIR" ]] && [[ -f "${EXTRACT_DIR}/picogallery" ]]; then
      info "Binary extracted successfully."
    else
      die "Archive did not contain expected binary for ${VERSION}."
    fi
  else
    die "Download failed (HTTP $HTTP_CODE). No pre-built binary found for ${VERSION} on architecture ${ARTIFACT_ARCH}."
  fi
fi

# ── Build from source ──────────────────────────────────────────────────────────
# Enabled via  --mode build|all,  --photoprism-url …,  or  PICOGALLERY_BUILD=1.
if [[ "${INSTALL_MODE:-}" == "build" ]]; then
  section "Building from source"

  # ── Choose build profile ──
  # release-fast: ~3-4x faster compile, binary ~30% larger (used on Pi Zero/low-RAM).
  # release:     slower, smaller binary.
  PROFILE="${PICOGALLERY_PROFILE:-}"
  if [[ -z "$PROFILE" ]]; then
    if [[ "$LOW_RAM" == "1" ]]; then
      PROFILE="release-fast"
    else
      PROFILE="release"
    fi
  fi

  # ── Choose feature set ──
  # Only the 'directory' plugin builds by default — it's what most users use and
  # dropping the rest roughly halves compile time. `--mode all` compiles every
  # plugin; `--photoprism-url` pulls in the photoprism plugin. PICOGALLERY_FEATURES
  # overrides all of this.
  if [[ -n "${PICOGALLERY_FEATURES:-}" ]]; then
    FEATURES="$PICOGALLERY_FEATURES"
  elif [[ "$MODE_ALL" == "1" ]]; then
    FEATURES="plugin-directory,plugin-local,plugin-google-photos,plugin-webdav,plugin-photoprism,plugin-usb"
  elif [[ -n "$PP_URL" ]]; then
    FEATURES="plugin-directory,plugin-photoprism"
  else
    FEATURES="plugin-directory"
  fi

  # ── Pick job count based on RAM ──
  # Each rustc job needs ~400-600 MB. Empirical safe rule: 1 job per ~500 MB RAM.
  CPU_COUNT=$(nproc 2>/dev/null || echo 1)
  MAX_JOBS_FROM_RAM=$(( MEM_MB / 500 ))
  [[ "$MAX_JOBS_FROM_RAM" -lt 1 ]] && MAX_JOBS_FROM_RAM=1
  JOBS=$(( CPU_COUNT < MAX_JOBS_FROM_RAM ? CPU_COUNT : MAX_JOBS_FROM_RAM ))

  if [[ "$LOW_RAM" == "1" ]]; then
    info "Low-RAM device detected — using profile='${PROFILE}', jobs=${JOBS}, features='${FEATURES}'."
    info "Expected build time: 15-25 minutes (was 40+ with full release profile)."
  else
    info "Profile='${PROFILE}', jobs=${JOBS}, features='${FEATURES}'."
    info "Expected build time: 5-10 minutes."
  fi

  # ── Enable swap on very-low-RAM Pis (Pi Zero 2 = 512 MB) ──
  SWAP_ADDED=0
  if [[ "$MEM_MB" -lt 600 ]]; then
    CURRENT_SWAP_MB=$(awk '/SwapTotal/ {print int($2/1024); exit}' /proc/meminfo 2>/dev/null || echo 0)
    if [[ "$CURRENT_SWAP_MB" -lt 1024 ]]; then
      info "Enabling 2 GB temporary swap (current swap: ${CURRENT_SWAP_MB} MB)..."
      sudo fallocate -l 2G /var/picogallery-swap 2>/dev/null \
        || sudo dd if=/dev/zero of=/var/picogallery-swap bs=1M count=2048 status=none
      sudo chmod 600 /var/picogallery-swap
      sudo mkswap /var/picogallery-swap >/dev/null
      sudo swapon /var/picogallery-swap && SWAP_ADDED=1
    fi
  fi

  # ── Install build dependencies ──
  info "Installing build dependencies..."
  sudo apt-get update -qq
  sudo apt-get install -y --no-install-recommends \
    libsdl2-dev \
    libsdl2-2.0-0 \
    libdrm-dev \
    libdrm2 \
    libgbm-dev \
    libegl-dev \
    libegl1 \
    ca-certificates \
    pkg-config \
    curl \
    git \
    gcc \
    rclone

  # ── Install Rust if needed ──
  if command -v rustup &>/dev/null; then
    info "Rust already installed — skipping update (use 'rustup update' manually if stale)."
  else
    info "Installing Rust toolchain (minimal profile)..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
      sh -s -- -y --default-toolchain stable --profile minimal
    # shellcheck source=/dev/null
    source "$HOME/.cargo/env"
  fi
  info "Rust $(rustc --version)"

  # ── Source tree ──
  # Prefer the checkout this script lives in, so a locally checked-out branch (or
  # fix) is exactly what gets built. Fall back to a shallow clone when run
  # standalone (e.g. curl | bash).
  if [[ -f "$SCRIPT_DIR/Cargo.toml" ]]; then
    SRC_DIR="$SCRIPT_DIR"
    info "Building from local checkout: $SRC_DIR"
  else
    SRC_DIR="${TMPDIR}/picogallery-src"
    info "Cloning repository (shallow, single-branch)..."
    git clone --depth 1 --single-branch "${REPO_URL}.git" "$SRC_DIR"
  fi

  # ── Build ──
  cd "$SRC_DIR"
  info "Compiling — this runs in the background; progress suppressed to reduce I/O."

  START_TS=$(date +%s)

  # CARGO_INCREMENTAL=0: release-fast already sets incremental=false, but belt-and-braces.
  # CARGO_NET_RETRY=10:  tolerate flaky Pi network during crate downloads.
  CARGO_INCREMENTAL=0 \
  CARGO_NET_RETRY=10 \
  CARGO_TERM_PROGRESS_WHEN=never \
    "$HOME/.cargo/bin/cargo" build \
      --profile "$PROFILE" \
      --no-default-features \
      --features "$FEATURES" \
      --jobs "$JOBS"

  ELAPSED=$(( $(date +%s) - START_TS ))
  info "Build finished in $(( ELAPSED / 60 ))m $(( ELAPSED % 60 ))s."

  # cargo puts release-fast output under target/release-fast/, release under target/release/
  BUILT_BINARY="$SRC_DIR/target/${PROFILE}/picogallery"
  [[ -f "$BUILT_BINARY" ]] || die "Build failed — binary not found at $BUILT_BINARY"
  info "Binary size: $(du -h "$BUILT_BINARY" | cut -f1)"

  # Tear down temporary swap.
  if [[ "$SWAP_ADDED" == "1" ]]; then
    sudo swapoff /var/picogallery-swap || true
    sudo rm -f /var/picogallery-swap || true
    info "Temporary swap removed."
  fi

  # Set up extract dir to match the download path
  EXTRACT_DIR="${TMPDIR}/picogallery-built"
  mkdir -p "$EXTRACT_DIR"
  cp "$BUILT_BINARY" "$EXTRACT_DIR/picogallery"
  cp "$SRC_DIR/config.example.toml" "$EXTRACT_DIR/" 2>/dev/null || true
  cp "$SRC_DIR/picogallery.service" "$EXTRACT_DIR/" 2>/dev/null || true
  if [[ -d "$SRC_DIR/sample_photos" ]]; then
    cp -r "$SRC_DIR/sample_photos" "$EXTRACT_DIR/" 2>/dev/null || true
  fi

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
  libgbm1 \
  libegl1 \
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

TARGET_USER="${USER_FLAG:-${SUDO_USER:-$(whoami)}}"
if ! id "$TARGET_USER" &>/dev/null; then
  die "Target user '$TARGET_USER' does not exist. Create it first, or pass --user <name>."
fi
TARGET_HOME="$(getent passwd "$TARGET_USER" | cut -d: -f6)"
[[ -n "$TARGET_HOME" && -d "$TARGET_HOME" ]] || die "Home directory for '$TARGET_USER' not found (got: ${TARGET_HOME:-<empty>}). Pass --user <name>."
if [[ "$TARGET_USER" == "root" && -z "$USER_FLAG" ]]; then
  warn "Installing for root — prefer a dedicated kiosk user:  --user dietpi"
fi
info "Installing for user: $TARGET_USER ($TARGET_HOME)"
for group in video render input; do
  if getent group "$group" &>/dev/null; then
    sudo usermod -aG "$group" "$TARGET_USER"
    info "Added $TARGET_USER to group: $group"
  fi
done

# ── Sample Photos ─────────────────────────────────────────────────────────────
#
# Ship 10 AI-generated landscape photos so the slideshow works the moment
# install.sh finishes (the directory plugin requires ≥1 photo to start).
#
# Source preference:
#   1. ${EXTRACT_DIR}/sample_photos  — bundled in the release tarball
#   2. ./sample_photos               — present when install.sh is run from a clone
#   3. raw.githubusercontent.com     — downloaded file-by-file as a last resort
#
# Photos are only copied when the target dir is empty, so user photos are
# never overwritten on a re-run.

section "Setting up sample photos"

PHOTO_DIR="${TARGET_HOME}/Pictures/PicoGallery"
sudo -u "$TARGET_USER" mkdir -p "$PHOTO_DIR"

PHOTO_DIR_IS_EMPTY=0
if [[ -z "$(ls -A "$PHOTO_DIR" 2>/dev/null)" ]]; then
  PHOTO_DIR_IS_EMPTY=1
fi

if [[ "$PHOTO_DIR_IS_EMPTY" == "1" && -z "$PP_URL" ]]; then
  SAMPLE_SRC=""
  if [[ -d "${EXTRACT_DIR}/sample_photos" ]] && \
     ls -A "${EXTRACT_DIR}/sample_photos" &>/dev/null; then
    SAMPLE_SRC="${EXTRACT_DIR}/sample_photos"
  elif [[ -d "sample_photos" ]] && ls -A "sample_photos" &>/dev/null; then
    SAMPLE_SRC="sample_photos"
  fi

  if [[ -n "$SAMPLE_SRC" ]]; then
    info "Copying sample photos from ${SAMPLE_SRC}..."
    sudo cp -r "$SAMPLE_SRC"/. "$PHOTO_DIR/" 2>/dev/null || true
  else
    info "No local samples — fetching 10 AI-generated landscape photos from GitHub..."
    SAMPLE_TMP="${TMPDIR}/samples"
    mkdir -p "$SAMPLE_TMP"
    FETCHED=0
    for i in 01 02 03 04 05 06 07 08 09 10; do
      URL="https://raw.githubusercontent.com/${REPO}/main/sample_photos/${i}.png"
      if curl -fsSL --max-time 30 -o "${SAMPLE_TMP}/${i}.png" "$URL" 2>/dev/null; then
        FETCHED=$((FETCHED + 1))
      fi
    done
    if [[ "$FETCHED" -gt 0 ]]; then
      sudo cp "$SAMPLE_TMP"/*.png "$PHOTO_DIR/" 2>/dev/null || true
      info "Installed $FETCHED sample photo(s)."
    else
      warn "Could not fetch sample photos. Add your own to $PHOTO_DIR before starting the service."
    fi
  fi

  sudo chown -R "$TARGET_USER:$TARGET_USER" "$PHOTO_DIR"
  PHOTO_COUNT=$(find "$PHOTO_DIR" -maxdepth 2 -type f \( -iname '*.png' -o -iname '*.jpg' -o -iname '*.jpeg' -o -iname '*.webp' -o -iname '*.gif' \) 2>/dev/null | wc -l | tr -d ' ')
  info "Photo directory ready: $PHOTO_DIR (${PHOTO_COUNT} photos)"
elif [[ -n "$PP_URL" ]]; then
  info "PhotoPrism mode — photos stream from the server; skipping local samples."
else
  info "Photo directory already has content — leaving it alone."
fi

# ── Config ───────────────────────────────────────────────────────────────────

section "Setting up configuration"

CONFIG_DIR="${TARGET_HOME}/.config/picogallery"
CONFIG_FILE="${CONFIG_DIR}/config.toml"
sudo -u "$TARGET_USER" mkdir -p "$CONFIG_DIR"

# Build a known-good config that matches the binary's compiled features.
# Double-quoted heredoc, so ${PHOTO_DIR} IS expanded — don't change to 'CONFIG_EOF'.
# Escape a value for a double-quoted TOML string (backslash then quote).
toml_escape() {
  local v="$1"
  v="${v//\\/\\\\}"
  v="${v//\"/\\\"}"
  printf '%s' "$v"
}

WRITE_CONFIG=0
if [[ -n "$PP_URL" ]]; then
  # PhotoPrism provisioning always (re)writes a config for the given server, so
  # a re-run picks up changed credentials. Back up any existing file first.
  WRITE_CONFIG=1
  if [[ -f "$CONFIG_FILE" ]]; then
    sudo cp "$CONFIG_FILE" "${CONFIG_FILE}.bak.$(date +%s)"
    info "Backed up existing config before writing PhotoPrism settings."
  fi
elif [[ ! -f "$CONFIG_FILE" ]]; then
  WRITE_CONFIG=1
  info "No existing config — writing a default."
elif [[ "${PICOGALLERY_RESET_CONFIG:-0}" == "1" ]]; then
  WRITE_CONFIG=1
  info "PICOGALLERY_RESET_CONFIG=1 — overwriting existing config."
  sudo cp "$CONFIG_FILE" "${CONFIG_FILE}.bak.$(date +%s)"
elif ! sudo grep -qE '^\s*enabled\s*=\s*true' "$CONFIG_FILE" 2>/dev/null; then
  # Existing config has every plugin disabled — picogallery would fail to start.
  # Back it up and write a working default.
  WRITE_CONFIG=1
  warn "Existing config has no enabled plugins — backing up and rewriting."
  sudo cp "$CONFIG_FILE" "${CONFIG_FILE}.bak.$(date +%s)"
else
  info "Config already exists at $CONFIG_FILE — keeping it."
fi

if [[ "$WRITE_CONFIG" == "1" && -n "$PP_URL" ]]; then
  # ── PhotoPrism-backed config ──
  PP_USER_EFF="${PP_USER:-admin}"
  PP_URL_ESC="$(toml_escape "$PP_URL")"
  PP_USER_ESC="$(toml_escape "$PP_USER_EFF")"
  # App password wins when supplied; otherwise username + password.
  if [[ -n "$PP_APP_PASS" ]]; then
    PP_AUTH_LINES=$(printf 'username     = "%s"\napp_password = "%s"' \
      "$PP_USER_ESC" "$(toml_escape "$PP_APP_PASS")")
  else
    PP_AUTH_LINES=$(printf 'username = "%s"\npassword = "%s"' \
      "$PP_USER_ESC" "$(toml_escape "$PP_PASS")")
  fi

  sudo -u "$TARGET_USER" tee "$CONFIG_FILE" > /dev/null <<CONFIG_EOF
# PicoGallery configuration — auto-generated by install.sh (PhotoPrism mode)
# Docs: https://github.com/kethanva/pico-gallery
# Re-provision with:  ./install.sh --photoprism-url … --photoprism-user … --photoprism-pass …

[display]
slide_duration_secs = 10
transition          = "fade"
transition_ms       = 800
fill_screen         = false
fps                 = 15
gallery_mode        = true

[targeting]
# album = ""
# favorites_only = false

[cache]
max_mb         = 256
prefetch_count = 3

# ── PhotoPrism plugin: streams photos on demand from a PhotoPrism server ──────
[[plugins]]
name     = "photoprism"
enabled  = true
url      = "${PP_URL_ESC}"
${PP_AUTH_LINES}
# order    = "newest"     # newest | oldest | added | name | random | similar
# per_page = 100
# max_thumb = "fit_1920"  # cap thumbnail size to save Pi-Zero RAM
# skip_tls_verify = false # true for self-signed LAN certificates

# ── Local directory: disabled while PhotoPrism is the source ──────────────────
[[plugins]]
name      = "directory"
enabled   = false
path      = "${PHOTO_DIR}"
order     = "shuffle"
recursive = true
CONFIG_EOF
  sudo chown "$TARGET_USER:$TARGET_USER" "$CONFIG_FILE"
  sudo chmod 600 "$CONFIG_FILE"   # holds a password — keep it private
  info "Wrote $CONFIG_FILE with PhotoPrism plugin enabled → $PP_URL"

elif [[ "$WRITE_CONFIG" == "1" ]]; then
  sudo -u "$TARGET_USER" tee "$CONFIG_FILE" > /dev/null <<CONFIG_EOF
# PicoGallery configuration — auto-generated by install.sh
# Docs: https://github.com/kethanva/pico-gallery
# Regenerate with:  PICOGALLERY_RESET_CONFIG=1 bash install.sh

[display]
slide_duration_secs = 10
transition          = "fade"
transition_ms       = 800
fill_screen         = false
fps                 = 15
gallery_mode        = true

[targeting]
# album = ""
# favorites_only = false

[cache]
max_mb         = 256
prefetch_count = 3

# ── Directory plugin: the default local-photos source ────────────────────────
# This is the only plugin guaranteed to be compiled into the Pi-Zero build
# (install.sh builds with --features plugin-directory by default).
[[plugins]]
name      = "directory"
enabled   = true
path      = "${PHOTO_DIR}"
order     = "shuffle"
recursive = true

# ── Optional plugins (only loaded if compiled in) ────────────────────────────
# Re-run with:
#   PICOGALLERY_BUILD=1 \\
#   PICOGALLERY_FEATURES="plugin-directory,plugin-local,plugin-google-photos" \\
#   bash install.sh
# to rebuild with these enabled.

[[plugins]]
name    = "local"
enabled = false
paths   = ["${PHOTO_DIR}"]

[[plugins]]
name     = "google-photos"
enabled  = false
sync_dir = "/tmp/picogallery-gdrive"

[[plugins]]
name          = "amazon-photos"
enabled       = false
client_id     = ""
client_secret = ""
CONFIG_EOF
  sudo chown "$TARGET_USER:$TARGET_USER" "$CONFIG_FILE"
  info "Wrote $CONFIG_FILE with directory plugin enabled → $PHOTO_DIR"
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
# systemd does not create /run/user/%U for non-login service users — use a
# dedicated runtime dir instead of pointing at a path that may not exist.
RuntimeDirectory=picogallery
Environment=XDG_RUNTIME_DIR=/run/picogallery
ExecStart=/usr/local/bin/picogallery
Restart=on-failure
RestartSec=10
StandardOutput=journal
StandardError=journal
UMask=0077
ProtectSystem=full
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
LockPersonality=yes
RestrictSUIDSGID=yes
TasksMax=128
MemoryMax=384M

# Allow DRM/input access without root. Wi-Fi and USB mounting require host
# policy (NetworkManager/udisks) and are deliberately not granted privileges.
SupplementaryGroups=video render input

[Install]
WantedBy=multi-user.target
SERVICE

sudo systemctl daemon-reload
info "Service installed: picogallery.service"

# ── KMS/DRM & GPU memory ─────────────────────────────────────────────────────
#
# SDL2's kmsdrm video backend needs /dev/dri/cardN. The kernel only creates
# those nodes when vc4-kms-v3d is enabled via dtoverlay. Raspberry Pi OS Lite
# ships with this on by default, but *DietPi* and other minimal distros strip
# it, which is why picogallery falls back to the 'offscreen' driver and nothing
# ever renders.
#
# With vc4-kms-v3d the firmware ignores gpu_mem= in favour of CMA, but we keep
# the gpu_mem line for old fkms/firmware setups that still honour it.

section "Configuring KMS/DRM & GPU memory"

REBOOT_REQUIRED=0

ensure_dtoverlay() {
  local cfg="$1"
  [[ -f "$cfg" ]] || return 0

  # Already active? Nothing to do.
  if grep -qE '^[[:space:]]*dtoverlay=vc4-kms-v3d([[:space:]]|,|$)' "$cfg"; then
    info "vc4-kms-v3d already enabled in $cfg"
    return 0
  fi

  # Older firmware may have the fake-KMS variant — comment it out so the real
  # KMS overlay wins.
  if grep -qE '^[[:space:]]*dtoverlay=vc4-fkms-v3d' "$cfg"; then
    sudo sed -i 's|^\([[:space:]]*\)dtoverlay=vc4-fkms-v3d|\1#dtoverlay=vc4-fkms-v3d|' "$cfg"
    info "Disabled legacy vc4-fkms-v3d in $cfg"
  fi

  {
    echo ""
    echo "# PicoGallery: enable KMS DRM so /dev/dri/cardN exists (SDL kmsdrm)"
    echo "dtoverlay=vc4-kms-v3d"
    echo "max_framebuffers=2"
  } | sudo tee -a "$cfg" > /dev/null
  info "Enabled vc4-kms-v3d in $cfg  (REBOOT REQUIRED)"
  REBOOT_REQUIRED=1
}

set_gpu_mem() {
  local cfg="$1"
  if [[ -f "$cfg" ]] && ! grep -q "^gpu_mem=" "$cfg"; then
    echo ""                          | sudo tee -a "$cfg" > /dev/null
    echo "# PicoGallery: GPU memory" | sudo tee -a "$cfg" > /dev/null
    echo "gpu_mem=64"                | sudo tee -a "$cfg" > /dev/null
    info "Set gpu_mem=64 in $cfg"
  fi
}

for cfg in /boot/firmware/config.txt /boot/config.txt; do
  ensure_dtoverlay "$cfg"
  set_gpu_mem     "$cfg"
done

# Post-flight: does /dev/dri exist NOW? If not, a reboot is needed for the
# overlay to take effect.
if [[ ! -d /dev/dri ]] || ! ls /dev/dri/card* &>/dev/null; then
  warn "/dev/dri is empty right now — the vc4 DRM module isn't loaded yet."
  warn "This is expected on a fresh DietPi install; a reboot will create /dev/dri/cardN."
  REBOOT_REQUIRED=1
fi

# ── Enable & start ───────────────────────────────────────────────────────────

section "Enabling service"

sudo systemctl enable picogallery
info "PicoGallery will start automatically on boot."

if [[ "${REBOOT_REQUIRED:-0}" == "1" ]]; then
  warn "Skipping service start — reboot first so /dev/dri is available."
elif [[ "$ASSUME_YES" == "1" ]]; then
  info "Starting picogallery service..."
  if ! sudo systemctl restart picogallery; then
    warn "Service start failed. After fixing issues: sudo systemctl restart picogallery"
    warn "Logs: sudo journalctl -u picogallery -n 50 --no-pager"
  fi
fi

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
if [[ -n "$PP_URL" ]]; then
echo "  Source  : PhotoPrism @ ${PP_URL} (user: ${PP_USER:-admin})"
fi
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

if [[ "${REBOOT_REQUIRED:-0}" == "1" ]]; then
  echo -e "${YELLOW}${BOLD}⚠  REBOOT REQUIRED${RESET}"
  echo -e "${YELLOW}   The kernel DRM device /dev/dri/cardN isn't present yet.${RESET}"
  echo -e "${YELLOW}   Run ${BOLD}sudo reboot${RESET}${YELLOW} before starting picogallery,${RESET}"
  echo -e "${YELLOW}   otherwise SDL will fall back to the 'offscreen' driver and fail.${RESET}"
  echo ""
fi
