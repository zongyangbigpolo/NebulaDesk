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
node scripts/desktop-sidecars.mjs --debug
cargo build -p nebula-desktop --features gui
```

For hot reload, use the Tauri 2 CLI from `crates/nebula-desktop`:

```sh
cargo tauri dev
```

The configured pre-development command starts the frontend on port 1420.
`gui` is enabled by Tauri configuration. To build an application bundle, first
prepare release sidecars (`node scripts/desktop-sidecars.mjs`), then run
`cargo tauri build` from this crate. The script's `--target <triple>` option
must match the Tauri CLI target. Cross-compilation needs the target's native
toolchain and platform libraries. Node is used only for UI/build preparation;
the installed application does not need Node.

Tauri embeds the frontend and bundles `nebula-client` and `nebula-agent` next
to the application executable. The build input names include the target triple;
installed executable names do not. Development builds may find these known
first-party executable names in the workspace's `target/debug` or
`target/release`; release builds never search a workspace, PATH, or
caller-supplied executable path.

## Runtime

Closing the management window hides it. The tray's Open action restores it.
Quit warns about active native sessions, closes their IPC pipes, requests
disconnect, and reaps them; a hung owned client is terminated after a bounded
grace period. Logout also clears sessions and human credentials. A separately
enabled local Agent is stopped only by the explicit local-host toggle.

Manager URLs require HTTPS by default. `login` and `enroll_local` accept
`allow_insecure_http: true` solely for explicit development use with loopback
or private IP addresses. Redirects are never followed, including same-origin
redirects. Authentication failures are not replaced with demo data.

The app-managed identity is separate from `NEBULA_AGENT_STATE` and defaults to
the application's data directory under `managed-agent/identity.json`.
`NEBULA_DESKTOP_AGENT_STATE` can select another dedicated identity path.
Never point it at an existing independently managed Agent identity.

Manager response types are allowlisted. `create_enrollment { name }` returns
only the one-time enrollment token and its expiry for the explicit enrollment
workflow; access/refresh tokens and session tickets never cross the bridge.
Entitlement subjects are normalized to `user_id` and `group_id`. Missing
manager endpoints reject with a bounded `unavailable` error.
