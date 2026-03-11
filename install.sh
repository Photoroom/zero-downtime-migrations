#!/bin/bash
# Zero-Downtime Migrations Installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Photoroom/zero-downtime-migrations/main/install.sh | bash
#
# Environment variables:
#   ZDM_INSTALL_DIR - Installation directory (default: ~/.local/bin)
#   ZDM_VERSION     - Version to install (default: latest)

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() {
    echo -e "${BLUE}info:${NC} $1"
}

success() {
    echo -e "${GREEN}success:${NC} $1"
}

warn() {
    echo -e "${YELLOW}warning:${NC} $1"
}

error() {
    echo -e "${RED}error:${NC} $1" >&2
    exit 1
}

# Detect OS and architecture
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Linux*)
            os="unknown-linux-gnu"
            ;;
        Darwin*)
            os="apple-darwin"
            ;;
        MINGW*|MSYS*|CYGWIN*)
            os="pc-windows-msvc"
            ;;
        *)
            error "Unsupported operating system: $(uname -s)"
            ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64)
            arch="x86_64"
            ;;
        arm64|aarch64)
            arch="aarch64"
            ;;
        *)
            error "Unsupported architecture: $(uname -m)"
            ;;
    esac

    echo "${arch}-${os}"
}

# Get the latest version from GitHub
get_latest_version() {
    local latest
    latest=$(curl -fsSL "https://api.github.com/repos/Photoroom/zero-downtime-migrations/releases/latest" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')

    if [[ -z "$latest" ]]; then
        error "Failed to fetch latest version"
    fi

    echo "$latest"
}

# Download and install
install() {
    local platform version install_dir download_url temp_dir

    platform=$(detect_platform)
    version="${ZDM_VERSION:-$(get_latest_version)}"
    install_dir="${ZDM_INSTALL_DIR:-$HOME/.local/bin}"

    info "Detected platform: $platform"
    info "Installing version: $version"
    info "Install directory: $install_dir"

    # Create install directory
    mkdir -p "$install_dir"

    # Determine file extension
    local ext=""
    if [[ "$platform" == *"windows"* ]]; then
        ext=".exe"
    fi

    # Build download URL
    download_url="https://github.com/Photoroom/zero-downtime-migrations/releases/download/${version}/zdm-${platform}${ext}"

    info "Downloading from: $download_url"

    # Create temp directory
    temp_dir=$(mktemp -d)
    trap "rm -rf $temp_dir" EXIT

    # Download binary
    if ! curl -fsSL "$download_url" -o "$temp_dir/zdm${ext}"; then
        error "Failed to download zdm. Please check that version $version exists."
    fi

    # Make executable (not needed on Windows)
    if [[ "$platform" != *"windows"* ]]; then
        chmod +x "$temp_dir/zdm${ext}"
    fi

    # Move to install directory
    mv "$temp_dir/zdm${ext}" "$install_dir/zdm${ext}"

    success "Installed zdm to $install_dir/zdm${ext}"

    # Check if install dir is in PATH
    if [[ ":$PATH:" != *":$install_dir:"* ]]; then
        warn "$install_dir is not in your PATH"
        echo ""
        echo "Add it to your PATH by adding this line to your shell config:"
        echo ""
        echo "  export PATH=\"$install_dir:\$PATH\""
        echo ""
    fi

    # Verify installation
    if command -v "$install_dir/zdm${ext}" &> /dev/null; then
        echo ""
        info "Verifying installation..."
        "$install_dir/zdm${ext}" --version
        echo ""
        success "zdm is ready to use!"
        echo ""
        echo "Quick start:"
        echo "  zdm .                     # Lint all migrations"
        echo "  zdm --diff origin/main    # Lint changed migrations"
        echo "  zdm rule R001             # Show rule documentation"
    fi
}

# Main
main() {
    echo ""
    echo "  ╔═══════════════════════════════════════════╗"
    echo "  ║  Zero-Downtime Migrations Installer       ║"
    echo "  ║  PostgreSQL migration safety for Django   ║"
    echo "  ╚═══════════════════════════════════════════╝"
    echo ""

    install
}

main
