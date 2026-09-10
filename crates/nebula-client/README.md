# Native sessions

`nebula-client list` and `nebula-client connect <resource>` retain their manager,
tenant, email and password options. The session window uses winit, the native
video decoder and one wgpu surface for both video and egui controls.
No media enters a WebView or the desktop control protocol.

The toolbar provides connection settings, audio, clipboard, file selection,
fullscreen and disconnect. The settings panel can be collapsed; toolbar,
footer, letterboxing and the visible panel never receive remote pointer input.
Video coordinates and rendering share the same DPI-aware viewport.
Audio and clipboard changes apply only to this connection and cannot exceed
the ticket policy. Disabling clipboard text/image sharing does not disable
authorized copied-file transfers. Muting drops playback and its queued samples;
enabling sound opens a fresh stream and decoder.

## Desktop supervision

The desktop host launches **`nebula-client desktop-session`** without login
arguments. Its private stdin begins with the version-1 `Launch` object defined
by `nebula-desktop-protocol`, followed by control commands. The complete wire
contract is in [`DESKTOP_CONTRACT.md`](../../docs/architecture/DESKTOP_CONTRACT.md).
Lines are limited to 64 KiB, including their newline; a file selection is limited
to 64 paths. Invalid commands and stdin EOF end the managed session.

Only control/status NDJSON is written to stdout; diagnostics use stderr.
Tickets are not command-line arguments, Debug output or error text.
Command, file-selection and output queues are bounded. A stalled supervisor
ends the session rather than blocking its render or network thread.
Window close and disconnect request a protocol Bye, close the transport and
release the gateway before exiting. A stalled shutdown has a bounded fallback;
the host should allow at least 10 seconds before forcefully reaping a child.

Connected is emitted only after the encrypted agent handshake succeeds.
RTT is the authenticated end-to-end active-path measurement, or null until
measured. Resolution comes from actual decoded pictures. Transfer progress
comes from the shared file engine; sending every chunk is **not** completion.
Send completion requires the receiver's verified final ACK; receive completion
requires hash verification and publication. Unconfirmed transfers fail when
the session ends.

## Application windows

APP resources use independent **local native windows**, not a desktop player or a
cropped desktop fallback. Each authorized surface has its own hardware decoder,
bounded reorder buffer and latest-frame mailbox. Removed IDs are never reused.
The client requires explicit application negotiation; old or incompatible hosts
fail visibly instead of opening a desktop. Connection/first-video startup is
bounded to 30 seconds. Session-wide clipboard, file transfer and audio are not
enabled in application mode. Application `Connected` is emitted only after an
actual decoded picture is available. Video is tagged with both geometry
generation and a per-surface sequence; multipath's global sequence is not a
decoder sequence.

Closing a native window asks the remote application to close normally. The local
window remains until the remote surface is removed, so Save/Cancel dialogs still
work. In a view-only session, local close disconnects the view rather than
requesting an unauthorized remote document close. Focus, resize and minimize are surface-scoped. Mac child-window ownership
and Windows owner/enable hooks complement client-side modal input gating.
Wayland/winit currently lacks a public top-level transient-parent API: modal input
is gated, but compositor-enforced parent stacking/grouping is not implemented.
These windows are presentation isolation, **not a security sandbox**.
The macOS global application menu is not represented or forwarded yet. Use
in-window controls or keyboard shortcuts; the client never substitutes a capture
of the desktop menu bar.

Keyboard input defaults to physical mappings, including existing desktop
sessions. Semantic editing shortcuts are opt-in per resource:

```sh
nebula-client connect "Editor" --keyboard-mode semantic \
  --keyboard-profile editing --host-os macos
```

The same options are supported on `desktop-session`, so the supervising host can
select them per resource without adding executable paths or credentials to IPC.
`Launch.application_windows` must be true to enter application mode; absent/false
keeps the existing desktop implementation. Direct clients also require the
manager's `SessionTicket.application_windows` hint to match the requested resource
kind; missing hints remain desktop-only. `--host-os` is an optional override of
the negotiated host advisory. Unknown host conventions preserve physical input.

The editing profile maps common C/V/X/A/S/F/Z chords between Control and Command
when the client/host conventions differ. `--keyboard-profile terminal` preserves
Control+C; modifiers are not globally remapped. Local task-switch chords remain
local. Text/IME composition is **not implemented**; application windows explicitly
disable IME and use physical key events, rather than pretending single-character
metadata is a composition protocol.

## Fonts

Chinese labels use an installed OS CJK font (PingFang/Heiti/Songti on macOS,
Microsoft YaHei/SimSun on Windows, Noto CJK/WenQuanYi on Linux). Fonts are read
locally, not redistributed. If none is available, the window visibly explains
that it is using English labels.

## Offline window preview

```sh
cargo run -p nebula-client --example session_preview
```

This opens the actual native window in its disconnected/error state. Its
deliberately invalid key is rejected before network access. It does not capture
a display, connect to a machine, or substitute a fake remote desktop. Close the
window to exit; this is a UI preview, not a streaming demonstration.
