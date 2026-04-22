#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
CARGO="${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}"
"$CARGO" build --release
cp target/release/rosync rosync-darwin-arm64
chmod +x rosync-darwin-arm64
