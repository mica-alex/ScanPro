#!/usr/bin/env bash
# Build ScanPro's macOS installers (ScanPro.app + .dmg) into dist/.
# Needs: Rust (rustup) and cargo-packager (`cargo install cargo-packager --locked`).
set -euo pipefail
cd "$(dirname "$0")/../.."
command -v cargo-packager >/dev/null || { echo "Install cargo-packager: cargo install cargo-packager --locked" >&2; exit 1; }
cargo packager --release --formats app,dmg "$@"
ls -1 dist
