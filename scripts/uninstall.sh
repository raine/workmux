#!/usr/bin/env bash
#
# workmux uninstall script
#
# Removes workmux binary, status tracking hooks, skills, and data
# directories. Handles all install methods.
#
# Usage: curl -fsSL https://raw.githubusercontent.com/raine/workmux/main/scripts/uninstall.sh | bash
#
# IMPORTANT: This script must be EXECUTED, never SOURCED.

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}==>${NC} $1"; }
log_success() { echo -e "${GREEN}==>${NC} $1"; }
log_warning() { echo -e "${YELLOW}==>${NC} $1"; }
log_error() { echo -e "${RED}Error:${NC} $1" >&2; }

echo ""
echo "  workmux uninstall"
echo ""

# 1. Detect install method and delegate to package manager if applicable
brew_path=""
if command -v brew &>/dev/null; then
    brew_path=$(brew --prefix workmux 2>/dev/null || true)
fi

cargo_bin="$HOME/.cargo/bin/workmux"
local_bin="$HOME/.local/bin/workmux"
usr_bin="/usr/local/bin/workmux"
workmux_path="$(command -v workmux 2>/dev/null || true)"

if [ -n "$brew_path" ]; then
    log_info "Detected Homebrew installation."
    log_info "Run these commands to finish uninstalling:"
    echo "  brew uninstall workmux"
    echo "  brew untap raine/workmux"
    echo ""
    log_info "You can also run the remaining cleanup steps:"
fi

if [ "$workmux_path" = "$cargo_bin" ] || [ -f "$cargo_bin" ]; then
    log_info "Detected Cargo installation."
    log_info "Run this command to finish uninstalling:"
    echo "  cargo uninstall workmux"
    echo ""
    log_info "You can also run the remaining cleanup steps:"
fi

# 2. If workmux binary is available, run binary uninstall first
if [ -n "$workmux_path" ]; then
    log_info "Removing status tracking hooks and skills..."
    if ! workmux uninstall 2>&1 | sed 's/^/  /'; then
        log_warning "workmux uninstall encountered issues (see above)"
    fi
    echo ""
fi

# 3. Remove binary from known locations
log_info "Removing workmux binary..."
for path in "$usr_bin" "$local_bin" "$cargo_bin"; do
    if [ -f "$path" ]; then
        if [ -w "$(dirname "$path")" ]; then
            rm -f "$path"
            log_success "Removed $path"
        else
            log_info "Removing $path requires sudo..."
            sudo rm -f "$path"
            log_success "Removed $path"
        fi
    fi
done
echo ""

# 4. Manual cleanup guidance
log_info "Manual cleanup (check your shell config):"
echo "  - Remove 'eval \"\$(workmux completions bash)\"' or similar from"
echo "    ~/.bashrc, ~/.zshrc, ~/.config/fish/config.fish, or nushell's config.nu"
echo "  - Remove 'alias wm=workmux' from your shell config"
echo ""

log_info "Configuration preserved at ~/.config/workmux/"
echo "  Remove manually with: rm -rf ~/.config/workmux"
echo ""

log_info "Worktrees remain on disk:"
echo "  Clean up with: git worktree list && git worktree remove <path>"
echo ""

log_success "workmux has been uninstalled."
