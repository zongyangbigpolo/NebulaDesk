#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
node scripts/desktop-sidecars.mjs --target "$target"
npm --prefix apps/desktop-ui ci
npm --prefix apps/desktop-ui run build
cargo build -p nebula-desktop --features gui --release --target "$target"
echo "Bundle: install the Tauri 2 CLI, then (cd crates/nebula-desktop && cargo tauri build --target $target)"
