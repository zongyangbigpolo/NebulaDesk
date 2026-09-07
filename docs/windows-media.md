# Windows native media

Requires Windows 11, a signed-in interactive desktop, a physical/virtual display,
and a current Intel, NVIDIA, or AMD driver with Media Foundation H.264 encoding
and D3D11VA H.264 decoding. Windows N also requires the Media Feature Pack.
Windows SDK and MSVC C++ tools are needed to build; the normal workspace build
bundles Opus and does not require FFmpeg or an external codec process.

The agent captures the primary display with Windows Graphics Capture (WGC).
D3D11 scales to the requested bounding rectangle without changing aspect ratio,
converts BGRA to limited-range BT.709 NV12, and feeds an asynchronous hardware
Media Foundation encoder. Software encoders are never enumerated. Low-latency
mode, zero B frames, and keyframe control are required driver capabilities;
unsupported drivers fail startup explicitly. Encoded slices are also checked
to reject a driver that emits B frames anyway.
Startup succeeds only after the first real IDR reaches the bounded frame sink;
missing capture frames, stalled encoder input/output, and failed IDR recovery
produce explicit errors rather than leaving a connected black session.

Every IDR carries SPS/PPS followed by four-byte AVCC NAL lengths, matching macOS.
Raw capture queues and in-flight encoder samples are bounded. A full network
queue invalidates the reference chain: subsequent P frames are suppressed until
an IDR can be sent. The last real WGC frame is retained so a static desktop can
still answer a keyframe request. No synthetic frames are substituted.

The client uses Microsoft's H.264 MFT with a hardware D3D11 device manager.
It never clears the manager to retry in software; every decoded sample must
contain a DXGI surface. CPU-only output is rejected. The existing renderer API
requires owned Y/U/V planes, so decoding performs a GPU readback before wgpu
upload. Capture/encode stays GPU-resident; decode/render is **not zero-copy**.

WASAPI captures the default playback endpoint using shared-mode system
loopback. The Windows audio engine remixes/resamples to 48 kHz float mono/stereo;
the existing Opus encoder emits 20 ms packets with FEC. A disabled or removed
endpoint is an explicit error. Silence reported by WASAPI is preserved.
Changing the default device requires reconnecting.

Input uses physical USB HID-to-PC-scan-code mapping, including extended keys,
or UTF-16 `SendInput` for resolved text. Normalized positions map to the same
primary display; wheel deltas retain sub-tick remainders. Held remote keys and
buttons are released when the injector drops. Clipboard uses `SystemClipboard`
and its native Win32 implementation, not a per-session memory clipboard.

## Operator limitations

- Run in the signed-in user's session, not a Session 0 service. Unlock the
  desktop and allow screen capture in Windows privacy settings.
- Secure desktops/UAC prompts and protected content cannot be captured.
  UIPI prevents a non-elevated agent from injecting into elevated applications.
- Only primary-display desktop capture is exposed by the current media API.
  A display resize/removal ends capture with a reconnect diagnostic rather than
  submitting wrongly sized textures to the encoder.
- COM initialization, MF startup, D3D/WGC/MF/WASAPI objects, and teardown stay on
  dedicated MTA workers. Stop signals and joins the worker; no worker is detached.
- Hosted Windows CI can compile and run pure tests but normally has no usable
  interactive GPU capture/audio device. Real capture/encode/decode, vendor-driver
  compatibility, and cross-platform audiovisual latency need a Windows GPU host.

Build: `cargo build -p nebula-agent -p nebula-client`.
Pure checks: `cargo test -p nebula-agent --lib platform::windows`.
Use the workspace capture/client media probes on an interactive GPU host; do not
treat passing headless compilation as a successful hardware-media smoke test.
