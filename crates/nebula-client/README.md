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

## Fonts

Chinese labels use an installed OS CJK font (PingFang/Heiti/Songti on macOS,
Microsoft YaHei/SimSun on Windows, Noto CJK/WenQuanYi on Linux). Fonts are read
locally, not redistributed. If none is available, the window visibly explains
that it is using English labels.
