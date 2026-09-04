#!/usr/bin/env bash
#
# Publish an enrolled machine's desktop and grant it to the signed-in owner.
#
#     scripts/publish-desktop.sh my-mac [resource name]
#
# Uses the token scripts/dev-stack.sh left in $NEBULA_STATE.

set -euo pipefail

STATE="${NEBULA_STATE:-/tmp/nebula-dev}"
HOST="${NEBULA_HOST:-127.0.0.1}"
MANAGER_URL="${NEBULA_MANAGER_URL:-http://${HOST}:${NEBULA_MANAGER_PORT:-8080}}"

machine_name="${1:-}"
resource_name="${2:-${machine_name} Desktop}"
if [[ -z "${machine_name}" ]]; then
  echo "usage: $0 <machine-name> [resource-name]" >&2
  exit 1
fi

token="$(cat "${STATE}/token")"
api() { curl -s -H "Authorization: Bearer ${token}" -H 'content-type: application/json' "$@"; }

machine="$(api "${MANAGER_URL}/v1/machines" |
  MACHINE="${machine_name}" python3 -c '
import json, os, sys
name = os.environ["MACHINE"]
machines = json.load(sys.stdin)
machines = machines.get("items", machines) if isinstance(machines, dict) else machines
for m in machines:
    if m["name"] == name:
        print(m["id"])
        break
else:
    sys.exit(f"no machine named {name} is enrolled")
')"

resource="$(api -X POST "${MANAGER_URL}/v1/machines/${machine}/resources" \
  -d "$(printf '{"kind":"DESKTOP","name":"%s","description":"the whole screen"}' "${resource_name}")" |
  python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')"

me="$(api "${MANAGER_URL}/v1/auth/me" | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')"

api -X POST "${MANAGER_URL}/v1/resources/${resource}/entitlements" \
  -d "$(printf '{"subject_kind":"USER","subject_id":"%s","role":"CONTROLLER","allow_clipboard":true,"allow_audio":true}' "${me}")" \
  >/dev/null

echo "published '${resource_name}' (${resource}) on ${machine_name}"
