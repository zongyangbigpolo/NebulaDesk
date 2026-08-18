#!/usr/bin/env bash
#
# package_app.sh - build the native helpers + Flutter manager, then embed the
# session helper into the manager .app bundle as Contents/Helpers/nebula_session.
#
# Result: app/manager/build/macos/Build/Products/Debug/nebula_manager.app that can
# launch independent session viewer processes.
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

echo "==> [3/3] Embedding session helper into $APP …"
mkdir -p "$HELPERS"
cp -f "$SESSION_BIN" "$HELPERS/nebula_session"
# Ad-hoc sign the helper so it runs without a developer cert (dev only).
codesign --force --sign - "$HELPERS/nebula_session" 2>/dev/null || true

echo
echo "Done. Launch with:"
echo "  open \"$APP\""
echo
echo "VDA side (the shared Mac), run separately:"
echo "  ./build/app/vda/nebula_vda --port 7000"
