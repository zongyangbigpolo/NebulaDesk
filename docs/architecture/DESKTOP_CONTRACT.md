# Desktop boundary contract

Implementation contract for the approved `design/product-ui` screens.
This document defines the boundaries; `PRODUCT_ARCHITECTURE.md` describes
the final assembled system.

## Ownership

- `apps/desktop-ui`: React, TypeScript and CSS. Presentation only.
- `crates/nebula-desktop`: Tauri 2 application, Rust application services,
  Manager HTTP client, local Agent supervision and native-session supervision.
- `crates/nebula-client`: independent native session process, winit/wgpu
  presentation and the existing QUIC/media/input implementation.
- `crates/nebula-desktop-protocol`: shared Rust types for session process IPC.
- `crates/nebula-manager`: authoritative users, ownership, entitlements and
  session admission. A UI toggle is never an authorization boundary.

No video/audio payloads pass through JavaScript or Tauri IPC.
No bearer tokens, machine credentials or session tickets are returned to the UI,
stored in browser storage or passed as process command-line arguments.

## Frontend to desktop host

One Tauri command: `desktop_request`, with `{ request }`.
The `request` is an object tagged by `op` (snake_case). Successful replies are
JSON values with the following shapes; failures reject with `{code,message}`.
The frontend catches and displays errors and does not replace them with demo data.

| Request | Reply |
| --- | --- |
| `{op:"login",manager_url,tenant,email,password,allow_insecure_http?}` | `Account` |
| `{op:"logout"}` | `null` |
| `{op:"account"}` | `Account \| null` |
| `{op:"resources"}` | `Resource[]` |
| `{op:"machines"}` | `Machine[]` (only managed/owned devices) |
| `{op:"resource",id}` | `Resource` |
| `{op:"connect",resource_id}` | `Session` (starting, not yet connected) |
| `{op:"sessions"}` | `Session[]` |
| `{op:"focus_session",session_id}` | `null` |
| `{op:"disconnect_session",session_id}` | `null` |
| `{op:"local_host"}` | `LocalHost` |
| `{op:"create_enrollment",name}` | `{token:string,expires_at:string}` (single-use, never persisted by the UI) |
| `{op:"enroll_local",manager_url,token,name,allow_insecure_http?}` | `LocalHost` |
| `{op:"set_host_enabled",enabled}` | `LocalHost` |
| `{op:"open_permission_settings",permission:"screen" \| "input" \| "audio"}` | `null` (fixed OS settings target; unsupported platforms return an explanatory error) |
| `{op:"rename_machine",machine_id,name}` | `null` |
| `{op:"remove_machine",machine_id}` | `null` |
| `{op:"machine_resources",machine_id}` | `PublishedResource[]` |
| `{op:"publish_resource",machine_id,resource:{kind,name,description,launch_path?,launch_args?}}` | `PublishedResource` |
| `{op:"update_resource",resource_id,changes:{name?,description?,enabled?}}` | `null` |
| `{op:"grants",resource_id}` | `Grant[]` |
| `{op:"grant_access",resource_id,email,role,allow_clipboard,allow_file_transfer,allow_audio}` | `Grant` |
| `{op:"revoke_access",entitlement_id}` | `null` |
| `{op:"transfers"}` | `Transfer[]` |
| `{op:"send_files",session_id}` | `null` (native file picker; cancel is not an error) |

Polling `sessions` and `transfers` is allowed at a bounded cadence while visible;
overlapping polls and stale replies after logout must not replace current state.
The host deduplicates open sessions by resource and focuses an existing window.

```ts
type Account = { id: string; email: string; display_name: string; role: string;
  tenant: string; manager_url: string };
type Policy = { input: boolean; audio: boolean; clipboard: boolean; file_transfer: boolean };
type Resource = { id: string; name: string; kind: "DESKTOP" | "APP";
  description: string; machine_status: string; role: string | null;
  policy: Policy; owner_name: string | null; owned: boolean;
  machine_id: string | null; os: string | null; os_version: string | null;
  last_seen_at: string | null; launch_supported: boolean };
type Machine = { id: string; name: string; os: string; os_version: string;
  arch: string; status: string; owner_user_id: string | null;
  last_seen_at: string | null; capabilities: Record<string, unknown> };
type PublishedResource = { id: string; machine_id: string; kind: "DESKTOP" | "APP";
  name: string; description: string; enabled: boolean; launch_path: string | null };
type Grant = { id: string; resource_id: string; user_id: string | null;
  group_id: string | null; role: string; allow_clipboard: boolean;
  allow_file_transfer: boolean; allow_audio: boolean;
  user_email?: string | null; user_display_name?: string | null; group_name?: string | null;
  expires_at?: string | null; revoked_at?: string | null };
type Session = { session_id: string; resource_id: string; name: string;
  state: "connecting" | "connected" | "disconnected" | "failed";
  path: "direct" | "relay" | null; started_at: string; error: string | null;
  rtt_ms: number | null };
type LocalHost = { enrolled: boolean; machine_id: string | null;
  manager_url: string | null; name: string | null; running: boolean;
  permissions: { screen: "granted" | "denied" | "unknown";
    input: "granted" | "denied" | "unknown"; audio: "granted" | "denied" | "unknown" };
  error: string | null };
type Transfer = { id: string; session_id: string; name: string;
  direction: "send" | "receive"; transferred: number; total: number;
  state: "offered" | "transferring" | "complete" | "failed"; error: string | null };
```

Unknown metadata stays null/unknown, never fabricated. Application publication
metadata is supported separately from native single-application streaming;
an unsupported APP must be clearly disabled, never silently open the whole desktop.
Permission checks that cannot establish an OS grant report unknown, not granted.
Normal resource consumers must not receive another user's executable paths.
Grant roles are `VIEWER`, `CONTROLLER` and `ADMIN`; the last is a resource role,
not tenant directory administration. VIEWER cannot enable any optional channel.
Grant history retains expiry and revocation fields; inactive rows are not shown
as usable or offered another revoke operation. Non-loopback plaintext HTTP requires an
explicit user acknowledgement (`allow_insecure_http`), not a hidden retry.

The UI can be previewed in a normal browser using an **explicit** demo switch.
The demo adapter is a separate module, visibly identified, and never selected
because production login or a command failed.

## Desktop host to native session

The sidecar starts as `nebula-client desktop-session`. Parent-owned stdin/stdout
pipes carry bounded newline-delimited JSON. Diagnostics go only to stderr.
The first input line is:

```json
{"version":1,"resource_id":"uuid","resource_name":"Office Mac","ticket":{"session_id":"uuid","ticket":"secret","gateway_addr":"host:port","gateway_pin":"hex","agent_key":"hex","policy":{"input":true,"audio":true,"clipboard":true,"file_transfer":true}}}
```

Subsequent commands are tagged by `command`:

```json
{"command":"focus"}
{"command":"disconnect"}
{"command":"set_audio","enabled":false}
{"command":"set_clipboard","enabled":false}
{"command":"send_files","paths":["/explicitly-selected/local/file"]}
```

Parent-selected paths originate in a native picker. The child never accepts
arbitrary remote requests to read files. EOF means the owning host exited and
ends this client session. Closing the main management window only hides it, so
its supervised session processes and separately managed Agent remain alive.
Explicit application exit closes client sessions with a clear warning.
Logging out closes client sessions and clears human credentials; stopping the
local Agent remains an explicit operation.

Events are tagged by `event`:

```json
{"event":"state","state":"connected","path":"direct","error":null}
{"event":"metrics","rtt_ms":3.2,"width":1440,"height":900}
{"event":"transfer","id":"send:1","name":"report.pdf","direction":"send","transferred":1024,"total":2048,"state":"transferring","error":null}
```

The child emits connected only after the real handshake. Path changes update
an already connected session. Final state and child exit both clear supervision;
error output must be bounded and must not include launch secrets.

## Manager additions

Keep existing admin APIs compatible. Add owner-aware machine listing, rename,
removal, resource publication/update and resource entitlement management with
tenant+ownership checks. Owners never gain general directory administration.
For grants by email, resolve the exact enabled recipient in the same tenant
without exposing a directory listing.

`GET /v1/resources` and `GET /v1/resources/{id}` return the additional resource
metadata/policy above using the same authoritative grant resolution as admission.
Session admission rejects unsupported application streaming explicitly.
New managed resource creation gives its owner access through a defined,
tested ownership policy, not a frontend-only implicit permission.

Any necessary contract change is coordinated between the frontend, host and
native-session owners before integration.
