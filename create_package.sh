#!/usr/bin/env bash
set -e

# Colors
GREEN='\033[0;32m'
BOLD='\033[1m'
RESET='\033[RESET'

echo -e "${BOLD}Starting Package Creation for Raspberry Pi Zero 2...${RESET}"

# 1. Install cargo-deb if not present
if ! command -v cargo-deb &> /dev/null; then
    echo "Installing packaging tools (cargo-deb)..."
    cargo install cargo-deb
fi

# 2. Build the optimized binary
echo "Compiling optimized release binary..."
cargo build --release

# 3. Generate the .deb package
echo "Generating .deb package..."
cargo deb --no-build

# 4. Success
PACKAGE=$(ls target/debian/*.deb | head -n 1)
echo -e "\n${GREEN}${BOLD}SUCCESS!${RESET}"
echo "Your installer is ready at: ${BOLD}$PACKAGE${RESET}"
echo "To install it on any Pi, run:"
echo "  sudo dpkg -i $PACKAGE"
