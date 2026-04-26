#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../plugin-src"
mkdir -p src
cp ../plugin/Plugin.luau src/RoSync.server.luau
wally install
rojo build plugin.project.json --output ../plugin/Plugin.rbxm
