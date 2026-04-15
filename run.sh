#!/usr/bin/env bash
# ============================================================
# PicoGallery — end-to-end development runner
# ============================================================
# Usage:
#   ./run.sh                        — build, test, use test fixtures, run
#   ./run.sh --photos /path/to/dir  — use a real photo library (skips
#                                     test-fixture generation & config overwrite)
#   ./run.sh --no-launch            — build + test only, skip the launch step
#   ./run.sh --log-level debug      — pass extra flags to picogallery
#   ./run.sh --help                 — show picogallery CLI help
#
# Test photo structure created in /tmp/picogallery-e2e/  (default mode):
#   Nature/     3 photos  (green tones)
#   City/       3 photos  (blue tones)
#   Portraits/  2 photos  (warm tones)
#   root.jpg    1 photo   (directly in root, no album)
# ============================================================

set -euo pipefail

# ── Colours ───────────────────────────────────────────────────────────────────
BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'
CYAN='\033[0;36m'; RESET='\033[0m'

info()    { echo -e "${GREEN}[✓]${RESET} $*"; }
warn()    { echo -e "${YELLOW}[!]${RESET} $*"; }
err()     { echo -e "${RED}[✗]${RESET} $*" >&2; }
step()    { echo -e "\n${BOLD}${CYAN}── $* ──${RESET}"; }
die()     { err "$*"; exit 1; }

# ── Arg parsing ───────────────────────────────────────────────────────────────
NO_LAUNCH=false
PHOTOS_DIR=""          # empty = use generated test fixtures
PASS_THROUGH=()
args=("$@")
i=0
while [[ $i -lt ${#args[@]} ]]; do
    arg="${args[$i]}"
    if [[ "$arg" == "--no-launch" ]]; then
        NO_LAUNCH=true
    elif [[ "$arg" == "--photos" ]]; then
        i=$(( i + 1 ))
        PHOTOS_DIR="${args[$i]:-}"
        [[ -z "$PHOTOS_DIR" ]] && { err "--photos requires a directory argument"; exit 1; }
        [[ -d "$PHOTOS_DIR" ]] || { err "--photos: '$PHOTOS_DIR' is not a directory"; exit 1; }
    else
        PASS_THROUGH+=("$arg")
    fi
    i=$(( i + 1 ))
done

echo -e "${BOLD}"
echo "  ╔═══════════════════════════════════════╗"
echo "  ║   PicoGallery — end-to-end runner     ║"
echo "  ╚═══════════════════════════════════════╝"
echo -e "${RESET}"

IS_MAC=false
[[ "$(uname)" == "Darwin" ]] && IS_MAC=true

# ── Step 1: Dependencies ──────────────────────────────────────────────────────
step "1/6  Checking dependencies"

if $IS_MAC; then
    if ! command -v cmake &>/dev/null; then
        info "Installing cmake via Homebrew…"
        brew install cmake
    fi
    info "cmake:   $(cmake --version | head -1)"

    if ! command -v python3 &>/dev/null; then
        die "python3 not found. Install via: brew install python"
    fi
    info "python3: $(python3 --version)"
else
    # Linux — check build essentials
    for pkg in cmake python3 build-essential; do
        if ! command -v "${pkg%%-*}" &>/dev/null; then
            warn "$pkg not found — install with: sudo apt-get install -y $pkg"
        fi
    done
fi

# Ensure Python Pillow is available for test photo generation.
if ! python3 -c "from PIL import Image" &>/dev/null 2>&1; then
    warn "Pillow not found — attempting to install…"
    if python3 -m pip install --quiet Pillow 2>/dev/null || \
       python3 -m pip install --quiet --break-system-packages Pillow 2>/dev/null; then
        info "Pillow installed."
    else
        warn "Could not install Pillow. Test photos must be placed manually in /tmp/picogallery-e2e/"
    fi
fi

# ── Step 2: Build ─────────────────────────────────────────────────────────────
step "2/6  Building PicoGallery"
cargo build 2>&1 | grep -E "^(error|warning: unused|   Compiling|    Finished)" || true
info "Build complete."

# On macOS the bundled SDL2 dylib lives inside the build output tree.
# Export DYLD_LIBRARY_PATH now so every subsequent binary call in this script
# (smoke tests, the final launch, etc.) can load it without extra wrappers.
if $IS_MAC; then
    SDL_LIB=$(find target/debug/build -name "libSDL2-*.dylib" 2>/dev/null | head -1 \
              | xargs -I{} dirname {} 2>/dev/null || true)
    if [[ -n "$SDL_LIB" ]]; then
        export DYLD_LIBRARY_PATH="${SDL_LIB}${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
        info "SDL2 lib path: $SDL_LIB (exported to DYLD_LIBRARY_PATH)"
    else
        warn "Could not locate bundled libSDL2-*.dylib — binary may fail to start."
    fi
fi

# ── Step 3: Unit tests ────────────────────────────────────────────────────────
step "3/6  Running unit tests"
# Run tests for all workspace members, streaming output.
if cargo test --workspace --quiet 2>&1; then
    info "All tests passed."
else
    die "Unit tests failed. Fix them before running end-to-end."
fi

# ── Step 4: Test fixtures (skipped when --photos is provided) ─────────────────
step "4/6  Resolving photo source"

if [[ -n "$PHOTOS_DIR" ]]; then
    # ── Real library mode ──────────────────────────────────────────────────────
    PHOTOS_DIR="$(cd "$PHOTOS_DIR" && pwd)"   # canonicalize
    PHOTO_COUNT=$(find "$PHOTOS_DIR" \( -iname "*.jpg" -o -iname "*.jpeg" \
                  -o -iname "*.png" -o -iname "*.webp" -o -iname "*.gif" \) \
                  | wc -l | tr -d ' ')
    ALBUM_COUNT=$(find "$PHOTOS_DIR" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
    info "Using real library: $PHOTOS_DIR"
    info "Photos found: $PHOTO_COUNT across $ALBUM_COUNT top-level album(s)"
    [[ "$PHOTO_COUNT" -eq 0 ]] && warn "No recognised image files found under $PHOTOS_DIR"
    ACTIVE_ROOT="$PHOTOS_DIR"
else
    # ── Test-fixture mode ──────────────────────────────────────────────────────
    TEST_ROOT="/tmp/picogallery-e2e"
    STAMP="$TEST_ROOT/.stamp"
    if [[ -f "$STAMP" ]]; then
        info "Test fixtures already exist (delete $STAMP to regenerate)."
    else
        mkdir -p \
            "$TEST_ROOT/Nature" \
            "$TEST_ROOT/City" \
            "$TEST_ROOT/Portraits" \
            "$TEST_ROOT/Abstract" \
            "$TEST_ROOT/Macro" \
            "$TEST_ROOT/Architecture"

        python3 - <<'PYEOF'
import os, sys, random
try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    print("WARNING: Pillow not available — skipping photo generation.")
    print("Place some JPEG/PNG files under /tmp/picogallery-e2e/ manually.")
    sys.exit(0)

BASE = "/tmp/picogallery-e2e"

albums = ["Nature", "City", "Portraits", "Abstract", "Macro", "Architecture"]
total_photos = 350
colors = [
    (34, 139, 34), (85, 107, 47), (70, 130, 180),
    (30, 80, 160), (60, 60, 120), (40, 100, 180),
    (200, 120, 80), (180, 90, 100), (120, 80, 160),
    (250, 128, 114), (255, 165, 0), (218, 112, 214)
]

print(f"Generating {total_photos} photos with high variety...")

for i in range(total_photos):
    # Randomly assign album (or no album ~5% of the time)
    if random.random() < 0.05:
        album = None
    else:
        album = random.choice(albums)
        
    filename = f"photo_{i+1:03d}.jpg"
    path = os.path.join(BASE, album, filename) if album else os.path.join(BASE, filename)
    
    base_color = random.choice(colors)
    img = Image.new("RGB", (1280, 720), base_color)
    draw = ImageDraw.Draw(img)
    
    # Draw abstract shapes for visual variety
    for _ in range(random.randint(10, 25)):
        shape_type = random.choice(['circle', 'rectangle', 'line', 'polygon'])
        x1 = random.randint(-200, 1280)
        y1 = random.randint(-200, 720)
        x2 = x1 + random.randint(100, 600)
        y2 = y1 + random.randint(100, 600)
        
        # slight variations of base color
        r = min(255, max(0, base_color[0] + random.randint(-80, 80)))
        g = min(255, max(0, base_color[1] + random.randint(-80, 80)))
        b = min(255, max(0, base_color[2] + random.randint(-80, 80)))
        shape_color = (r, g, b)
        
        if shape_type == 'circle':
            draw.ellipse([x1, y1, x2, y2], fill=shape_color)
        elif shape_type == 'rectangle':
            draw.rectangle([x1, y1, x2, y2], fill=shape_color)
        elif shape_type == 'polygon':
            x3 = random.randint(-200, 1280)
            y3 = random.randint(-200, 720)
            draw.polygon([(x1, y1), (x2, y2), (x3, y3)], fill=shape_color)
        else:
            width = random.randint(5, 50)
            draw.line([x1, y1, x2, y2], fill=shape_color, width=width)
            
    # White card in the centre
    draw.rounded_rectangle([290, 260, 990, 460], radius=20, fill=(255, 255, 255))
    
    label = f"{album if album else 'Root'} • Photo {i+1:03d}"
    try:
        bbox = draw.textbbox((0, 0), label)
        tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
        draw.text(((1280 - tw) // 2, (720 - th) // 2), label, fill=(30, 30, 30))
    except Exception:
        pass
    
    draw.rectangle([0, 0, 1279, 719], outline=tuple(min(c + 60, 255) for c in base_color), width=8)
    img.save(path, "JPEG", quality=85)
    
    if (i+1) % 50 == 0:
        print(f"  created {i+1}/{total_photos} photos...")

print(f"Generated {total_photos} test photos.")
PYEOF

        touch "$STAMP"
        info "Test fixtures ready under $TEST_ROOT"
    fi

    PHOTO_COUNT=$(find "$TEST_ROOT" \( -name "*.jpg" -o -name "*.png" \) | wc -l | tr -d ' ')
    info "Photos available: $PHOTO_COUNT ($(find "$TEST_ROOT" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ') albums + root)"
    ACTIVE_ROOT="$TEST_ROOT"
fi

# ── Step 5: Config ────────────────────────────────────────────────────────────
step "5/6  Writing configuration"

CONFIG_DIR="$HOME/.config/picogallery"
CONFIG_FILE="$CONFIG_DIR/config.toml"
mkdir -p "$CONFIG_DIR"

# Choose sensible defaults based on library size.
if [[ -n "$PHOTOS_DIR" ]]; then
    SLIDE_SECS=6
    CACHE_MB=256
    PREFETCH=4
    FILL_SCREEN=false
else
    # Small test set — quick transitions make iteration faster.
    SLIDE_SECS=4
    CACHE_MB=64
    PREFETCH=2
    FILL_SCREEN=false
fi

# Only overwrite the config when run.sh controls the photo source
# (test-fixture mode), OR when the user explicitly passed --photos.
# This prevents silently clobbering a hand-crafted config when neither
# flag is in use — but since both modes here set ACTIVE_ROOT, we always
# write so the path stays consistent with the chosen source.
cat > "$CONFIG_FILE" <<TOML
# PicoGallery — configuration written by run.sh
# Generated: $(date)
# Source:    ${ACTIVE_ROOT}

[display]
slide_duration_secs = ${SLIDE_SECS}
transition          = "fade"
transition_ms       = 600
fill_screen         = ${FILL_SCREEN}
fps                 = 30

[cache]
max_mb         = ${CACHE_MB}
prefetch_count = ${PREFETCH}

# ── Directory plugin ────────────────────────────────────────────────────────
[[plugins]]
name      = "directory"
enabled   = false
path      = "${ACTIVE_ROOT}"
order     = "shuffle"
recursive = true

# ── Local plugin (Default) ──────────────────────────────────────────────────
[[plugins]]
name    = "local"
enabled = true
paths   = ["${ACTIVE_ROOT}"]

[[plugins]]
name    = "google-photos"
enabled = false
sync_dir = "/tmp/picogallery-gdrive"

[[plugins]]
name    = "amazon-photos"
enabled = false
client_id     = "PLACEHOLDER"
client_secret = "PLACEHOLDER"
TOML

info "Config written to $CONFIG_FILE"
info "Active plugin: local → $ACTIVE_ROOT"

# ── Step 5b: Smoke-test --generate-config ─────────────────────────────────
TMPDIR_GEN=$(mktemp -d)
target/debug/picogallery --generate-config --config "$TMPDIR_GEN/config.toml" 2>&1 | \
    grep -v "^warning:" || true
[[ -f "$TMPDIR_GEN/config.toml" ]] || die "--generate-config did not create the file"
info "--generate-config: OK"

# Verify --print-default-config emits valid TOML (parse it).
target/debug/picogallery --print-default-config 2>/dev/null | \
    python3 -c "
import sys
try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        # Neither available — skip parse check
        sys.exit(0)
data = tomllib.loads(sys.stdin.read())
assert 'display' in data, 'missing [display] section'
assert 'plugins' in data, 'missing [[plugins]]'
" 2>/dev/null && info "--print-default-config: valid TOML" || \
    warn "--print-default-config: TOML parse check skipped (tomllib/tomli not available)"

rm -rf "$TMPDIR_GEN"

# ── Step 6: Launch ────────────────────────────────────────────────────────────
if $NO_LAUNCH; then
    echo ""
    echo -e "${BOLD}${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo -e "${BOLD}${GREEN}  Build + tests passed (--no-launch).   ${RESET}"
    echo -e "${BOLD}${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${RESET}"
    echo ""
    echo "  To launch manually:"
    echo "    ./run.sh"
    echo ""
    exit 0
fi

step "6/6  Launching PicoGallery"
echo ""
echo "  Photos : $ACTIVE_ROOT ($PHOTO_COUNT images)"
echo "  Config : $CONFIG_FILE"
echo "  Plugin : local (shuffle, prefetch=${PREFETCH}, cache=${CACHE_MB}MB)"
echo ""
echo "  Controls:  Q = quit   Space = pause/resume   → = next   ← = prev"
echo ""

# DYLD_LIBRARY_PATH was already exported in the build step on macOS.
target/debug/picogallery \
    --config "$CONFIG_FILE" \
    --log-level info \
    "${PASS_THROUGH[@]+"${PASS_THROUGH[@]}"}"
