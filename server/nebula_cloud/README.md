# nebula_cloud

`nebula_cloud` is a new Node.js + TypeScript + PostgreSQL control-plane for Nebula.
It layers multi-tenant accounts, device ownership, sharing, and short-lived relay session tickets on top of today's `nebula_relay` pairing model.

## What this service does

- Users register/login with email + password.
- A user owns SaaS devices.
- Each device has a stable **relayDeviceId** and a long-lived **relay token** used only by the VDA side to register with `nebula_relay`.
- Owners can grant access to other existing users.
- A viewer/controller asks `nebula_cloud` for a **short-lived session JWT** via `POST /devices/:id/connect`.
- `nebula_relay` calls `POST /internal/authorize` before bridging a session and gets a live yes/no answer.
- **New (P4, see the main repo's `ROADMAP.md` §12): a real browser can watch/control a device directly**, no native
  client install required. `nebula_cloud` runs a `/ws/signaling` WebSocket endpoint that relays SDP offer/answer and
  trickle ICE candidates between a `nebula_vda --webrtc` process and the browser's `RTCPeerConnection` — pure
  message pass-through, `nebula_cloud` never touches media bytes. See "Browser (WebRTC) viewer" below.

## Out of scope

- No change to the *native-client* media path: `nebula_relay`, `nebula_vda`, and `nebula_session` still carry that
  traffic exactly as before. The WebRTC path (previous bullet) is a fully separate, opt-in path on the VDA side.
- No TURN server implementation — deploy standard `coturn` (see "Browser (WebRTC) viewer" below); this service only
  tells clients which STUN/TURN servers to use, it doesn't relay media itself either way.
- No device-side daemon yet; heartbeat/claim flows are kept minimal and documented for later relay/VDA integration.

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
- `createdAt`

### Device
- `id` UUID (SaaS device id)
- `ownerUserId` FK -> User
- `name`
- `relayDeviceId` unique stable string configured into `nebula_vda`
- `relayTokenHash` SHA-256 hash of the long-lived relay token
- `claimCodeHash`, `claimCodeExpiresAt` for one-time onboarding
- `createdAt`, `lastSeenAt`

### AccessGrant
- `id` UUID
- `deviceId` FK -> Device
- `granteeUserId` FK -> User
- `role` enum: `VIEWER | CONTROLLER`
- `createdAt`, `revokedAt`
- `createdByUserId`

> Owner access stays implicit through `Device.ownerUserId`.

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
    "claimCode": "NEB-ABCDE-FGHIJ",
    "claimCodeExpiresAt": "2026-08-17T10:00:00.000Z",
    "vdaCommand": "nebula_vda --port 7000 --relay 127.0.0.1 --relay-port 7100 --device nebula-7f29b8a4f1234567 --token <long-lived relay token>"
  }
}
```

That response is exactly the bridge to the existing Nebula CLI contract:

```sh
nebula_vda --port 7000 --relay <host> --relay-port <port> --device <relayDeviceId> --token <relayToken>
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

Optional/future hook to update `lastSeenAt`.

Owner-authenticated form:

```sh
curl -X POST http://127.0.0.1:4000/devices/<deviceId>/heartbeat \
  -H 'Authorization: Bearer <accessToken>'
```

Relay-authenticated form:

```sh
curl -X POST http://127.0.0.1:4000/devices/<deviceId>/heartbeat \
  -H 'X-Relay-Secret: <RELAY_SHARED_SECRET>'
```

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
  "expiresAt": "2026-08-17T09:01:00.000Z",
  "role": "VIEWER",
  "sessionCommand": "NEBULA_PSK=<shared-psk> nebula_session --relay 127.0.0.1 --relay-port 7100 --device nebula-7f29b8a4f1234567 --token <short-lived session JWT>"
}
```

Current expectation for the CWA/session side:

```sh
NEBULA_PSK=<shared-psk> nebula_session --relay <host> --relay-port <port> --device <relayDeviceId> --token <sessionToken>
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

## How this integrates with `nebula_relay`

Relay-side contract expected by this service:

- Relay is configured with:
  - `--saas-auth-url http(s)://<nebula_cloud>/internal/authorize`
  - `--saas-auth-secret <same value as RELAY_SHARED_SECRET>`
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
