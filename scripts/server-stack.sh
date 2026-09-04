#!/usr/bin/env bash
#
# Run the control plane on a server that already has the binaries built on it.
#
# This is dev-stack.sh with the parts that only make sense on a laptop taken
# out: no cargo build, no createdb, and a database URL with a password in it
# because the server's postgres does not trust the unix user.
#
#     NEBULA_HOST=47.103.58.159 scripts/server-stack.sh
#
# Run with "stop" to take it down again.

set -euo pipefail

HOST="${NEBULA_HOST:-127.0.0.1}"
STATE="${NEBULA_STATE:-/opt/nebula/state}"
BIN="${NEBULA_BIN:-/opt/nebula/bin}"
DATABASE_URL="${NEBULA_DATABASE_URL:-postgres://nebula:nebuladev@127.0.0.1/nebula}"
MANAGER_PORT="${NEBULA_MANAGER_PORT:-8080}"
GATEWAY_PORT="${NEBULA_GATEWAY_PORT:-7443}"
RELAY_PORT="${NEBULA_RELAY_PORT:-7444}"
BOOTSTRAP="${NEBULA_BOOTSTRAP_TOKEN:-devbootstrap}"
TENANT="${NEBULA_TENANT:-acme}"
EMAIL="${NEBULA_EMAIL:-me@acme.test}"
PASSWORD="${NEBULA_PASSWORD:-devpassword1234}"
MANAGER_URL="http://${HOST}:${MANAGER_PORT}"

stop() {
  if [[ -f "${STATE}/pids" ]]; then
    while read -r pid; do
      [[ -n "${pid}" ]] && kill "${pid}" 2>/dev/null || true
    done <"${STATE}/pids"
    rm -f "${STATE}/pids"
  fi
  echo "stopped"
}

if [[ "${1:-start}" == "stop" ]]; then
  stop
  exit 0
fi

mkdir -p "${STATE}"

wait_for() {
  local what="$1" check="$2" tries=120
  until eval "${check}" >/dev/null 2>&1; do
    tries=$((tries - 1))
    if [[ ${tries} -le 0 ]]; then
      echo "${what} did not come up; see ${STATE}/${what}.log" >&2
      tail -20 "${STATE}/${what}.log" >&2 || true
      exit 1
    fi
    sleep 0.5
  done
}

: >"${STATE}/pids"

NEBULA_DATABASE_URL="${DATABASE_URL}" \
NEBULA_ACCESS_TOKEN_SECRET="dev-access-token-secret-0123456789abcdef" \
NEBULA_BOOTSTRAP_TOKEN="${BOOTSTRAP}" \
NEBULA_LISTEN="0.0.0.0:${MANAGER_PORT}" \
NEBULA_PUBLIC_URL="${MANAGER_URL}" \
  "${BIN}/nebula-manager" >"${STATE}/manager.log" 2>&1 &
echo $! >>"${STATE}/pids"
wait_for manager "curl -sf http://127.0.0.1:${MANAGER_PORT}/health"

if [[ ! -f "${STATE}/pair.secret" ]]; then
  "${BIN}/nebula-relay" gen-secret >"${STATE}/pair.secret"
fi
PAIR="$(cat "${STATE}/pair.secret")"

NEBULA_PAIR_SECRET="${PAIR}" "${BIN}/nebula-relay" \
  --listen "0.0.0.0:${RELAY_PORT}" \
  --advertise "${HOST}:${RELAY_PORT}" \
  --manager-url "http://127.0.0.1:${MANAGER_PORT}" \
  --bootstrap-secret "${BOOTSTRAP}" \
  --region local >"${STATE}/relay.log" 2>&1 &
echo $! >>"${STATE}/pids"
wait_for relay "grep -q 'relay listening' ${STATE}/relay.log"

NEBULA_PAIR_SECRET="${PAIR}" "${BIN}/nebula-gateway" \
  --listen "0.0.0.0:${GATEWAY_PORT}" \
  --advertise "${HOST}:${GATEWAY_PORT}" \
  --manager-url "http://127.0.0.1:${MANAGER_PORT}" \
  --ticket-issuer "${MANAGER_URL}" \
  --bootstrap-secret "${BOOTSTRAP}" \
  --region local >"${STATE}/gateway.log" 2>&1 &
echo $! >>"${STATE}/pids"
wait_for gateway "grep -q 'gateway listening' ${STATE}/gateway.log"

curl -s -X POST "http://127.0.0.1:${MANAGER_PORT}/v1/tenants" \
  -H "Authorization: Bearer ${BOOTSTRAP}" \
  -H 'content-type: application/json' \
  -d "$(printf '{"name":"Acme","slug":"%s","owner_email":"%s","owner_password":"%s","owner_display_name":"Owner"}' \
    "${TENANT}" "${EMAIL}" "${PASSWORD}")" >/dev/null || true

TOKEN="$(curl -s -X POST "http://127.0.0.1:${MANAGER_PORT}/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "$(printf '{"tenant":"%s","email":"%s","password":"%s"}' "${TENANT}" "${EMAIL}" "${PASSWORD}")" |
  python3 -c 'import sys,json; print(json.load(sys.stdin)["access_token"])')"
printf '%s' "${TOKEN}" >"${STATE}/token"

cat <<EOF

  manager   ${MANAGER_URL}
  gateway   ${HOST}:${GATEWAY_PORT}
  relay     ${HOST}:${RELAY_PORT}
  logs      ${STATE}

EOF
