#!/usr/bin/env bash
# Build ScanPro's Linux installers (.deb + AppImage + pacman) into dist/.
# Needs: Rust, cargo-packager, and the Slint/winit build deps:
#   Debian/Ubuntu: sudo apt install build-essential pkg-config libfontconfig1-dev libxkbcommon-dev libsane1
#   Fedora:        sudo dnf install gcc pkgconf fontconfig-devel libxkbcommon-devel sane-backends-libs
set -euo pipefail
cd "$(dirname "$0")/../.."
command -v cargo-packager >/dev/null || { echo "Install cargo-packager: cargo install cargo-packager --locked" >&2; exit 1; }
cargo packager --release --formats deb,appimage,pacman "$@"
ls -1 dist
