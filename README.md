# Nebula

macOS-only remote display framework (Mac **VDA** → Mac **CWA**) over **QUIC**,
modeled on the icagraphics VDA→CWA pipeline. Streams the VDA's screen **video +
audio** to a CWA viewer with full hardware codecs and zero-copy GPU rendering,
plus mouse/keyboard control back to the VDA (see `ARCHITECTURE.md` §8).
Every payload is end-to-end encrypted (see §4a) whether the session is direct,
relayed, or mid-upgrade between the two.

Beyond the native QUIC path there are two independent extensions built on top
of the same capture/encode pipeline: a **WebRTC** path so a plain browser can
watch/control a VDA with no native client install, and an optional
**`nebula_cloud` SaaS control plane** (accounts, device ownership, sharing,
short-lived session tickets, a small web dashboard) layered above
`nebula_relay`'s device-id/token pairing.

See **[PLAN.md](./PLAN.md)** (计划书), **[ARCHITECTURE.md](./ARCHITECTURE.md)**
(wire protocol / component detail), **[ARCHITECTURE_OVERVIEW.md](./ARCHITECTURE_OVERVIEW.md)**
(系统全景图, 中文), **[USAGE.md](./USAGE.md)** (完整运行指南, 中文),
**[NAT_TRAVERSAL.md](./NAT_TRAVERSAL.md)** (NAT 穿透方案), and
**[ROADMAP.md](./ROADMAP.md)** for the relay/upgrade/SaaS design in depth.

## Pipeline

```
VDA: ScreenCaptureKit ─► VideoToolbox(HEVC/H264) ─┐
     (virtual display)                             │
     system audio     ─► AudioToolbox(AAC) ─────────┤─► encrypt ─► QUIC (3 channels) ─► network
                                                     │                                    │
                       OpusAudioEncoder (Opus) ──────┘─► WebRtcGateway (H264/Opus/DataChannel, optional)
                                                                          │
CWA (native): network ─► QUIC ─► decrypt ─► VideoToolbox decode ─► Metal render          ▼
                                            └► AAC decode ─────────► AVAudioEngine   browser RTCPeerConnection
                                                                          <video> + DataChannel input
```

- **Transport**: Apple Network.framework QUIC. Each logical channel
  (Control / Video / Audio) runs on its own QUIC connection so they never
  head-of-line block each other. TLS uses an embedded self-signed identity
  (dev only — replace for production).
- **Encryption**: application-layer ChaCha20-Poly1305 (libsodium) on top of
  every payload, independent of the QUIC/TLS layer — see ARCHITECTURE.md §4a.
  This is what makes `nebula_relay`'s "blind forward" true even though its own
  TLS identity is a dev self-signed cert.
- **Video**: VideoToolbox HW HEVC (default) or H.264 (`--h264`, mandatory for
  the WebRTC path), real-time low latency.
- **Audio**: AudioToolbox AAC for the native path; a separate Opus encoder for
  the WebRTC path (browsers don't support AAC over WebRTC).
- **Render**: Metal NV12→RGB shader, zero-copy via `CVMetalTextureCache`.
- **Virtual display**: the VDA never captures its physical screen — it creates
  a `CGVirtualDisplay` sized to the viewer's logical screen dimensions (sent in
  HELLO, or `--webrtc-width/-height` for browser-only sessions) and captures
  only that. Creation failure is fatal; there's no physical-display fallback.
- **Relay & reconnect**: optional `nebula_relay` bridges NAT'd peers, then
  best-effort upgrades to a direct path (LAN candidates + the relay's own
  observed public address for each peer, plus a lightweight UDP hole-punch —
  no external STUN server involved); a lost direct path falls back to relay
  automatically. The relay also keeps a VDA's registration alive across
  reconnects, grants a 30s grace period on VDA drop, and hands out a
  reconnect ticket so a CWA doesn't have to re-present its shared token.
- **WebRTC (browser) path**: fully independent of QUIC — the VDA fans the same
  encoded H264/Opus frames out to a `WebRtcGateway` (libdatachannel, MPL 2.0)
  that speaks standard ICE/DTLS/SRTP to real browsers via a WebSocket
  signaling relay (`nebula_cloud`'s `/ws/signaling`). Unlike the QUIC path
  (one active viewer at a time), each browser viewer gets its own DTLS/SRTP
  session, so this path supports genuinely concurrent viewers, and supports
  standard TURN (`--turn`) for symmetric-NAT traversal.
- **SaaS control plane** (`server/nebula_cloud`, optional): Node.js/TypeScript +
  PostgreSQL service providing accounts, device ownership, email-based access
  grants (VIEWER/CONTROLLER), short-lived session tickets, connection audit
  logs, WebRTC signaling forwarding, and a minimal web dashboard/watch page.
  See `USAGE.md` and `server/nebula_cloud/README.md` for the full flow.

## Build

```sh
# Native targets (VDA / session / relay)
cmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build build

# One-shot build + package into a single .app bundling the session helper
./scripts/package_app.sh
```

Produces `build/app/vda/nebula_vda`, `build/app/session/nebula_session`, and
(if `libmsquic` is found) `build/server/nebula_relay/nebula_relay`.

Requires Xcode toolchain, macOS 26+, Flutter (for `app/manager`), and:

```sh
brew install libmsquic libsodium opus libjuice srtp libusrsctp nlohmann-json plog
```

plus the system `libcurl` (for `nebula_relay`'s optional SaaS integration).
[libdatachannel](https://github.com/paullouisageneau/libdatachannel) (WebRTC,
MPL 2.0) is fetched and built automatically by the top-level CMake via
`FetchContent` — no separate install needed.

## Run

On the **VDA** Mac (the one being shared):

```sh
./build/app/vda/nebula_vda --port 7000      # options: --fps --bitrate --h264
```

Grant **Screen Recording** permission (System Settings → Privacy & Security →
Screen Recording) on first run. For mouse/keyboard control, also grant
**Accessibility** permission. Relaunch after granting.

On the **CWA** Mac (viewer):

```sh
./build/app/session/nebula_session --host <vda-ip> --port 7000
```

A window opens and renders the VDA's screen with audio. The CWA sends its main
screen's logical dimensions in HELLO; the VDA creates and captures a mandatory
virtual display of exactly that size. Move/click/type in the window to control it.

Or launch/manage sessions from the **Flutter GUI manager** instead of the CLI:

```sh
open "app/manager/build/macos/Build/Products/Debug/nebula_manager.app"
```

Add a VDA (name / host / port / optional relay / key) and click **Connect**;
it spawns and supervises a `nebula_session` subprocess, tracking its lifecycle
via the `NEBULA_STATUS:` stdout protocol (see `core/inc/SessionStatus.h`).

For relay/NAT-traversal, the SaaS control plane, and the browser/WebRTC path,
see **[USAGE.md](./USAGE.md)** for full command lines and flows.

> **Local (single-machine) testing note:** on a Mac with VPNs or many network
> interfaces, Apple's QUIC listener may not bind loopback, so `127.0.0.1` can
> time out. Test across two Macs (use the VDA's LAN IP), or on a host with few
> interfaces. See ARCHITECTURE.md §9.

## Layout

| Dir | Contents |
|-----|----------|
| `core/` | Portable C++: `NebulaProtocol`, `NebulaCrypto`, `NebulaInput`, `NebulaLog`, `NebulaTypes`/`NebulaFrame`, `Signaling`, `RelayProtocol`, `OpusAudioEncoder`, `WebRtcSession`/`WebRtcGateway` (libdatachannel wrapper), `SessionStatus` |
| `platform/mac/` | `ScreenCapture`, `VirtualDisplay`, `VideoEncoder`/`Decoder`, `AudioEncoder`/`Decoder`, `MetalRenderer`, `AudioPlayer`, `InputInjector`/`InputView`, `QuicTransport`, `RelayTransport`, `UpgradingTransport`, `ProcessSpawner`, `VdaServer`, `CwaClient` |
| `app/vda`, `app/session` | native entry points (`nebula_vda`, `nebula_session`) |
| `app/manager` | Flutter desktop GUI: manages a list of VDAs, spawns/supervises `nebula_session` |
| `server/nebula_relay` | blind-forwarding QUIC relay (msquic), reconnect tickets/grace period, optional SaaS authorization callback |
| `server/nebula_cloud` | optional SaaS control plane (Node/TS + PostgreSQL): accounts, device sharing, session tickets, WebRTC signaling, web dashboard |
| `tests/` | `test_input.cpp` (input protocol), `test_crypto.cpp` (session encryption), `test_webrtc_session.cpp` (WebRTC offer/ICE) |

## Notes

- Mouse + keyboard control (CWA→VDA) is **implemented** on the Control channel
  (`MsgType::Input` + `CGEvent` injection) for native clients, and via WebRTC
  DataChannel (same `NebulaInputEvent` wire format) for browser viewers.
  See ARCHITECTURE.md §8.
- The embedded `platform/mac/inc/NebulaIdentityP12.h` is a **development** self-signed
  cert. Provision a real identity and remove the trust-all client verify block
  for production. This is independent of the application-layer encryption in §4a,
  which does not depend on this cert being trustworthy.
- NAT traversal differs by path: the native QUIC path is LAN candidates + the
  relay's observed-address candidate + a lightweight UDP hole-punch (no help
  against symmetric NAT); the WebRTC path supports standard TURN and so can
  traverse symmetric NAT. See `NAT_TRAVERSAL.md` for the full comparison.
- The QUIC path serves **one active viewer at a time** (a new CWA supersedes
  the previous one); the WebRTC path supports genuinely concurrent browser
  viewers since each has independent DTLS/SRTP keys. See ROADMAP.md §3.
- Windows/Linux backends are tracked in ROADMAP.md as future work.
