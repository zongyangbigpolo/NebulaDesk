#!/usr/bin/env bash
#
# build_all.sh - one-shot build of VDA, CWA (session + Flutter manager) and the
# relay, packaged into ./dist for two-Mac testing.
#
# Output layout:
#   dist/
#     vda-mac/   nebula_vda.app           <- copy to the SHARED Mac (被控端)
#                run-vda.sh
#                README.txt
#     cwa-mac/   nebula_manager.app       <- copy to the VIEWER Mac (查看端)
#                nebula_session.app       (also embedded inside the manager)
#                run-session.sh
#                README.txt
#     relay/     nebula_relay             <- run anywhere reachable (or a Linux VPS)
#                certs/
#                run-relay.sh
#                README.txt
#
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DIST="$ROOT/dist"
BUNDLE_ID_PREFIX="com.nebula"

echo "==> [1/5] Building native targets (Release)…"
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release >/dev/null
cmake --build build

echo "==> [2/5] Building Flutter manager…"
HAVE_FLUTTER=0
FLUTTER_BIN="${FLUTTER_BIN:-}"
if [[ -z "$FLUTTER_BIN" ]]; then
  if [[ -x /opt/homebrew/bin/flutter ]]; then
    FLUTTER_BIN=/opt/homebrew/bin/flutter
  elif [[ -x /usr/local/bin/flutter ]]; then
    FLUTTER_BIN=/usr/local/bin/flutter
  elif command -v flutter >/dev/null 2>&1; then
    FLUTTER_BIN="$(command -v flutter)"
  fi
fi
if [[ -n "$FLUTTER_BIN" ]]; then
  echo "    using Flutter: $FLUTTER_BIN"
  ( cd app/manager && "$FLUTTER_BIN" build macos --release ) && HAVE_FLUTTER=1 || \
    echo "    (flutter build failed; will package CLI session only)"
else
  echo "    (flutter not found; skipping manager app)"
fi

# ---- helper: wrap a CLI binary into a minimal .app bundle ------------------
# make_app <binaryPath> <AppName> <bundleId> <destDir>
make_app() {
  local bin="$1" name="$2" bid="$3" dest="$4"
  local app="$dest/$name.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS"
  cp "$bin" "$app/Contents/MacOS/$name"
  cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>$name</string>
  <key>CFBundleIdentifier</key><string>$bid</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>26.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
  # Stable ad-hoc signature so TCC (screen-recording / accessibility) grants
  # persist across runs as long as the bundle id is unchanged.
  codesign --force --sign - --identifier "$bid" "$app" >/dev/null 2>&1 || true
  echo "$app"
}

echo "==> [3/5] Packaging VDA app…"
mkdir -p "$DIST/vda-mac"
make_app "build/app/vda/nebula_vda" "nebula_vda" "$BUNDLE_ID_PREFIX.vda" "$DIST/vda-mac" >/dev/null
cat > "$DIST/vda-mac/run-vda.sh" <<'SH'
#!/usr/bin/env bash
# Run the VDA (被控端). Grant Screen Recording + Accessibility on first launch,
# then re-run. Direct mode listens on --port; relay mode dials a relay.
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/nebula_vda.app/Contents/MacOS/nebula_vda"
# Direct:   ./run-vda.sh --port 7000
# Via relay:./run-vda.sh --relay <relayIP> --relay-port 7100 --device mymac --token secret123
exec "$BIN" "${@:---port 7000}"
SH
chmod +x "$DIST/vda-mac/run-vda.sh"
cat > "$DIST/vda-mac/README.txt" <<'TXT'
Nebula VDA (被控端 / the shared Mac)
======================================
1. Copy this whole folder to the Mac you want to share.
2. First run will prompt for permissions; grant BOTH:
     System Settings → Privacy & Security → Screen Recording  → enable nebula_vda
     System Settings → Privacy & Security → Accessibility     → enable nebula_vda
   (Accessibility is needed for remote mouse/keyboard control.)
3. Direct mode (same LAN):
     ./run-vda.sh --port 7000
   Relay mode (across networks):
     ./run-vda.sh --relay <relayIP> --relay-port 7100 --device mymac --token secret123
4. Note the VDA Mac's LAN IP (System Settings → Network) for the viewer.
TXT

echo "==> [4/5] Packaging CWA (session + manager)…"
mkdir -p "$DIST/cwa-mac"
make_app "build/app/session/nebula_session" "nebula_session" "$BUNDLE_ID_PREFIX.session" "$DIST/cwa-mac" >/dev/null
if [[ "$HAVE_FLUTTER" == "1" ]]; then
  MGR_SRC="app/manager/build/macos/Build/Products/Release/nebula_manager.app"
  if [[ -d "$MGR_SRC" ]]; then
    rm -rf "$DIST/cwa-mac/nebula_manager.app"
    cp -R "$MGR_SRC" "$DIST/cwa-mac/nebula_manager.app"
    # Embed the session helper so the manager can launch it.
    mkdir -p "$DIST/cwa-mac/nebula_manager.app/Contents/Helpers"
    cp "build/app/session/nebula_session" "$DIST/cwa-mac/nebula_manager.app/Contents/Helpers/nebula_session"
    codesign --force --sign - "$DIST/cwa-mac/nebula_manager.app/Contents/Helpers/nebula_session" >/dev/null 2>&1 || true
  fi
fi
cat > "$DIST/cwa-mac/run-session.sh" <<'SH'
#!/usr/bin/env bash
# Run a single session viewer directly (without the manager UI).
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="$HERE/nebula_session.app/Contents/MacOS/nebula_session"
# Direct:    ./run-session.sh --host <VDA_LAN_IP> --port 7000
# Via relay: NEBULA_PSK=secret ./run-session.sh --relay <relayIP> --relay-port 7100 --device mymac --token secret123
exec "$BIN" "$@"
SH
chmod +x "$DIST/cwa-mac/run-session.sh"
cat > "$DIST/cwa-mac/README.txt" <<'TXT'
Nebula CWA (查看端 / the viewer Mac)
======================================
Option A — GUI (recommended): open nebula_manager.app, click "+", add the VDA
  (name / host = VDA's LAN IP / port 7000 / optional relay), then Connect.
  Each connection opens an independent high-refresh session window.

Option B — CLI single session:
  Direct:    ./run-session.sh --host <VDA_LAN_IP> --port 7000
  Via relay: NEBULA_PSK=secret ./run-session.sh --relay <relayIP> --relay-port 7100 \
                 --device mymac --token secret123

Tip: if a Gatekeeper warning appears, right-click the .app → Open the first time,
or run:  xattr -dr com.apple.quarantine nebula_manager.app nebula_session.app
TXT

echo "==> [5/5] Packaging relay…"
mkdir -p "$DIST/relay/certs"
cp "build/server/nebula_relay/nebula_relay" "$DIST/relay/nebula_relay"
cp build/server/nebula_relay/certs/*.pem "$DIST/relay/certs/" 2>/dev/null || true
cat > "$DIST/relay/run-relay.sh" <<'SH'
#!/usr/bin/env bash
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
exec "$HERE/nebula_relay" --port "${1:-7100}" \
  --cert "$HERE/certs/relay_cert.pem" --key "$HERE/certs/relay_key.pem"
SH
chmod +x "$DIST/relay/run-relay.sh"
cat > "$DIST/relay/README.txt" <<'TXT'
Nebula relay (optional — only for cross-network / NAT traversal)
==================================================================
Run on any host both peers can reach (a VPS, or a Mac on the same LAN):
  ./run-relay.sh 7100
QUIC is UDP — open UDP 7100 in the firewall AND the cloud security group.
For Linux production deployment (Docker/systemd) see server/nebula_relay/DEPLOY.md.
TXT

echo
echo "================ DONE ================"
echo "Artifacts in: $DIST"
echo "  vda-mac/   -> copy to the SHARED Mac, run ./run-vda.sh --port 7000"
echo "  cwa-mac/   -> copy to the VIEWER Mac, open nebula_manager.app"
echo "  relay/     -> optional, run ./run-relay.sh 7100"
echo
echo "Quick direct test (two Macs, same LAN):"
echo "  VDA Mac:   cd vda-mac && ./run-vda.sh --port 7000   (grant permissions)"
echo "  CWA Mac:   cd cwa-mac && ./run-session.sh --host <VDA_LAN_IP> --port 7000"
