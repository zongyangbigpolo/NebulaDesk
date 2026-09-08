#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
node scripts/desktop-sidecars.mjs --target "$target"
npm --prefix apps/desktop-ui ci
npm --prefix apps/desktop-ui run build
npm --prefix apps/desktop-host ci
npm --prefix apps/desktop-host run build -- --target "$target"
