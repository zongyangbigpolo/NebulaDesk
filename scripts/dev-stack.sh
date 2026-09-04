#!/usr/bin/env bash
#
# Bring up a whole NebulaDesk deployment on this machine: manager, relay and
# gateway, plus a tenant and an owner to sign in as.
#
# Everything binds to $NEBULA_HOST, which defaults to the loopback address.
# Set it to this machine's LAN address when a second machine has to reach it:
#
#     NEBULA_HOST=192.168.1.20 scripts/dev-stack.sh
#
# Logs, secrets and pids land in $NEBULA_STATE (default /tmp/nebula-dev).
# Run scripts/dev-stack.sh stop to take it all down again.

set -euo pipefail

HOST="${NEBULA_HOST:-127.0.0.1}"
STATE="${NEBULA_STATE:-/tmp/nebula-dev}"
DB="${NEBULA_DEV_DATABASE:-nebula_dev}"
MANAGER_PORT="${NEBULA_MANAGER_PORT:-8080}"
GATEWAY_PORT="${NEBULA_GATEWAY_PORT:-7443}"
RELAY_PORT="${NEBULA_RELAY_PORT:-7444}"
BOOTSTRAP="${NEBULA_BOOTSTRAP_TOKEN:-devbootstrap}"
TENANT="${NEBULA_TENANT:-acme}"
EMAIL="${NEBULA_EMAIL:-me@acme.test}"
PASSWORD="${NEBULA_PASSWORD:-correct horse battery staple}"
MANAGER_URL="http://${HOST}:${MANAGER_PORT}"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${root}/target/debug"

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

command -v createdb >/dev/null || {
  echo "postgres client tools are needed; install postgresql" >&2
  exit 1
}

mkdir -p "${STATE}"
cargo build --manifest-path "${root}/Cargo.toml" --workspace --bins

createdb "${DB}" 2>/dev/null || true

# Waits for a service to answer, rather than sleeping and hoping.
wait_for() {
  local what="$1" check="$2" tries=60
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

NEBULA_DATABASE_URL="postgres:///${DB}" \
NEBULA_ACCESS_TOKEN_SECRET="dev-access-token-secret-0123456789abcdef" \
NEBULA_BOOTSTRAP_TOKEN="${BOOTSTRAP}" \
NEBULA_LISTEN="${HOST}:${MANAGER_PORT}" \
NEBULA_PUBLIC_URL="${MANAGER_URL}" \
  "${bin}/nebula-manager" >"${STATE}/manager.log" 2>&1 &
echo $! >>"${STATE}/pids"
wait_for manager "curl -sf ${MANAGER_URL}/health"

# One pairing secret, shared by the relay and every gateway that splices
# through it. Reused across restarts so a running agent keeps working.
if [[ ! -f "${STATE}/pair.secret" ]]; then
  "${bin}/nebula-relay" gen-secret >"${STATE}/pair.secret"
fi
PAIR="$(cat "${STATE}/pair.secret")"

NEBULA_PAIR_SECRET="${PAIR}" "${bin}/nebula-relay" \
  --listen "${HOST}:${RELAY_PORT}" \
  --advertise "${HOST}:${RELAY_PORT}" \
  --manager-url "${MANAGER_URL}" \
  --bootstrap-secret "${BOOTSTRAP}" \
  --region local >"${STATE}/relay.log" 2>&1 &
echo $! >>"${STATE}/pids"
wait_for relay "grep -q 'relay listening' ${STATE}/relay.log"

NEBULA_PAIR_SECRET="${PAIR}" "${bin}/nebula-gateway" \
  --listen "${HOST}:${GATEWAY_PORT}" \
  --advertise "${HOST}:${GATEWAY_PORT}" \
  --manager-url "${MANAGER_URL}" \
  --bootstrap-secret "${BOOTSTRAP}" \
  --region local >"${STATE}/gateway.log" 2>&1 &
echo $! >>"${STATE}/pids"
wait_for gateway "grep -q 'gateway listening' ${STATE}/gateway.log"

# Idempotent: a second run against the same database keeps the tenant it made
# the first time, so restarting the stack does not invalidate an enrolment.
curl -s -X POST "${MANAGER_URL}/v1/tenants" \
  -H "Authorization: Bearer ${BOOTSTRAP}" \
  -H 'content-type: application/json' \
  -d "$(printf '{"name":"Acme","slug":"%s","owner_email":"%s","owner_password":"%s","owner_display_name":"Owner"}' \
    "${TENANT}" "${EMAIL}" "${PASSWORD}")" >/dev/null || true

TOKEN="$(curl -s -X POST "${MANAGER_URL}/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "$(printf '{"tenant":"%s","email":"%s","password":"%s"}' "${TENANT}" "${EMAIL}" "${PASSWORD}")" |
  python3 -c 'import sys,json; print(json.load(sys.stdin)["access_token"])')"
printf '%s' "${TOKEN}" >"${STATE}/token"

cat <<EOF

  manager   ${MANAGER_URL}
  gateway   ${HOST}:${GATEWAY_PORT}
  relay     ${HOST}:${RELAY_PORT}
  logs      ${STATE}

  Enrol a machine (run this on the machine being shared):

    MACHINE=my-mac
    TOKEN=\$(curl -s -X POST ${MANAGER_URL}/v1/machines/enrollment-tokens \\
      -H "Authorization: Bearer \$(cat ${STATE}/token)" \\
      -H 'content-type: application/json' \\
      -d "{\"machine_name\":\"\${MACHINE}\",\"region\":\"local\"}" |
      python3 -c 'import sys,json; print(json.load(sys.stdin)["token"])')
    nebula-agent enroll --manager-url ${MANAGER_URL} --token "\${TOKEN}" \\
      --name "\${MACHINE}" --state ~/.nebula-agent
    nebula-agent run --state ~/.nebula-agent

  Then publish its desktop and grant yourself access:

    scripts/publish-desktop.sh <machine-name>

  And connect:

    NEBULA_PASSWORD='${PASSWORD}' nebula-client \\
      --manager-url ${MANAGER_URL} --tenant ${TENANT} --email ${EMAIL} \\
      connect '<resource name>'

  Stop everything with: scripts/dev-stack.sh stop

EOF
