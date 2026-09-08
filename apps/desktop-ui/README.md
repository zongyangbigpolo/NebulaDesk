# NebulaDesk desktop presentation

React + TypeScript + Vite frontend for the approved `design/product-ui` references.
This folder owns presentation only; the native titlebar, authentication, Manager
client, Agent and session processes belong to the Rust desktop host.

## Commands

Run in `apps/desktop-ui` with Node 22:

```sh
npm ci
npm run dev
npm run typecheck
npm test
npm run build
npm run preview
```

- `dev`: `127.0.0.1:1420`, strict port, for Tauri's development URL.
- `build`: TypeScript checking plus production assets in `dist/`.
- `preview`: loopback production preview on Vite's default port 4173.
- `test`: Vitest + Testing Library, no remote systems or native processes.
- `test:watch`: interactive Vitest.

The native host must load `dist/` after building. Generated assets and dependencies
are not committed. Tauri launcher/configuration is owned by the parent integration.

## Explicit browser demonstration

Open `http://127.0.0.1:1420/?demo=1` (or preview port 4173). The visible demo badge
identifies a separate, lazily loaded, in-memory adapter. Example devices, accounts,
paths and files exist only in `src/api/demo.ts`. Never enter real credentials in
demo mode. Refresh resets demo changes.

The demo supports resources/search/filtering, device details, sharing, grants,
publication, enrollment, transfer records and logout/login. A native-session
action explains the independent-window behavior; no video or remote desktop is
simulated. Permission-settings actions explain their effect without opening OS
settings. Production errors never select the demo adapter.

## Boundary and state

`src/api/types.ts` mirrors `docs/architecture/DESKTOP_CONTRACT.md`.
`src/api/desktop.ts` is the sole production transport: Tauri 2
`invoke('desktop_request', { request })`. Structured `{code,message}` and textual
errors are displayed without optimistic success. The coordinated additions are
`create_enrollment`, `open_permission_settings` and explicit development HTTP
opt-in for login/enrollment. Grant roles are `VIEWER`, `CONTROLLER`, `ADMIN`.

`DesktopStore` owns authenticated state, serial mutations and non-overlapping
3-second session/transfer polls while visible. Authentication epochs and read
versions reject stale responses after logout or mutations. Component queries
ignore replies after unmount/selection changes. Login passwords are cleared on
submit; enrollment tokens remain transient. No local/session storage is used.

`src/ui` contains typed view components and dialogs without Tauri imports;
`App.tsx` wires intent callbacks to the API/store. Locally bundled Lucide SVGs
and system fonts require no CDN, telemetry or external asset server.
The layout uses the actual native frame, not simulated traffic lights.

## Deliberate product boundaries

- APP publication is metadata management. Unsupported streaming stays visibly
  disabled; it never connects the complete desktop instead.
- Unknown OS/account metadata remains unknown. A local sharing toggle is not an
  authorization boundary; backend ownership remains authoritative.
- Existing grants display `user_id`/`group_id`, because the contract does not
  return recipient email. New grants use an exact complete email, not a directory.
- Recent use comes from observed session history, not fabricated timestamps.
  The contract has no persisted recents or host access audit endpoint.
- No remote image/audio payload, bearer token, session ticket or executable path
  from another user's resource is needed by the presentation layer.
- Account settings show actual account/host state. Audio and clipboard controls
  belong to each independent native session, not this management window.
- File selection is native. Cancellation does not imply a transfer completed.
