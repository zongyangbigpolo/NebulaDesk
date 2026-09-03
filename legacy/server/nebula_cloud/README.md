# nebula_cloud

`nebula_cloud` is a new Node.js + TypeScript + PostgreSQL control-plane for Nebula.
It layers multi-tenant accounts, device ownership, sharing, and short-lived relay session tickets on top of today's `nebula_relay` pairing model.

## What this service does

- Users register/login with email + password.
- A user owns SaaS devices.
- Each device has a stable **relayDeviceId** and a long-lived **relay token** used only by the VDA side to register with `nebula_relay`.
- Owners can grant access to other existing users.
- Admins (bootstrapped via `INITIAL_ADMIN_EMAILS`, promoted thereafter) see and can connect to every device fleet-wide,
  and curate Groups of devices with blanket VIEWER/CONTROLLER membership for a set of users — see "Groups & admin role".
- Any logged-in user can self-register a device under their own account with a shared enrollment secret
  (no admin/claim-code step first), and every account starts with a metered trial "connect" credit that an
  admin can top up — see "Self-registration & trial credit".
- A viewer/controller asks `nebula_cloud` for a **short-lived session JWT** via `POST /devices/:id/connect`.
- `nebula_relay` calls `POST /internal/authorize` before bridging a session and gets a live yes/no answer.
- `nebula_relay` also calls `POST /internal/heartbeat` on VDA (re)registration and periodically thereafter
  (`--saas-heartbeat-url`), so `online`/`lastSeenAt` stay accurate on the classic QUIC/relay path too.
- **New (P4, see the main repo's `ROADMAP.md` §12): a real browser can watch/control a device directly**, no native
  client install required. `nebula_cloud` runs a `/ws/signaling` WebSocket endpoint that relays SDP offer/answer and
  trickle ICE candidates between a `nebula_vda --webrtc` process and the browser's `RTCPeerConnection` — pure
  message pass-through, `nebula_cloud` never touches media bytes. See "Browser (WebRTC) viewer" below.

## Out of scope

- No change to the *native-client* media path: `nebula_relay`, `nebula_vda`, and `nebula_session` still carry that
  traffic exactly as before. The WebRTC path (previous bullet) is a fully separate, opt-in path on the VDA side.
- No TURN server implementation — deploy standard `coturn` (see "Browser (WebRTC) viewer" below); this service only
  tells clients which STUN/TURN servers to use, it doesn't relay media itself either way.
- No device-side daemon yet; the relay-side heartbeat (previous section) covers online-status freshness for the QUIC path, but there's still no agent running on the VDA host itself for OS-level health/updates.

## Architecture

```text
┌───────────────┐                 ┌─────────────────────┐
│ Web dashboard │                 │ External API client │
│  (public/*)   │                 │   curl / scripts    │
└──────┬────────┘                 └──────────┬──────────┘
       │ HTTPS / JSON / static pages                    │
       └──────────────────────┬─────────────────────────┘
                              ▼
                    ┌────────────────────┐
                    │    nebula_cloud    │
                    │ Fastify + services │
                    │ Auth / Devices /   │
                    │ Grants / Tickets / │
                    │ WS signaling relay │
                    └───────┬────────────┘
                            │ Prisma ORM
                            ▼
                    ┌────────────────────┐
                    │     PostgreSQL     │
                    │ Users / Devices /  │
                    │ Grants / Audits /  │
                    │ Refresh tokens     │
                    └───────┬────────────┘
                            │ live policy callback
                            ▼
                    ┌────────────────────┐
                    │    nebula_relay    │
                    │  blind forwarding  │
                    │   + HTTP authz     │
                    └───────┬────────────┘
                            │ QUIC pairing
              ┌─────────────┴─────────────┐
              ▼                           ▼
        nebula_vda                  nebula_session
  --device relayDeviceId      --device relayDeviceId
  --token long-lived relay    --token short-lived JWT

  In parallel, fully independent of the QUIC path above:

        nebula_vda --webrtc  ───/ws/signaling (WS, JSON)───►  browser (public/watch.html)
        (libdatachannel: ICE/DTLS/SRTP)                        RTCPeerConnection
                    │                                                 │
                    └──────────────── STUN/TURN ─────────────────────┘
                                (direct media, nebula_cloud never sees it)
```

## Data model

### User
- `id` UUID
- `email` unique
- `passwordHash` bcryptjs hash
- `displayName`
- `role` enum: `USER | ADMIN` (default `USER`) — see "Groups & admin role" below
- `creditSeconds` remaining "connect" credit, in seconds (default `DEFAULT_TRIAL_CREDIT_SECONDS`) —
  see "Self-registration & trial credit" below
- `createdAt`

### Device
- `id` UUID (SaaS device id)
- `ownerUserId` FK -> User
- `name`
- `relayDeviceId` unique stable string configured into `nebula_vda`
- `relayTokenHash` SHA-256 hash of the long-lived relay token
- `psk` **plaintext** application-layer session-encryption secret (see "PSK distribution" below) —
  unlike `relayTokenHash` this can't be a one-way hash, since every authorized connecting viewer needs the
  actual value back
- `claimCodeHash`, `claimCodeExpiresAt` for one-time onboarding
- `groupId` FK -> Group, nullable — see "Groups & admin role" below
- `createdAt`, `lastSeenAt`

### PSK distribution

`ARCHITECTURE.md` §4a's application-layer ChaCha20-Poly1305 session encryption needs a shared secret
(`NEBULA_PSK`/`--psk`) known to both the VDA and every CWA that connects to it — independent of, and in
addition to, the relay token / session JWT used for *pairing*. `nebula_cloud` now generates one per device and
distributes it automatically instead of requiring an out-of-band copy/paste:

- Generated once at `POST /devices` time, returned in `registration.psk` (and baked into
  `registration.vdaCommand`'s `--psk` flag) — the same "shown once at creation" pattern as `relayToken`.
- Also returned (unrotated) by `POST /device-claims/redeem`, for an install script that redeems a claim code
  instead of copying the command line directly.
- Returned to every **authorized** caller of `POST /devices/:id/connect` (`psk` field, also baked into
  `sessionCommand`'s `NEBULA_PSK=`) — gated by the exact same owner/grant/group-membership/admin check that
  already decides whether the connect ticket is issued at all, so this doesn't widen who can decrypt a session
  beyond who could already open one.

> **Known trade-off**: because callers need the plaintext back (not just a verifiable hash), `psk` is stored
> as plaintext in Postgres rather than hashed. This is consistent with this project's current "self-hosted /
> pre-production-hardening" posture (see `server/nebula_relay/DEPLOY.md`'s hardening checklist) but a real
> production deployment should encrypt this column at rest (e.g. envelope encryption with a KMS-held key)
> rather than relying solely on database access controls.

### AccessGrant
- `id` UUID
- `deviceId` FK -> Device
- `granteeUserId` FK -> User
- `role` enum: `VIEWER | CONTROLLER`
- `createdAt`, `revokedAt`
- `createdByUserId`

> Owner access stays implicit through `Device.ownerUserId`.

### Group
- `id` UUID
- `name`
- `createdByUserId` FK -> User
- `createdAt`
- has many `Device` (via `Device.groupId`) and `GroupMembership`

### GroupMembership
- `id` UUID
- `groupId` FK -> Group
- `userId` FK -> User
- `role` enum: `VIEWER | CONTROLLER` — applies to every device currently in the group
- `createdAt`
- unique on `(groupId, userId)`

### RefreshToken
- `id` UUID
- `userId`
- `tokenHash`
- `expiresAt`, `revokedAt`, `createdAt`

### ConnectionAudit
- `id` UUID (also used as JWT `jti`)
- `deviceId`
- `requestedByUserId`
- `issuedTokenHash`
- `issuedAt`, `expiresAt`, `usedAt`
- `sourceIp`
- `result` enum: `ISSUED | AUTHORIZED | DENIED | EXPIRED`
- `reason`

## Groups & admin role

Per-device `AccessGrant` (above) is fine for one owner sharing a handful of their
own machines, but doesn't scale to "a fleet of machines curated centrally and
assigned to whichever users should see them" — that's what `Group` +
`GroupMembership` + the `ADMIN` user role are for:

- **Bootstrapping the first admin**: there is no admin by default. Set
  `INITIAL_ADMIN_EMAILS` (comma-separated) in the environment; whoever
  registers with one of those emails becomes `role=ADMIN` immediately. Every
  admin after that is promoted by an existing admin via
  `PATCH /admin/users/:id/role`.
- **Admins see and can connect to every device**, regardless of ownership,
  grants, or group membership — `GET /devices` reports `role: "ADMIN"` for
  these entries so the UI can tell "I can see this because I'm an admin" apart
  from actually owning/being granted it. Admins can also delete any device and
  manage any device's grants (an owner-or-admin check, same as the owner-only
  checks elsewhere).
- **A device belongs to at most one Group** (`Device.groupId`, nullable) —
  assigned/unassigned only by an admin, via `POST /admin/devices/:id/group`.
- **A user in a Group's membership gets that role (VIEWER/CONTROLLER) on every
  device currently in the group** — no per-device grant needed. If a user has
  *both* a direct `AccessGrant` and a group-membership role on the same
  device, the higher of the two wins (`CONTROLLER` > `VIEWER`).
- Like grants, **group membership is re-checked live** on every
  `/devices/:id/connect` and on the relay's `/internal/authorize` callback —
  removing someone from a group revokes their access to every device in it
  immediately, not just on their next token refresh.

Admin-only endpoints (all require the caller's access token to carry
`role: "ADMIN"`; see the REST API section below for exact payloads):

| Endpoint | Purpose |
|---|---|
| `GET /admin/users` | List every registered user and their role |
| `PATCH /admin/users/:id/role` | Promote/demote a user (admins can't demote themselves) |
| `PATCH /admin/users/:id/credit` | Top up (or claw back) a user's trial credit — see below |
| `POST /admin/groups` | Create a group |
| `GET /admin/groups` | List groups with member/device counts |
| `GET /admin/groups/:id` | Group detail: members + devices |
| `DELETE /admin/groups/:id` | Delete a group (devices are unassigned, not deleted) |
| `POST /admin/groups/:id/members` | Add/update a member's role in a group |
| `DELETE /admin/groups/:id/members/:userId` | Remove a member from a group |
| `POST /admin/devices/:id/group` | Assign (or unassign, `groupId: null`) a device to a group |

## Self-registration & trial credit

Two related features close the loop for "install the app, register your own
Mac, and immediately be able to connect to it" without any admin having to
pre-create a device row or hand out a per-device claim code — see the Flutter
manager's "Host this Mac" tab (`app/manager/lib/host_tab.dart`).

**Self-registration** (`POST /devices/self-register`): any LOGGED-IN user who
presents the shared `DEVICE_ENROLLMENT_TOKEN` (a single secret configured by
whoever runs this `nebula_cloud` instance, distributed out-of-band — e.g. in
onboarding docs) gets a device created under their OWN account immediately.
This is exactly `POST /devices` with one extra check up front; there's no
separate claim-code round trip since the caller is already authenticated.
Disabled by default (empty `DEVICE_ENROLLMENT_TOKEN` refuses every request
with `403`) — an operator opts in explicitly.

**Trial credit** (`User.creditSeconds`): every new account starts with
`DEFAULT_TRIAL_CREDIT_SECONDS` (default 600 = 10 minutes) of "connect"
credit. Each successful `POST /devices/:id/connect` deducts a flat
`CONNECT_CREDIT_COST_SECONDS` from the caller's balance — regardless of how
long the resulting session actually lasts, since metering real session
duration would need the relay/VDA to report it back to `nebula_cloud`, which
doesn't exist today (see ROADMAP.md). Once the balance can't cover the cost,
`/devices/:id/connect` returns `402 insufficient_credit` instead of issuing a
ticket. **Admins are never metered.** An admin restores/grants credit via
`PATCH /admin/users/:id/credit`. The spend is atomic at the DB layer
(`PrismaDataStore.spendUserCredit`'s conditional `UPDATE ... WHERE
creditSeconds >= cost`), so concurrent connect attempts can't both pass a
stale balance check.

> **Known trade-off**: `DEVICE_ENROLLMENT_TOKEN` is a single shared secret
> (not per-user or per-device) — anyone who has it can self-register devices
> under whichever account they're logged in as. Treat it like
> `RELAY_SHARED_SECRET`: rotate it if it leaks, and only hand it out to
> people/scripts you trust to register real devices.

## Session-token design

- Access token: HS256 JWT, default TTL 15 minutes.
- Refresh token: opaque random string, stored hashed, default TTL 30 days.
- Session token: HS256 JWT, default TTL 60 seconds.
- Session token claims:

```json
{
  "sub": "<requesting user id>",
  "deviceId": "<relayDeviceId>",
  "type": "session",
  "jti": "<ConnectionAudit.id>",
  "exp": 1234567890
}
```

The relay never needs the long-lived VDA relay token from a viewing user. Only the short-lived session JWT crosses the CWA/session side.

## Claim flow

`POST /devices` returns:
- the new SaaS device id
- a stable `relayDeviceId`
- a freshly generated long-lived relay token (shown once)
- a short-lived claim code

Optional onboarding helper: `POST /device-claims/redeem` accepts a claim code and rotates the relay token to a fresh value. This lets you hand only the claim code to an operator/helper instead of the original relay token.

## Running locally

### 1) Install dependencies

```sh
cd server/nebula_cloud
export PATH="/opt/homebrew/bin:$PATH"   # useful on Apple Silicon macOS if node/npm are not already on PATH
cp .env.example .env
npm install
```

### 2) Start PostgreSQL with Docker Compose

```sh
docker compose up -d postgres
```

### 3) Apply migrations

```sh
npm run prisma:migrate
```

### 4) Start the app

```sh
npm run dev
```

Open:
- Dashboard: <http://127.0.0.1:4000>
- Health check: <http://127.0.0.1:4000/health>

### One-command container run

```sh
docker compose up --build
```

The container entrypoint runs `prisma migrate deploy` before starting the service.

## Build and test

```sh
npm run build
npm test
```

The repo also includes an API smoke script that exercises register -> create device -> invite -> connect -> internal authorize against a running server:

```sh
node scripts/smoke-flow.mjs
```

## Environment variables

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `PORT` | yes | `4000` | HTTP listen port |
| `HOST` | yes | `0.0.0.0` | HTTP listen host |
| `DATABASE_URL` | yes | none | Prisma PostgreSQL connection string |
| `JWT_ACCESS_SECRET` | yes | none | HS256 secret for access tokens |
| `JWT_REFRESH_SECRET` | yes | none | Reserved secret namespace for refresh-token flows; opaque refresh tokens are still stored hashed |
| `JWT_SESSION_SECRET` | yes | none | HS256 secret for relay session JWTs |
| `ACCESS_TOKEN_TTL_SECONDS` | no | `900` | Access-token lifetime |
| `REFRESH_TOKEN_TTL_SECONDS` | no | `2592000` | Refresh-token lifetime |
| `SESSION_TOKEN_TTL_SECONDS` | no | `60` | Session-ticket lifetime |
| `CLAIM_CODE_TTL_SECONDS` | no | `3600` | Claim-code lifetime |
| `DEVICE_ONLINE_WINDOW_SECONDS` | no | `120` | `lastSeenAt` freshness window used to compute `online` |
| `RELAY_PUBLIC_HOST` | yes | none | Host advertised back to VDA/session clients |
| `RELAY_PUBLIC_PORT` | no | `7100` | Relay port advertised back to VDA/session clients |
| `RELAY_SHARED_SECRET` | yes | none | Shared secret expected in `X-Relay-Secret` for `/internal/authorize` and optional relay-side heartbeat |
| `INITIAL_ADMIN_EMAILS` | no | none | Comma-separated emails that get `role=ADMIN` on registration — bootstraps the first admin(s); see "Groups & admin role" |
| `DEVICE_ENROLLMENT_TOKEN` | no | none (disabled) | Shared secret enabling `POST /devices/self-register`; see "Self-registration & trial credit" |
| `DEFAULT_TRIAL_CREDIT_SECONDS` | no | `600` | Starting `creditSeconds` balance for new accounts |
| `CONNECT_CREDIT_COST_SECONDS` | no | `600` | Flat credit cost deducted per successful `/devices/:id/connect` (non-admins only) |

## Dashboard

The dashboard is intentionally simple and lives in `public/`.

It supports:
- register
- login/logout
- list owned/shared devices
- create a device and show claim/VDA registration command
- redeem a claim code
- invite an existing user by email
- revoke grants
- request a session ticket and display a copy/paste `nebula_session` command

## REST API

All JSON APIs are same-origin and accept `Authorization: Bearer <accessToken>` where noted.

### Auth

#### `POST /auth/register`

```sh
curl -X POST http://127.0.0.1:4000/auth/register \
  -H 'Content-Type: application/json' \
  -d '{"email":"owner@example.com","password":"super-secret-pass","displayName":"Owner"}'
```

Response:

```json
{
  "user": {
    "id": "uuid",
    "email": "owner@example.com",
    "displayName": "Owner",
    "createdAt": "2026-08-17T09:00:00.000Z"
  }
}
```

#### `POST /auth/login`

```sh
curl -X POST http://127.0.0.1:4000/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"owner@example.com","password":"super-secret-pass"}'
```

Response:

```json
{
  "accessToken": "...jwt...",
  "refreshToken": "...opaque...",
  "tokenType": "Bearer",
  "expiresIn": 900,
  "user": {
    "userId": "uuid",
    "email": "owner@example.com",
    "displayName": "Owner"
  }
}
```

#### `POST /auth/refresh`

```sh
curl -X POST http://127.0.0.1:4000/auth/refresh \
  -H 'Content-Type: application/json' \
  -d '{"refreshToken":"<opaque-refresh-token>"}'
```

#### `POST /auth/logout`

```sh
curl -X POST http://127.0.0.1:4000/auth/logout \
  -H 'Content-Type: application/json' \
  -d '{"refreshToken":"<opaque-refresh-token>"}'
```

#### `GET /auth/me`

```sh
curl http://127.0.0.1:4000/auth/me \
  -H 'Authorization: Bearer <accessToken>'
```

### Devices

#### `POST /devices`

Creates a device, returns relay registration material **once**.

```sh
curl -X POST http://127.0.0.1:4000/devices \
  -H 'Authorization: Bearer <accessToken>' \
  -H 'Content-Type: application/json' \
  -d '{"name":"Mac mini Lab"}'
```

Example response:

```json
{
  "device": {
    "id": "uuid",
    "name": "Mac mini Lab",
    "relayDeviceId": "nebula-7f29b8a4f1234567",
    "createdAt": "2026-08-17T09:00:00.000Z",
    "claimCodeExpiresAt": "2026-08-17T10:00:00.000Z"
  },
  "registration": {
    "relayHost": "127.0.0.1",
    "relayPort": 7100,
    "relayDeviceId": "nebula-7f29b8a4f1234567",
    "relayToken": "<long-lived relay token>",
    "psk": "<session encryption secret>",
    "claimCode": "NEB-ABCDE-FGHIJ",
    "claimCodeExpiresAt": "2026-08-17T10:00:00.000Z",
    "vdaCommand": "nebula_vda --port 7000 --psk <session encryption secret> --relay 127.0.0.1 --relay-port 7100 --device nebula-7f29b8a4f1234567 --token <long-lived relay token>"
  }
}
```

That response is exactly the bridge to the existing Nebula CLI contract:

```sh
nebula_vda --port 7000 --relay <host> --relay-port <port> --device <relayDeviceId> --token <relayToken>
```

#### `POST /devices/self-register`

Self-service registration (see "Self-registration & trial credit" above): any logged-in user presenting
`DEVICE_ENROLLMENT_TOKEN` gets a device created under their own account immediately — no admin
action or claim code needed first. Disabled (`403`) unless `DEVICE_ENROLLMENT_TOKEN` is configured.

```sh
curl -X POST http://127.0.0.1:4000/devices/self-register \
  -H 'Authorization: ******' \
  -H 'Content-Type: application/json' \
  -d '{"name":"My Mac mini","enrollmentToken":"<DEVICE_ENROLLMENT_TOKEN>"}'
```

Example response (same shape as `POST /devices`, minus the claim-code fields — there's nothing to
redeem separately since the caller already authenticated):

```json
{
  "device": {
    "id": "uuid",
    "name": "My Mac mini",
    "relayDeviceId": "nebula-a1b2c3d4e5f60708",
    "createdAt": "2026-08-24T09:00:00.000Z"
  },
  "registration": {
    "relayHost": "127.0.0.1",
    "relayPort": 7100,
    "relayDeviceId": "nebula-a1b2c3d4e5f60708",
    "relayToken": "<long-lived relay token>",
    "psk": "<session encryption secret>",
    "vdaCommand": "nebula_vda --port 7000 --psk <session encryption secret> --relay 127.0.0.1 --relay-port 7100 --device nebula-a1b2c3d4e5f60708 --token <long-lived relay token>"
  }
}
```

#### `POST /device-claims/redeem`

Optional onboarding helper. Redeems a claim code and rotates the relay token.

```sh
curl -X POST http://127.0.0.1:4000/device-claims/redeem \
  -H 'Content-Type: application/json' \
  -d '{"claimCode":"NEB-ABCDE-FGHIJ"}'
```

#### `GET /devices`

Lists owned + shared devices visible to the caller.

```sh
curl http://127.0.0.1:4000/devices \
  -H 'Authorization: Bearer <accessToken>'
```

Example item:

```json
{
  "id": "uuid",
  "name": "Mac mini Lab",
  "role": "OWNER",
  "relayDeviceId": "nebula-7f29b8a4f1234567",
  "createdAt": "2026-08-17T09:00:00.000Z",
  "lastSeenAt": null,
  "online": false
}
```

#### `DELETE /devices/:id`

Owner only.

```sh
curl -X DELETE http://127.0.0.1:4000/devices/<deviceId> \
  -H 'Authorization: Bearer <accessToken>'
```

#### `POST /devices/:id/heartbeat`

Owner-authenticated manual refresh of `lastSeenAt` (`:id` is the internal device UUID, not `relayDeviceId`):

```sh
curl -X POST http://127.0.0.1:4000/devices/<deviceId>/heartbeat \
  -H 'Authorization: ******'
```

For `nebula_relay` itself, use `POST /internal/heartbeat` instead (below) — the relay only ever
knows a device's `relayDeviceId`, never this internal UUID.

### Access grants

> `granteeEmail` must already belong to an existing account.

#### `POST /devices/:id/grants`

Owner only.

```sh
curl -X POST http://127.0.0.1:4000/devices/<deviceId>/grants \
  -H 'Authorization: Bearer <accessToken>' \
  -H 'Content-Type: application/json' \
  -d '{"granteeEmail":"viewer@example.com","role":"VIEWER"}'
```

#### `GET /devices/:id/grants`

Owner only.

```sh
curl http://127.0.0.1:4000/devices/<deviceId>/grants \
  -H 'Authorization: Bearer <accessToken>'
```

#### `DELETE /grants/:id`

Owner only. Soft-revokes the grant by setting `revokedAt`.

```sh
curl -X DELETE http://127.0.0.1:4000/grants/<grantId> \
  -H 'Authorization: Bearer <accessToken>'
```

### Admin: users and groups

> All of these require the caller's access token to carry `role: "ADMIN"` (see "Groups & admin role" above);
> otherwise they return `403`.

#### `GET /admin/users`

```sh
curl http://127.0.0.1:4000/admin/users \
  -H 'Authorization: ******'
```

#### `PATCH /admin/users/:id/role`

Promotes or demotes a user. An admin cannot demote themselves (`400`) — ask another admin.

```sh
curl -X PATCH http://127.0.0.1:4000/admin/users/<userId>/role \
  -H 'Authorization: ******' \
  -H 'Content-Type: application/json' \
  -d '{"role":"ADMIN"}'
```

#### `PATCH /admin/users/:id/credit`

Tops up (or, with a negative `addSeconds`, claws back) a user's trial "connect" credit — see
"Self-registration & trial credit" above. Balance never goes below zero.

```sh
curl -X PATCH http://127.0.0.1:4000/admin/users/<userId>/credit \
  -H 'Authorization: ******' \
  -H 'Content-Type: application/json' \
  -d '{"addSeconds":600}'
```

```json
{ "user": { "id": "uuid", "email": "trial@example.com", "creditSeconds": 1200 } }
```

#### `POST /admin/groups`

```sh
curl -X POST http://127.0.0.1:4000/admin/groups \
  -H 'Authorization: ******' \
  -H 'Content-Type: application/json' \
  -d '{"name":"Lab Macs"}'
```

#### `GET /admin/groups`

Lists every group with member/device counts.

```sh
curl http://127.0.0.1:4000/admin/groups \
  -H 'Authorization: ******'
```

#### `GET /admin/groups/:id`

Group detail: members (with their role) and the devices currently assigned to it.

```sh
curl http://127.0.0.1:4000/admin/groups/<groupId> \
  -H 'Authorization: ******'
```

#### `DELETE /admin/groups/:id`

Deletes the group. Devices previously in it are unassigned (`groupId` becomes `null`), not deleted.

```sh
curl -X DELETE http://127.0.0.1:4000/admin/groups/<groupId> \
  -H 'Authorization: ******'
```

#### `POST /admin/groups/:id/members`

Adds a user to the group (or updates their role if already a member). `email` must already belong to an
existing account, same requirement as `/devices/:id/grants`.

```sh
curl -X POST http://127.0.0.1:4000/admin/groups/<groupId>/members \
  -H 'Authorization: ******' \
  -H 'Content-Type: application/json' \
  -d '{"email":"viewer@example.com","role":"VIEWER"}'
```

#### `DELETE /admin/groups/:id/members/:userId`

```sh
curl -X DELETE http://127.0.0.1:4000/admin/groups/<groupId>/members/<userId> \
  -H 'Authorization: ******'
```

#### `POST /admin/devices/:id/group`

Assigns a device to a group, or unassigns it with `"groupId": null`.

```sh
curl -X POST http://127.0.0.1:4000/admin/devices/<deviceId>/group \
  -H 'Authorization: ******' \
  -H 'Content-Type: application/json' \
  -d '{"groupId":"<groupId>"}'
```

### Connect / session ticket issuance

#### `POST /devices/:id/connect`

Allowed for the owner or a user with an active grant.

```sh
curl -X POST http://127.0.0.1:4000/devices/<deviceId>/connect \
  -H 'Authorization: Bearer <accessToken>'
```

Example response:

```json
{
  "relayHost": "127.0.0.1",
  "relayPort": 7100,
  "relayDeviceId": "nebula-7f29b8a4f1234567",
  "sessionToken": "<short-lived session JWT>",
  "psk": "<session encryption secret, same value returned at device creation>",
  "expiresAt": "2026-08-17T09:01:00.000Z",
  "role": "VIEWER",
  "creditSecondsRemaining": 0,
  "sessionCommand": "NEBULA_PSK=<session encryption secret> nebula_session --relay 127.0.0.1 --relay-port 7100 --device nebula-7f29b8a4f1234567 --token <short-lived session JWT>"
}
```

`creditSecondsRemaining` is the caller's post-deduction trial balance (always `null` for admins, who
aren't metered). Once it can't cover `CONNECT_CREDIT_COST_SECONDS`, this endpoint returns:

```http
HTTP/1.1 402 Payment Required
```
```json
{ "error": "insufficient_credit", "message": "Not enough connect credit remaining — ask an admin to top up your account" }
```

Current expectation for the CWA/session side (the Flutter manager's "Cloud" tab does exactly this
automatically — see `app/manager/lib/cloud_client.dart`):

```sh
NEBULA_PSK=<psk from the response above> nebula_session --relay <host> --relay-port <port> --device <relayDeviceId> --token <sessionToken>
```

Today the relay/session token handling is being extended in parallel on the C++ side. The CLI shape above is the intended contract.

### Relay integration callback

#### `POST /internal/authorize`

Headers:
- `X-Relay-Secret: <RELAY_SHARED_SECRET>`

Body:

```json
{
  "deviceId": "<relayDeviceId>",
  "token": "<sessionToken JWT from nebula_session>"
}
```

Example call:

```sh
curl -X POST http://127.0.0.1:4000/internal/authorize \
  -H 'Content-Type: application/json' \
  -H 'X-Relay-Secret: <RELAY_SHARED_SECRET>' \
  -d '{"deviceId":"nebula-7f29b8a4f1234567","token":"<sessionToken>"}'
```

Success response:

```json
{ "authorized": true }
```

Business-logic deny response (still HTTP 200 by design):

```json
{ "authorized": false, "reason": "user is no longer authorized for this device" }
```

Wrong or missing relay secret:

```http
HTTP/1.1 401 Unauthorized
```

### Why this endpoint returns `200 authorized:false`

For `/internal/authorize`, a non-200 should mean transport/service failure to the relay HTTP client.
A policy decision (expired token, wrong device, revoked grant) is returned as:

```json
{ "authorized": false, "reason": "..." }
```

This lets the relay distinguish:
- **HTTP 200 + authorized=false** -> clean deny
- **HTTP 401/500/timeout** -> infrastructure/authn problem

#### `POST /internal/heartbeat`

Called by `nebula_relay` (not by the CWA/browser client) to report that a VDA is registered and
alive on the classic QUIC/relay path, so `GET /devices`' `online`/`lastSeenAt` reflect it too —
previously only the WebRTC signaling path (below) ever refreshed those fields. Purely informational:
a failed/unreachable call never affects the relay's own pairing/bridging decisions.

Headers:
- `X-Relay-Secret: <RELAY_SHARED_SECRET>`

Body:

```json
{ "deviceId": "<relayDeviceId>" }
```

Example call:

```sh
curl -X POST http://127.0.0.1:4000/internal/heartbeat \
  -H 'Content-Type: application/json' \
  -H 'X-Relay-Secret: <RELAY_SHARED_SECRET>' \
  -d '{"deviceId":"nebula-7f29b8a4f1234567"}'
```

Responses:

```json
{ "ok": true, "lastSeenAt": "2026-08-18T09:00:00.000Z" }
```

```http
HTTP/1.1 401 Unauthorized
```
```json
{ "ok": false, "reason": "relay secret mismatch" }
```

```http
HTTP/1.1 404 Not Found
```
```json
{ "ok": false, "reason": "device not found" }
```

## How this integrates with `nebula_relay`

Relay-side contract expected by this service:

- Relay is configured with:
  - `--saas-auth-url http(s)://<nebula_cloud>/internal/authorize`
  - `--saas-auth-secret <same value as RELAY_SHARED_SECRET>`
  - optionally `--saas-heartbeat-url http(s)://<nebula_cloud>/internal/heartbeat` and
    `--saas-heartbeat-interval-secs <n>` (default 30) so `online`/`lastSeenAt` stay fresh on the
    classic QUIC/relay path too (see `server/nebula_relay/DEPLOY.md`)
- Before bridging a viewer session, relay sends:
  - header `X-Relay-Secret: <shared secret>`
  - body `{"deviceId":"<relayDeviceId>","token":"<sessionTokenJWT>"}`
- `nebula_cloud` verifies:
  1. relay shared secret
  2. JWT signature + expiry
  3. JWT `deviceId` matches the requested relay device id
  4. matching `ConnectionAudit` exists and is unused
  5. requester still owns the device or still has an active non-revoked grant
- On success it sets `usedAt` and `result=AUTHORIZED`.
- On failure it updates the audit row with `DENIED` or `EXPIRED` and returns `authorized:false`.
- Separately (and unrelated to authorization), on VDA registration/reattach and then every
  `--saas-heartbeat-interval-secs`, the relay POSTs `/internal/heartbeat` for each VDA it still has
  registered, keeping the device's online status current for the dashboard.

## Browser (WebRTC) viewer

Real, unmodified browsers only speak WebRTC (ICE + DTLS + SRTP) for live media — there is no way around
that protocol if the goal is "open a URL and see the screen, no install." The native `nebula_vda` gained an
**opt-in, fully independent second path** for this (see the main repo's `core/inc/WebRtcGateway.h` /
`WebRtcSession.h` and `ROADMAP.md` §12), built on [libdatachannel](https://github.com/paullouisageneau/libdatachannel)
(MPL 2.0) — not Google's libwebrtc, and not derived from any GPL/LGPL code.

### Enabling it on the VDA

```sh
nebula_vda --port 7000 --h264 \
  --webrtc --webrtc-signaling-url ws://<nebula_cloud-host>:4000/ws/signaling \
  --device <relayDeviceId> --token <relayToken> \
  --webrtc-width 1920 --webrtc-height 1080 \
  --stun stun.l.google.com:19302 \
  --turn turn.example.com:3478 <turn-username> <turn-password>
```

- `--device`/`--token` reuse the **same** long-lived credentials the VDA registers with `nebula_relay` (this
  service verifies them the same way, via `DeviceService.verifyRelayToken`).
- `--webrtc-width`/`--webrtc-height` size the mandatory virtual display for browser viewers, independent of
  whatever a native `nebula_session` CWA might separately request — one shared capture pipeline serves every
  connected viewer (native or browser), so it can only have one resolution at a time; whichever client
  triggers the pipeline first wins the resolution (see `ROADMAP.md` §3/§12).
- Real browsers only negotiate H264 broadly for WebRTC video (not HEVC), so `--webrtc` forces H264.
- Unlike the QUIC/relay path (one active viewer at a time, see `ROADMAP.md` §3), **the WebRTC path supports
  genuinely concurrent multiple browser viewers** — each gets its own independent DTLS/SRTP session, so there's
  no shared-key nonce-reuse concern the QUIC path's single encryption session has.

### Watching from a browser

1. Log into the dashboard (`/dashboard`) and click **Connect** on a device — this opens `/watch?device=<id>`.
2. `watch.js` calls `POST /devices/:id/connect` for a session ticket, opens `/ws/signaling`, sends
   `{"type":"hello","role":"viewer","deviceId":...,"token":<sessionToken>}`.
3. On `viewer-join`, the VDA creates a real `RTCPeerConnection`-compatible offer; `watch.js` answers it with the
   browser's native `RTCPeerConnection`, and video/audio/data-channel flow directly between the browser and the
   VDA once ICE/DTLS complete (`nebula_cloud` only ever relays the small JSON signaling messages).
4. Mouse/keyboard captured on the `<video>` element are encoded into the exact same 24-byte `NebulaInputEvent`
   wire format the native `nebula_session` client uses (see `core/inc/NebulaInput.h`) and sent over the
   WebRTC DataChannel the VDA creates.

### TURN server (for NAT types STUN alone can't solve)

STUN-only ICE candidates work for direct/cone NATs but not for symmetric NATs (see the main repo's
`NAT_TRAVERSAL.md`). For those, deploy a standard TURN server — **[coturn](https://github.com/coturn/coturn) is
the well-known, battle-tested choice; this repo does not implement its own TURN server.**

```sh
# Debian/Ubuntu
sudo apt-get install coturn
# Minimal /etc/turnserver.conf
listening-port=3478
fingerprint
lt-cred-mech
user=nebula:CHANGE_ME
realm=your-domain.example
# then point nebula_vda at it:
#   --turn your-domain.example:3478 nebula CHANGE_ME
```

Point every `nebula_vda --webrtc` invocation and the browser client (`window.NEBULA_ICE_SERVERS` in
`watch.html`, or edit the default in `public/watch.js`) at the same STUN/TURN servers.

## Schema / migration notes

- Prisma schema: `prisma/schema.prisma`
- Initial migration: `prisma/migrations/20260817180000_init/migration.sql`
- Groups + admin role migration: `prisma/migrations/20260824021120_add_groups_and_admin_role/migration.sql`
  (adds `User.role`, `Device.groupId`, and the `Group`/`GroupMembership` tables)
- Device PSK migration: `prisma/migrations/20260824043827_add_device_psk/migration.sql` (adds `Device.psk`)
- User credit migration: `prisma/migrations/20260825014654_add_user_credit/migration.sql` (adds `User.creditSeconds`)
- Fresh DB apply path:

```sh
docker compose up -d postgres
npm run prisma:migrate
```

## Project layout

```text
server/nebula_cloud/
├── prisma/
│   ├── schema.prisma
│   └── migrations/
├── public/
│   ├── index.html
│   ├── dashboard.html
│   ├── watch.html       # browser WebRTC viewer (P4)
│   ├── app.js
│   ├── watch.js         # RTCPeerConnection + NebulaInputEvent encoding
│   ├── styles.css
│   └── watch.css
├── scripts/
│   └── smoke-flow.mjs
├── src/
│   ├── app.ts
│   ├── server.ts
│   ├── config.ts
│   ├── signaling.ts     # /ws/signaling — WebRTC offer/answer/ICE relay
│   ├── domain/
│   ├── repositories/
│   └── services/
├── tests/
├── Dockerfile
├── docker-compose.yml
└── .env.example
```
