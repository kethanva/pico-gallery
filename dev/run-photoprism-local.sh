#!/usr/bin/env bash
# ============================================================
# PicoGallery — local PhotoPrism end-to-end runner
# ============================================================
# Boots a PhotoPrism container on http://localhost:2342, seeds it with a few
# test photos (if Pillow is available), writes a picogallery config that uses
# the new `photoprism` plugin, builds picogallery, and launches it.
#
# Usage:
#   dev/run-photoprism-local.sh                      build + run
#   dev/run-photoprism-local.sh --no-launch          stack up + config + build only
#   dev/run-photoprism-local.sh --down               stop and remove the PhotoPrism stack
#   dev/run-photoprism-local.sh --url URL --user U --pass P
#                                                    point at an existing PhotoPrism
#                                                    server instead of starting Docker
#
# Requirements:
#   - Docker + `docker compose` (unless --url is passed)
#   - Rust toolchain (`cargo`)
#   - macOS: cmake (Homebrew)
#   - Linux: libsdl2-dev pkg-config cmake clang build-essential
# ============================================================

set -euo pipefail

BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'
CYAN='\033[0;36m'; RESET='\033[0m'
info() { echo -e "${GREEN}[\xe2\x9c\x93]${RESET} $*"; }
warn() { echo -e "${YELLOW}[!]${RESET} $*"; }
err()  { echo -e "${RED}[\xe2\x9c\x97]${RESET} $*" >&2; }
step() { echo -e "\n${BOLD}${CYAN}\xe2\x94\x80\xe2\x94\x80 $* \xe2\x94\x80\xe2\x94\x80${RESET}"; }
die()  { err "$*"; exit 1; }

# ── Paths ─────────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE_DIR="$SCRIPT_DIR/photoprism"
ORIGINALS_DIR="$COMPOSE_DIR/originals"

# ── Defaults ──────────────────────────────────────────────────────────────────
PP_URL="http://localhost:2342"
PP_USER="admin"
PP_PASS="insecure"
NO_LAUNCH=false
DOWN_ONLY=false
USE_DOCKER=true   # set false when --url is provided

# ── Arg parsing ───────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-launch) NO_LAUNCH=true; shift ;;
        --down)      DOWN_ONLY=true; shift ;;
        --url)       PP_URL="$2"; USE_DOCKER=false; shift 2 ;;
        --user)      PP_USER="$2"; shift 2 ;;
        --pass)      PP_PASS="$2"; shift 2 ;;
        -h|--help)
            sed -n '1,25p' "$0"; exit 0 ;;
        *) die "Unknown arg: $1 (try --help)" ;;
    esac
done

cd "$REPO_ROOT"

# ── --down: tear down and exit ───────────────────────────────────────────────
if $DOWN_ONLY; then
    step "Stopping PhotoPrism stack"
    (cd "$COMPOSE_DIR" && docker compose down) || warn "compose down failed"
    info "Stack stopped. Persistent data kept under $COMPOSE_DIR/storage and originals."
    exit 0
fi

echo -e "${BOLD}"
echo "  \xe2\x95\x94\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x97"
echo "  \xe2\x95\x91   PicoGallery + PhotoPrism (local)   \xe2\x95\x91"
echo "  \xe2\x95\x9a\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x90\xe2\x95\x9d"
echo -e "${RESET}"

# ── Step 1: PhotoPrism server ────────────────────────────────────────────────
if $USE_DOCKER; then
    step "1/5  Starting PhotoPrism (Docker)"
    command -v docker        >/dev/null || die "docker not found. Install Docker Desktop or docker.io."
    docker compose version   >/dev/null || die "'docker compose' plugin not available."

    mkdir -p "$ORIGINALS_DIR" "$COMPOSE_DIR/storage"

    # Seed a handful of test photos if the originals dir is empty.
    if [[ -z "$(ls -A "$ORIGINALS_DIR" 2>/dev/null || true)" ]]; then
        info "Seeding test photos into $ORIGINALS_DIR"
        if python3 -c "from PIL import Image" 2>/dev/null; then
            python3 - "$ORIGINALS_DIR" <<'PYEOF'
import os, sys, random
from PIL import Image, ImageDraw
base = sys.argv[1]
colours = [(220, 60, 60), (60, 140, 220), (60, 180, 80),
           (200, 160, 40), (140, 60, 200), (60, 180, 180)]
for i, c in enumerate(colours, 1):
    img = Image.new("RGB", (1280, 720), c)
    d = ImageDraw.Draw(img)
    d.rectangle([340, 260, 940, 460], fill=(255, 255, 255))
    d.text((640, 360), f"Test photo {i}", fill=(30, 30, 30), anchor="mm")
    img.save(os.path.join(base, f"test_{i:02d}.jpg"), "JPEG", quality=85)
print(f"Seeded {len(colours)} photos.")
PYEOF
        else
            warn "Pillow not installed; drop your own JPEGs into $ORIGINALS_DIR"
        fi
    else
        info "Originals dir already populated; leaving as-is."
    fi

    (cd "$COMPOSE_DIR" && docker compose up -d)
    info "Container starting; waiting for HTTP on $PP_URL ..."

    # Poll /api/v1/status until 200 (up to ~60s).
    for i in $(seq 1 60); do
        if curl -fsS --max-time 2 "$PP_URL/api/v1/status" >/dev/null 2>&1; then
            info "PhotoPrism is up (after ${i}s)."
            break
        fi
        sleep 1
        [[ $i -eq 60 ]] && die "PhotoPrism did not come up within 60s. Check 'docker logs picogallery-photoprism-dev'."
    done

    info "Triggering background index of seeded photos ..."
    # Open a session, then POST /index — best-effort; failures are non-fatal.
    SID=$(curl -fsS -X POST "$PP_URL/api/v1/session" \
              -H 'Content-Type: application/json' \
              -d "{\"username\":\"$PP_USER\",\"password\":\"$PP_PASS\"}" 2>/dev/null \
          | python3 -c "import sys,json;d=json.load(sys.stdin);print(d.get('id') or d.get('session_id') or '')" \
          2>/dev/null || true)
    if [[ -n "$SID" ]]; then
        curl -fsS -X POST "$PP_URL/api/v1/index" \
             -H "X-Session-ID: $SID" -H 'Content-Type: application/json' \
             -d '{"path":"","rescan":false,"cleanup":false}' >/dev/null 2>&1 \
             || warn "Index trigger failed (you can index manually in the UI at $PP_URL)"
    else
        warn "Could not open session for indexing (ok if seed dir was empty)."
    fi
else
    step "1/5  Using external PhotoPrism at $PP_URL"
    curl -fsS --max-time 5 "$PP_URL/api/v1/status" >/dev/null \
        || die "Cannot reach $PP_URL/api/v1/status — check URL / network."
    info "PhotoPrism reachable."
fi

# ── Step 2: Build ────────────────────────────────────────────────────────────
step "2/5  Building picogallery (default features incl. photoprism)"
cargo build 2>&1 | grep -E "^(error|warning: unused|   Compiling|    Finished)" || true

# On macOS export the bundled SDL2 dylib path so the binary can load it.
if [[ "$(uname)" == "Darwin" ]]; then
    SDL_LIB=$(find target/debug/build -name "libSDL2-*.dylib" 2>/dev/null | head -1 \
              | xargs -I{} dirname {} 2>/dev/null || true)
    if [[ -n "$SDL_LIB" ]]; then
        export DYLD_LIBRARY_PATH="${SDL_LIB}${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
        info "SDL2 lib path: $SDL_LIB"
    fi
fi

# ── Step 3: Tests ────────────────────────────────────────────────────────────
step "3/5  Running PhotoPrism plugin tests"
cargo test -p picogallery-photoprism --quiet || die "PhotoPrism plugin tests failed."
info "All plugin tests passed."

# ── Step 4: Config ───────────────────────────────────────────────────────────
step "4/5  Writing picogallery config"
CONFIG_DIR="$HOME/.config/picogallery"
CONFIG_FILE="$CONFIG_DIR/config.toml"
mkdir -p "$CONFIG_DIR"

cat > "$CONFIG_FILE" <<TOML
# PicoGallery — local PhotoPrism dev config (written by dev/run-photoprism-local.sh)
# Generated: $(date)

[display]
slide_duration_secs = 4
transition          = "fade"
transition_ms       = 500
fill_screen         = false
fps                 = 30
order               = "newest_first"
show_osd            = true

[cache]
max_mb         = 128
prefetch_count = 2

[[plugins]]
name     = "photoprism"
enabled  = true
url      = "${PP_URL}"
username = "${PP_USER}"
password = "${PP_PASS}"
order    = "newest"
per_page = 50
max_thumb      = "fit_1920"
allow_original = true
skip_tls_verify = false
TOML

info "Config written to $CONFIG_FILE"
info "Plugin: photoprism -> $PP_URL (user: $PP_USER)"

# ── Step 5: Launch ───────────────────────────────────────────────────────────
if $NO_LAUNCH; then
    echo ""
    echo "Stack and config ready. Launch manually with:"
    echo "  target/debug/picogallery --config $CONFIG_FILE"
    echo ""
    echo "Stop the PhotoPrism container with: dev/run-photoprism-local.sh --down"
    exit 0
fi

step "5/5  Launching picogallery"
echo ""
echo "  PhotoPrism : $PP_URL  (UI: open this in a browser to add/manage photos)"
echo "  Config     : $CONFIG_FILE"
echo ""
echo "  Controls   :  Q = quit   Space = pause/resume   \xe2\x86\x92 = next   \xe2\x86\x90 = prev"
echo ""

target/debug/picogallery --config "$CONFIG_FILE" --log-level info
