# NebulaDesk desktop host

The Tauri 2 management host exposes only `desktop_request({ request })`. Rust
owns manager credentials and refresh rotation, agent enrollment, native file
dialogs, and native-client process lifetimes. No webview HTTP, shell, filesystem,
or dialog-plugin permission is granted. Production pages cannot navigate to
remote origins or contact a manager directly.

## Build and development

Service-only builds have no GUI feature:

```sh
cargo test -p nebula-desktop --lib
```

From the repository root, prepare the frontend and the target-specific native
sidecars before compiling the GUI:

```sh
npm --prefix apps/desktop-ui ci
npm --prefix apps/desktop-ui run build
npm --prefix apps/desktop-host ci
node scripts/desktop-sidecars.mjs --debug
cargo build -p nebula-desktop --features gui
```

For hot reload, run the locked Tauri 2 CLI from the repository root:

```sh
npm --prefix apps/desktop-host run dev
```

The configured pre-development command starts the frontend on port 1420.
`gui` is enabled by Tauri configuration. To build an application bundle, first
prepare release sidecars (`node scripts/desktop-sidecars.mjs`), then run
`npm --prefix apps/desktop-host run build`. The script's `--target <triple>` option
must match the Tauri CLI target. Cross-compilation needs the target's native
toolchain and platform libraries. Node is used only for UI/build preparation;
the installed application does not need Node. Supported macOS starts at 26.0.
Tauri's build command enables `custom-protocol`, embedding production assets
instead of loading the development URL. A direct embedded-asset Cargo build
must explicitly use `--features gui,custom-protocol`.

Tauri embeds the frontend and bundles `nebula-client`, `nebula-agent`, and the
desktop-owned `nebula-desktop-agent` helper next to the application executable.
The build input names include the target triple;
installed executable names do not. Development builds may find these known
first-party executable names in the workspace's `target/debug` or
`target/release`; release builds never search a workspace, PATH, or
caller-supplied executable path.

## Runtime

Closing the management window hides it. The tray's Open action restores it.
Quit warns about active native sessions, closes their IPC pipes, requests
disconnect, and reaps them; a hung owned client is terminated after a bounded
ten-second grace period. Logout also clears sessions and human credentials.
A separately enabled local Agent is stopped only by the explicit local-host
toggle and remains controllable after restarting the management application.
The tray also provides **Stop local sharing**, with native confirmation and
error reporting. This authenticated local-helper operation remains available
while logged out or when the Manager is unavailable; it terminates incoming
sharing, not outgoing client sessions.

Manager URLs require HTTPS by default. `login` and `enroll_local` accept
`allow_insecure_http: true` solely for explicit development use with loopback
or private IP addresses. Redirects are never followed, including same-origin
redirects. Authentication failures are not replaced with demo data.

The app-managed identity is separate from `NEBULA_AGENT_STATE` and defaults to
the application's data directory under `managed-agent/identity.json`.
`NEBULA_DESKTOP_AGENT_STATE` can select another dedicated identity path.
Never point it at an existing independently managed Agent identity.
The first-party helper holds a service-lifetime file lock and exposes only
bounded authenticated status/stop requests on loopback. Its random local-control
capability is stored in a private adjacent control file, never returned to the
webview or passed in arguments. A stale control file is reclaimed only after
proving that no service holds the lock. No process scanning or stored-PID
termination is used. An externally managed identity is never adopted.

`LocalHost.running` describes the authenticated helper process, not gateway
readiness. `connection_state` remains `unknown`: current Agent library APIs do
not expose a trustworthy readiness signal. Manager machine status is the
authoritative remote reachability indicator.

Manager response types are allowlisted. `create_enrollment { name }` returns
only the one-time enrollment token and its expiry for the explicit enrollment
workflow; access/refresh tokens and session tickets never cross the bridge.
Entitlement subjects are normalized to `user_id` and `group_id`. Missing
manager endpoints reject with a bounded `unavailable` error.
Recipient labels and grant expiry/revocation timestamps are preserved so
historical grants cannot appear active accidentally.
