#!/usr/bin/env bash
#
# install_linux.sh - build and install nebula_relay as a systemd service on a
# Debian/Ubuntu server. Run as root (sudo).
#
#   sudo ./install_linux.sh [--port 7100]
#
set -euo pipefail

PORT=7100
while [[ $# -gt 0 ]]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    *) echo "unknown arg: $1"; exit 1 ;;
  esac
done

if [[ $EUID -ne 0 ]]; then echo "Please run as root (sudo)."; exit 1; fi

HERE="$(cd "$(dirname "$0")" && pwd)"

echo "==> [1/6] Installing build deps + libmsquic…"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends ca-certificates curl gnupg cmake g++ ninja-build openssl libcurl4-openssl-dev
# Microsoft package repo for libmsquic (auto-detect Ubuntu version).
. /etc/os-release
curl -sSL https://packages.microsoft.com/keys/microsoft.asc \
  | gpg --dearmor -o /usr/share/keyrings/microsoft.gpg
echo "deb [signed-by=/usr/share/keyrings/microsoft.gpg] https://packages.microsoft.com/ubuntu/${VERSION_ID}/prod ${VERSION_CODENAME} main" \
  > /etc/apt/sources.list.d/microsoft.list
apt-get update
apt-get install -y --no-install-recommends libmsquic

echo "==> [2/6] Building nebula_relay…"
# Use the standalone CMakeLists (self-contained, only needs RelayProtocol.h).
BUILD_SRC="$HERE"
if [[ -f "$HERE/CMakeLists.standalone.txt" ]]; then
  cp -f "$HERE/CMakeLists.standalone.txt" "$HERE/CMakeLists.deploy.txt"
  # CMake requires the file be named CMakeLists.txt in the source dir; build in
  # a temp dir that symlinks the standalone config.
  rm -rf "$HERE/.deploybuild" && mkdir -p "$HERE/.deploybuild/src"
  cp "$HERE/main.cpp" "$HERE/.deploybuild/src/main.cpp"
  cp "$HERE/CMakeLists.standalone.txt" "$HERE/.deploybuild/src/CMakeLists.txt"
  mkdir -p "$HERE/.deploybuild/src/core/inc"
  # RelayProtocol.h lives two levels up in a full checkout; copy it in.
  if [[ -f "$HERE/../../core/inc/RelayProtocol.h" ]]; then
    cp "$HERE/../../core/inc/RelayProtocol.h" "$HERE/.deploybuild/src/RelayProtocol.h"
  fi
  cmake -S "$HERE/.deploybuild/src" -B "$HERE/.deploybuild/build" -G Ninja -DCMAKE_BUILD_TYPE=Release
  cmake --build "$HERE/.deploybuild/build"
  BUILT_BIN="$HERE/.deploybuild/build/nebula_relay"
else
  cmake -S "$HERE" -B "$HERE/build" -G Ninja -DCMAKE_BUILD_TYPE=Release
  cmake --build "$HERE/build"
  BUILT_BIN="$HERE/build/nebula_relay"
fi

echo "==> [3/6] Installing binary to /usr/local/bin…"
install -m 0755 "$BUILT_BIN" /usr/local/bin/nebula_relay

echo "==> [4/6] Provisioning certificate in /etc/nebula-relay…"
mkdir -p /etc/nebula-relay
if [[ ! -f /etc/nebula-relay/relay_cert.pem ]]; then
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout /etc/nebula-relay/relay_key.pem \
    -out    /etc/nebula-relay/relay_cert.pem \
    -days 3650 -subj "/CN=nebula-relay"
  echo "    generated self-signed cert (replace with a real one for production)"
fi

echo "==> [5/6] Creating service user + systemd unit…"
id -u nebula-relay >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin nebula-relay
chown -R nebula-relay:nebula-relay /etc/nebula-relay
sed "s/--port 7100/--port ${PORT}/" "$HERE/nebula-relay.service" > /etc/systemd/system/nebula-relay.service
systemctl daemon-reload
systemctl enable --now nebula-relay

echo "==> [6/6] Opening firewall (if ufw present)…"
if command -v ufw >/dev/null 2>&1; then ufw allow "${PORT}/udp" || true; fi

echo
echo "Done. Relay listening on UDP ${PORT}."
echo "  status:  systemctl status nebula-relay"
echo "  logs:    journalctl -u nebula-relay -f"
echo
echo "IMPORTANT: open UDP ${PORT} in your cloud provider's security group too."
