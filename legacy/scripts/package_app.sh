#!/usr/bin/env bash
#
# package_app.sh - build the native helpers + Flutter manager, then embed both
# the session (CWA viewer) and vda (host) helpers into the manager .app bundle
# as Contents/Helpers/nebula_session and Contents/Helpers/nebula_vda.
#
# Result: app/manager/build/macos/Build/Products/Debug/nebula_manager.app that
# can, from the same app, launch independent CWA session viewer processes
# ("Direct"/"Cloud" tabs) AND host this Mac as a VDA ("Host this Mac" tab) —
# see README.md for the "one app, both roles" design.
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

echo "==> [1/3] Building native targets (nebula_vda, nebula_session)…"
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release >/dev/null
cmake --build build

echo "==> [2/3] Building Flutter manager…"
( cd app/manager && flutter build macos --debug )

APP="app/manager/build/macos/Build/Products/Debug/nebula_manager.app"
HELPERS="$APP/Contents/Helpers"
SESSION_BIN="build/app/session/nebula_session"
VDA_BIN="build/app/vda/nebula_vda"

echo "==> [3/3] Embedding session + vda helpers into $APP …"
mkdir -p "$HELPERS"
cp -f "$SESSION_BIN" "$HELPERS/nebula_session"
cp -f "$VDA_BIN" "$HELPERS/nebula_vda"
# Ad-hoc sign the helpers so they run without a developer cert (dev only).
codesign --force --sign - "$HELPERS/nebula_session" 2>/dev/null || true
codesign --force --sign - "$HELPERS/nebula_vda" 2>/dev/null || true

echo
echo "Done. Launch with:"
echo "  open \"$APP\""
echo
echo "The app's 'Host this Mac' tab can now register + run nebula_vda directly"
echo "(no separate binary/terminal needed). To run nebula_vda standalone instead:"
echo "  ./build/app/vda/nebula_vda --port 7000"
