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
rm -f "$out"
cp target/release/rosync "$out"
chmod +x "$out"
echo "built: $(pwd)/$out"

# Keep the optional compiler-only lint pass working for source builds too. The
# installer pins and verifies both the official archive and extracted binary.
# A missing Node/network does not invalidate the Rust build; release bundles
# always include the verified compiler.
if command -v node >/dev/null 2>&1; then
  if ! node ../scripts/install-luau-compiler.mjs; then
    echo "warning: Luau compiler install failed; run 'node scripts/install-luau-compiler.mjs' from the widget root" >&2
  fi
else
  echo "warning: Node.js not found; run scripts/install-luau-compiler.mjs later to enable compiler lint checks" >&2
fi
