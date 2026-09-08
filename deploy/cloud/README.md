# Cloud control plane

Manager, Gateway and Relay run as separate systemd services. They may share one
server; only Manager connects to PostgreSQL. Clients configure the public Manager
URL, not a fixed Gateway or Relay. Session admission supplies the selected nodes
and their certificate pins.

## Addresses and private settings

Copy `deployment.env.example` to `/etc/nebula/deployment.env` and edit it.
It is a systemd environment file: plain `KEY=value`, no shell evaluation.

| Setting | Meaning |
| --- | --- |
| `NEBULA_PUBLIC_URL` | Public HTTPS management origin used by desktop apps and Agents |
| `NEBULA_LISTEN` | Manager's private HTTP listener behind the HTTPS proxy |
| `NEBULA_MANAGER_URL` | Internal Manager address used by this host's Gateway/Relay |
| `NEBULA_TICKET_ISSUER` | Must equal Manager's public URL, even when nodes use an internal URL |
| `NEBULA_GATEWAY_LISTEN` / `NEBULA_RELAY_LISTEN` | Local QUIC UDP listeners |
| `NEBULA_GATEWAY_ADVERTISE` / `NEBULA_RELAY_ADVERTISE` | Addresses remote endpoints can actually reach |
| `NEBULA_REGION` | Node placement region; use `default` for desktop self-enrollment |
| `NEBULA_ALLOW_SELF_REGISTRATION` | Explicitly enable creation of new personal/organization workspaces |

The example uses `https://47.103.58.159` on TCP 443, UDP 7443/7444, and private
HTTP 18081. It deliberately does not occupy an existing application's port 8080.
Replace the public addresses for another installation; they are not compiled into
the clients.

Create **root-owned, mode 0600** files outside the repository:

- `/etc/nebula/manager.secrets.env`: `NEBULA_DATABASE_URL`,
  `NEBULA_ACCESS_TOKEN_SECRET` (at least 32 random characters),
  `NEBULA_BOOTSTRAP_TOKEN`.
- `/etc/nebula/infrastructure.secrets.env`: `NEBULA_BOOTSTRAP_SECRET`
  (same operator token), `NEBULA_PAIR_SECRET` (32 random bytes encoded as hex,
  shared by Gateway and Relay).

Use fresh random secrets, not the defaults from the old test-stack scripts.
Infrastructure services do not receive the database or access-token signing
credentials. PostgreSQL must be persistent, backed up, and reachable only on the
private network/loopback. Use a dedicated database and database role rather than
another application's database.

## Upgrade existing accounts first

Before replacing a running Manager, take a restorable database backup and
quiesce user-directory writes. Run:

```sh
psql "$NEBULA_DATABASE_URL" -X -f scripts/check-registration-upgrade.sql
```

Migration 0003 enforces uniqueness of trimmed, case-insensitive email addresses
within a workspace. Older versions allowed conflicting addresses. The preflight
reports the affected tenant/user IDs and stops without changing them. Resolve
each conflict with its account owners before upgrading; retain IDs, passwords,
memberships, devices and grants. The existing user-edit API does not rename
emails, so any targeted SQL correction needs explicit operator approval.
Do not merge or delete accounts automatically. A failed migration leaves the
transaction rolled back, but the new Manager cannot start until the conflict is
resolved. Back up again after the approved correction and rerun the preflight.

## Install services

Build the three native Linux server programs from the same source revision:

```sh
cargo build --locked --release -p nebula-manager -p nebula-gateway -p nebula-relay
```

On a small shared host, build with `--jobs 1` and, if LLVM exceeds available
memory, set `CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`.
This keeps optimized release code while avoiding whole-program link-time
optimization. Do not stop unrelated applications to make room for the compiler.

Install binaries under a versioned directory and point `/opt/nebula/current` to
it; its `bin/` directory must contain the three executables. Create a non-login
`nebula` system user. Give it private writable directories
`/var/lib/nebula/gateway` and `/var/lib/nebula/relay`, plus
`/var/lib/nebula/nginx/body` and `/var/lib/nebula/nginx/proxy`.

Provision stable QUIC PEM certificates and keys at the configured node paths,
owned by `nebula` with keys mode 0600. These are authenticated by pins distributed
through Manager; do not regenerate them on every restart. Provision a trusted
HTTPS certificate separately at the paths in `nginx.conf.template`.

```sh
python3 scripts/render-cloud-config.py /etc/nebula/deployment.env \
  --output /etc/nebula/nginx.conf
install -m 644 deploy/cloud/*.service deploy/cloud/*.timer /etc/systemd/system/
systemctl daemon-reload
systemd-analyze verify /etc/systemd/system/nebula-*.service
systemctl enable --now nebula-manager nebula-web nebula-relay nebula-gateway
```

The proxy has its own configuration, PID and service; it does not load or replace
other nginx sites. Ensure the selected ports are free before starting it.
`render-cloud-config.py` is a single-host template renderer; distributed nodes
can use separate copies of `deployment.env`, with a reachable internal Manager
URL and authenticated private networking/HTTPS between hosts.

## HTTPS and renewal

IP-address certificates from Let's Encrypt require its `shortlived` profile.
The supplied renewal service expects Certbot 5.4+ in `/opt/nebula/certbot`,
with config/work/log directories `/opt/nebula/tls`, `/opt/nebula/tls-work`,
`/opt/nebula/tls-logs`, and a certificate named `nebula-manager`.

For this IP deployment, issue the certificate with Certbot's `standalone`
authenticator and `--preferred-profile shortlived --ip-address 47.103.58.159`.
Public TCP port 80 must remain available for its renewal challenges. If another
web server needs port 80, switch to a coordinated webroot challenge instead;
do not stop an unrelated service to renew certificates.

```sh
systemctl enable --now nebula-tls-renew.timer
/opt/nebula/certbot/bin/certbot renew --cert-name nebula-manager --dry-run \
  --config-dir /opt/nebula/tls --work-dir /opt/nebula/tls-work \
  --logs-dir /opt/nebula/tls-logs
systemctl list-timers nebula-tls-renew.timer
```

The timer checks twice daily and reloads only `nebula-web` after renewal.
Monitor failed renewals: an IP certificate lasts about six days. Never use
`curl -k`, disabled certificate validation, or public plaintext HTTP for login.

## Registration and verification

Self-registration defaults to off. Enable it deliberately in the deployment
file after HTTPS and abuse controls are configured, then restart Manager.
The proxy rate-limits login, signup and invitation redemption per source address;
shared NAT users share that allowance. Retain equivalent controls if replacing
the proxy or allowing other paths to Manager. This is not an email-verification
or CAPTCHA service.

Registration creates a new personal or organization workspace and its owner.
Joining an existing organization requires an email-bound, single-use invitation
from its administrator, valid for 48 hours. Invitations are manually delivered;
there is no automatic invitation email, email ownership verification, password
recovery, or cross-workspace global account in this version.

```sh
curl --fail https://47.103.58.159/health
curl --fail https://47.103.58.159/v1/auth/registration
systemctl --no-pager status nebula-manager nebula-gateway nebula-relay nebula-web
journalctl -u nebula-manager -u nebula-gateway -u nebula-relay --since '5 minutes ago'
```

Verify a complete desktop connection from a real client before retiring an old
deployment. Moving a database does not change stored Agent Manager URLs:
reconfigure/re-enroll those endpoints deliberately. Old tickets and active
connections are not immediately recalled by grant changes; stop sharing when
an immediate cutoff is required.
