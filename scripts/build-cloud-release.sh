#!/usr/bin/env bash
# Cross-compile the cloud services on the workstation; package no credentials.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
if [[ -n "$(git status --porcelain)" ]]; then
  echo "Commit or set aside working-tree changes before making a versioned release." >&2
  exit 1
fi

TARGET=x86_64-unknown-linux-gnu
GLIBC=2.28
COMMIT="$(git rev-parse HEAD)"
VERSION="$(git rev-parse --short=12 HEAD)"
OUTPUT="$ROOT/target/cloud-releases"
STAGE="$OUTPUT/$VERSION-linux-x86_64"
ARCHIVE="$STAGE.tar.gz"
if [[ -e "$STAGE" || -e "$ARCHIVE" ]]; then
  echo "Release already exists: $STAGE. Retain it or explicitly move it before rebuilding." >&2
  exit 1
fi

cargo zigbuild --locked --release --target "$TARGET.$GLIBC" \
  -p nebula-manager -p nebula-gateway -p nebula-relay --jobs "${NEBULA_BUILD_JOBS:-2}"

BIN_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}/$TARGET/release"
mkdir -p "$STAGE/bin"
for program in nebula-manager nebula-gateway nebula-relay; do
  install -m 755 "$BIN_ROOT/$program" "$STAGE/bin/$program"
done
{
  printf 'source_commit=%s\ntarget=%s\nglibc_baseline=%s\n' "$COMMIT" "$TARGET" "$GLIBC"
  rustc --version
  cargo zigbuild --version
  zig version
} > "$STAGE/build-info.txt"
(
  cd "$STAGE"
  shasum -a 256 bin/nebula-manager bin/nebula-gateway bin/nebula-relay build-info.txt > SHA256SUMS
)
COPYFILE_DISABLE=1 tar -czf "$ARCHIVE" -C "$STAGE" bin build-info.txt SHA256SUMS
printf 'Linux release: %s\n' "$ARCHIVE"
