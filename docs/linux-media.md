# Linux desktop media

## Native dependencies

The Linux agent requires an active Wayland user session with PipeWire,
WirePlumber, xdg-desktop-portal and a matching compositor portal backend.
Ubuntu 24.04 (GStreamer 1.24) or newer is the minimum native media baseline.
GNOME and KDE provide ScreenCast and RemoteDesktop portals. A ScreenCast-only
backend can serve view-only sessions, but cannot accept remote keyboard/mouse
control. No X11 capture, screenshot polling, ffmpeg process, uinput bypass, or
software H.264 fallback is used.

Build packages on Ubuntu:

```sh
sudo apt-get install pkg-config libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev libpipewire-0.3-dev \
  libasound2-dev libudev-dev libopus-dev
```

Runtime packages (in addition to your installed desktop and portal backend):

```sh
sudo apt-get install gstreamer1.0-pipewire gstreamer1.0-plugins-base \
  gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-tools pipewire wireplumber xdg-desktop-portal vainfo
```

Install the GPU vendor's VA driver, for example `intel-media-va-driver` for
supported Intel GPUs or `mesa-va-drivers` for supported AMD GPUs. GPU models and
distribution builds differ in H.264 encode/decode availability. The desktop user
must have access to the GPU render node. NVIDIA configurations without a working
VA-API encoder are **not supported** by this backend; NVENC is not implemented.
Neither elevated privileges nor disabling portal policy is a supported remedy.

Rust bindings are target-specific: ashpd 0.12, GStreamer Rust 0.24, and arboard's
`wayland-data-control` feature. Use the repository lockfile (`--locked`).
`kstring` is locked to 2.0.2 because 2.0.4 raises its compiler requirement to Rust
1.96; 2.0.2 supports Rust 1.73 and satisfies GStreamer's dependency range.

## Consent and session boundaries

Run the agent as the logged-in desktop user with `WAYLAND_DISPLAY`,
`XDG_RUNTIME_DIR`, and `DBUS_SESSION_BUS_ADDRESS` inherited from that session.
An SSH/root/system-service environment without these is rejected explicitly.
Unattended/headless login screens and virtual-display provisioning are not
implemented.

Every authenticated session owns a fresh portal session. A viewer is offered
ScreenCast consent only; a controller additionally requests the RemoteDesktop
keyboard and pointer devices. Select exactly one monitor. The portal must supply
its logical size. Embedded cursor mode is required. There are no persisted
restore tokens and no automatic consent reuse across viewers.

The agent's portal worker owns its own Tokio runtime, so synchronous probes work
without a caller runtime. Consent times out after 120 seconds. Portal calls and
input acknowledgements have bounded timeouts. Stopping video closes the portal
and its dialogs. User revocation ends capture; input failure also revokes the
session rather than executing a stale queued release later. Capture startup
runs off the agent's async executor. If the connection is revoked during the
synchronous consent wait, the connection closes immediately and the eventual
source is stopped automatically; an already-open consent dialog can remain
until cancellation or the 120-second timeout.

Input uses the portal's permission-checked `Notify*` methods (supported on
GNOME/KDE), not an unprivileged global device or XTest. USB keyboard usages map
to **evdev** codes, not XKB codes (+8). Physical keys follow the host layout;
client Unicode layout translation is not implemented. The compositor owns key
repeat. Pointer leave releases tracked keys/buttons; closing the portal removes
the remote input session. High-resolution wheel deltas accumulate into discrete
steps. Touch, additional monitor indices, and undefined key usages are rejected.

## Media and recovery

Capture uses the **portal-issued PipeWire fd and selected node ID**, never an
unrestricted screen source. Native `pipewiresrc` feeds a bounded, raw-frame queue,
`vapostproc` performs VA-API scaling/color conversion, and `vah264enc` uses zero
B-frames and one reference frame. Output is capped to the requested dimensions
without changing aspect ratio. Compositor/GPU format negotiation can involve
upload or DMA-BUF import; this is not a claim of universal zero-copy.

`h264parse` aligns complete access units. The agent converts Annex B to four-byte
big-endian AVCC lengths and prepends cached SPS/PPS to **every IDR**, matching the
macOS VideoToolbox wire format. Encoded appsinks never discard packets. If the
network sink is full, an IDR is requested and dependent frames are suppressed
until an IDR is actually delivered. Dropping before encoding is safe.

The client explicitly selects `vah264dec`; it never autoplugs a software decoder.
It submits one access unit at a time, waits at most two seconds for the decoded
picture, and destroys decoder state on failure so the existing session loop can
request an IDR. Streams requiring picture reordering are rejected by timeout.
Native decoded NV12 planes are mapped and deinterleaved into the existing owned I420 `Picture`
for wgpu upload. This preserves the renderer interface, **not GPU zero-copy**.

Audio uses a separate native PipeWire connection with `stream.capture.sink=true`:
WirePlumber selects the default output sink's **monitor**, not a microphone.
48 kHz interleaved PCM is encoded using the existing Opus library in 20 ms
packets with in-band FEC. The session's audio policy controls whether it starts.
Linux does not currently provide an audio-sharing portal prompt; monitor access
uses the logged-in user's PipeWire permissions. No monitor/microphone fallback
is attempted if the stream fails. Queues are bounded and late audio is dropped.

## Clipboard

Existing `SystemClipboard` text/PNG handling is reused with arboard's native
Wayland data-control backend enabled. This is actual Wayland protocol access,
not `wl-copy`/`wl-paste` subprocesses. The compositor must expose the protocol
supported by `wl-clipboard-rs` (notably wlroots and compatible KDE versions).
GNOME does not universally expose data-control; arboard may use an available
XWayland clipboard, otherwise clipboard opening fails explicitly. Do not assume
GNOME Wayland clipboard support solely because screen sharing works.

## Validation and manual desktop probes

Headless compilation/unit tests validate signatures, wire conversion, loss
recovery and plane packing, **not** real screen/VA/audio operation.

```sh
cargo clippy --locked -p nebula-agent -p nebula-client --all-targets
cargo test --locked -p nebula-agent -p nebula-client --lib
gst-inspect-1.0 pipewiresrc
gst-inspect-1.0 vah264enc
gst-inspect-1.0 vah264dec
vainfo
cargo run -p nebula-agent --example capture_probe
cargo test -p nebula-client --test media -- --ignored --nocapture
```

On a real supported desktop/GPU, also exercise consent denial, stop while
idle, user revocation, concurrent independent viewers, a full network queue,
IDR recovery, resolution changes, both modifier sides, pointer edges on a
scaled monitor, default-sink monitor audio and clipboard text/PNG. Verify
Linux-to-macOS and macOS-to-Linux streams before claiming runtime interoperability.
No desktop/GPU runtime pass is implied by adding this backend.

API references:

- [ashpd RemoteDesktop + ScreenCast](https://docs.rs/ashpd/0.12.3/ashpd/desktop/remote_desktop/index.html)
- [GStreamer VA H.264 encoder](https://gstreamer.freedesktop.org/documentation/va/vah264enc.html)
- [GStreamer VA postprocessor](https://gstreamer.freedesktop.org/documentation/va/vapostproc.html)
- [GStreamer VA H.264 decoder](https://gstreamer.freedesktop.org/documentation/va/vah264dec.html)
- [PipeWire stream properties](https://docs.pipewire.org/page_man_pipewire-props_7.html)
