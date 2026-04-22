#!/usr/bin/env bash
# Build the rosync daemon for the current POSIX host. Names the output binary
# per-platform so the widget's platform-aware binary lookup finds it.
set -euo pipefail
cd "$(dirname "$0")"
CARGO="${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}"
"$CARGO" build --release

uname_s="$(uname -s)"
uname_m="$(uname -m)"
case "$uname_s" in
  Darwin)  out="rosync-darwin-${uname_m/x86_64/x86_64}";;
  Linux)   out="rosync-linux-${uname_m/aarch64/arm64}";;
  *)       out="rosync-${uname_s}-${uname_m}";;
esac
cp target/release/rosync "$out"
chmod +x "$out"
echo "built: $(pwd)/$out"
