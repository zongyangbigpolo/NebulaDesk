#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Darwin ]]; then
  echo "The native application fixture requires macOS 26 or newer." >&2
  exit 1
fi

output="$PWD/target/seamless-fixture"
bundle="$output/NebulaSeamlessFixture.app"
status="$output/status"
source_dir="crates/nebula-agent/tests/fixtures"
mkdir -p "$bundle/Contents/MacOS" "$status"
cp "$source_dir/SeamlessFixture.plist" "$bundle/Contents/Info.plist"
plutil -insert NebulaFixtureStatusDirectory -string "$status" "$bundle/Contents/Info.plist"
swiftc -target "$(uname -m)-apple-macos26.0" "$source_dir/SeamlessFixture.swift" \
  -o "$bundle/Contents/MacOS/NebulaSeamlessFixture"
codesign --force --sign - "$bundle"
codesign --verify --strict "$bundle"
printf 'NEBULA_APP_PROBE_PATH=%s\nNEBULA_APP_PROBE_STATUS_DIR=%s\n' "$bundle" "$status"
