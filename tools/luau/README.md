# Pinned Luau compiler

`rosync lint` checks `tools/luau/<platform>/luau-compile` before optional
external installations. The compiler catches bytecode-generation failures that
static type analysis cannot, including the per-function register limit.

Release bundles contain the matching compiler. Source installs can acquire it
without trusting an unpacker or an unpinned latest release:

```sh
node scripts/install-luau-compiler.mjs # Node.js 18+
```

The installer downloads only the official Luau 0.729 GitHub release asset for
the selected target. It bounds the download/extraction, verifies the archive
SHA-256, extracts only `luau-compile`, verifies that executable's SHA-256, and
atomically installs it. `--target`, `--dest`, and `--archive` support release
packaging and offline verification; `--verify-manifest` performs a no-network
check that the manifest, release targets, checksum table, and bundled upstream
license still agree.

Expected layout:

```text
tools/luau/darwin-arm64/luau-compile
tools/luau/linux-x86_64/luau-compile
tools/luau/windows-x86_64/luau-compile.exe
```

Pinned official assets and digests:

| Ro Sync target | Official asset | Archive SHA-256 | Extracted compiler SHA-256 |
| --- | --- | --- | --- |
| `darwin-arm64` | `luau-macos.zip` | `1027273dd636b4a8ad1a4167f7a43d153fef8d0c13e8a8502ed488ce95d8e2d9` | `a27e6ac06e24745c9c38478df8d0abdfc71ec535af925d7ff00cd3c5e9551a0d` |
| `linux-x86_64` | `luau-ubuntu.zip` | `cadc6e5737e6186c3b6a17047ffb25ff9ccd3728f8951ba39df7d39121a0f0f6` | `463646ea8cb3f964297fde72e7624d169158ff4f1af7baee70be942f0b8f114a` |
| `windows-x86_64` | `luau-windows.zip` | `16c079e4eebe9ba5aabfd86357ae1e48ae0bfb04b7ac4be133d403da389f2e84` | `4cee61d651c8ac34f412bc408c65959a4f8dcdbe3ef06dfe8dd09c8dbb70d7be` |

The source assets are pinned under the upstream
[Luau 0.729 release](https://github.com/luau-lang/luau/releases/tag/0.729).
Luau is distributed under its upstream MIT license, reproduced in
`tools/luau/LICENSE.txt` and sourced from the same 0.729 tag.
