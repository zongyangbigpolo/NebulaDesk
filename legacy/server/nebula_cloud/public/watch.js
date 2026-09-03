// watch.js — Nebula Cloud browser viewer (P4 WebRTC path)
//
// Connects to a VDA via the /ws/signaling endpoint (see src/signaling.ts) and
// a real RTCPeerConnection — plain browser APIs, no custom media protocol.
// Mouse/keyboard capture is encoded into the SAME 24-byte NebulaInputEvent
// wire format the native nebula_session client uses (see core/inc/NebulaInput.h),
// sent over the WebRTC DataChannel the VDA creates ("input").

(() => {
  // --- Default ICE servers. Override by setting window.NEBULA_ICE_SERVERS
  // before this script runs (e.g. a small inline <script> in watch.html) if
  // you run your own STUN/TURN (see server/nebula_relay/DEPLOY.md-style TURN
  // notes in this repo's README for why TURN matters for symmetric NATs). ---
  const ICE_SERVERS = window.NEBULA_ICE_SERVERS || [{ urls: 'stun:stun.l.google.com:19302' }];

  const statusEl = () => document.getElementById('watch-status');
  const titleEl = () => document.getElementById('watch-title');
  const videoEl = () => document.getElementById('remote-video');

  function setStatus(text) {
    const el = statusEl();
    if (el) el.textContent = text;
  }

  // --- NebulaInputEvent wire format: 24 bytes, little-endian, packed. -------
  // type:u8 button:u8 modifiers:u16 x:f32 y:f32 wheelX:i32 wheelY:i32 keyCode:u16 reserved:u16
  const InputType = { MouseMove: 1, MouseDown: 2, MouseUp: 3, MouseDrag: 4, Wheel: 5, KeyDown: 6, KeyUp: 7 };
  const MouseButton = { Left: 0, Right: 1, Middle: 2 };
  const Mod = { Shift: 1 << 0, Control: 1 << 1, Option: 1 << 2, Command: 1 << 3, CapsLock: 1 << 4, Fn: 1 << 5 };

  function encodeInputEvent({ type, button = 0, modifiers = 0, x = 0, y = 0, wheelX = 0, wheelY = 0, keyCode = 0 }) {
    const buf = new ArrayBuffer(24);
    const view = new DataView(buf);
    view.setUint8(0, type);
    view.setUint8(1, button);
    view.setUint16(2, modifiers, true);
    view.setFloat32(4, x, true);
    view.setFloat32(8, y, true);
    view.setInt32(12, wheelX, true);
    view.setInt32(16, wheelY, true);
    view.setUint16(20, keyCode, true);
    view.setUint16(22, 0, true); // reserved
    return buf;
  }

  function domModifiersToMask(e) {
    let m = 0;
    if (e.shiftKey) m |= Mod.Shift;
    if (e.ctrlKey) m |= Mod.Control;
    if (e.altKey) m |= Mod.Option;
    if (e.metaKey) m |= Mod.Command;
    return m;
  }

  function domButtonToNebula(button) {
    if (button === 2) return MouseButton.Right;
    if (button === 1) return MouseButton.Middle;
    return MouseButton.Left;
  }

  // KeyboardEvent.code -> macOS Carbon kVK_* virtual keycode. Covers the
  // common US-layout keys; anything not listed is dropped rather than
  // guessed (a wrong keycode is worse than a dropped keypress).
  const CODE_TO_KVK = {
    KeyA: 0x00, KeyS: 0x01, KeyD: 0x02, KeyF: 0x03, KeyH: 0x04, KeyG: 0x05, KeyZ: 0x06, KeyX: 0x07,
    KeyC: 0x08, KeyV: 0x09, KeyB: 0x0b, KeyQ: 0x0c, KeyW: 0x0d, KeyE: 0x0e, KeyR: 0x0f, KeyY: 0x10,
    KeyT: 0x11, Digit1: 0x12, Digit2: 0x13, Digit3: 0x14, Digit4: 0x15, Digit6: 0x16, Digit5: 0x17,
    Equal: 0x18, Digit9: 0x19, Digit7: 0x1a, Minus: 0x1b, Digit8: 0x1c, Digit0: 0x1d, BracketRight: 0x1e,
    KeyO: 0x1f, KeyU: 0x20, BracketLeft: 0x21, KeyI: 0x22, KeyP: 0x23, Enter: 0x24, KeyL: 0x25,
    KeyJ: 0x26, Quote: 0x27, KeyK: 0x28, Semicolon: 0x29, Backslash: 0x2a, Comma: 0x2b, Slash: 0x2c,
    KeyN: 0x2d, KeyM: 0x2e, Period: 0x2f, Tab: 0x30, Space: 0x31, Backquote: 0x32, Backspace: 0x33,
    Escape: 0x35, MetaLeft: 0x37, ShiftLeft: 0x38, CapsLock: 0x39, AltLeft: 0x3a, ControlLeft: 0x3b,
    ShiftRight: 0x3c, AltRight: 0x3d, ControlRight: 0x3e,
    ArrowLeft: 0x7b, ArrowRight: 0x7c, ArrowDown: 0x7d, ArrowUp: 0x7e,
    F1: 0x7a, F2: 0x78, F3: 0x63, F4: 0x76, F5: 0x60, F6: 0x61, F7: 0x62, F8: 0x64,
    F9: 0x65, F10: 0x6d, F11: 0x67, F12: 0x6f, Delete: 0x75,
  };

  class WatchSession {
    constructor(deviceUuid, deviceName) {
      this.deviceUuid = deviceUuid;
      this.deviceName = deviceName;
      this.pc = null;
      this.ws = null;
      this.dataChannel = null;
      this.viewerId = null;
      this.pointerLocked = false;
    }

    async start() {
      titleEl().textContent = this.deviceName || 'Nebula device';
      setStatus('Requesting a session ticket…');

      const connectResp = await fetchApi(`/devices/${this.deviceUuid}/connect`, { method: 'POST' });
      const connect = await readJson(connectResp);
      if (!connectResp.ok) {
        setStatus(`Failed to get a session ticket: ${renderError(connect)}`);
        return;
      }

      const wsProto = window.location.protocol === 'https:' ? 'wss' : 'ws';
      this.ws = new WebSocket(`${wsProto}://${window.location.host}/ws/signaling`);

      this.ws.addEventListener('open', () => {
        setStatus('Signaling connected — waiting for the VDA…');
        this.ws.send(JSON.stringify({
          type: 'hello', role: 'viewer', deviceId: connect.relayDeviceId, token: connect.sessionToken,
        }));
      });
      this.ws.addEventListener('message', (evt) => this.onSignalingMessage(JSON.parse(evt.data)));
      this.ws.addEventListener('close', () => setStatus('Signaling connection closed.'));
      this.ws.addEventListener('error', () => setStatus('Signaling connection error.'));
    }

    ensurePeerConnection() {
      if (this.pc) return;
      this.pc = new RTCPeerConnection({ iceServers: ICE_SERVERS });
      this.pc.ontrack = (evt) => {
        const video = videoEl();
        if (video.srcObject !== evt.streams[0]) video.srcObject = evt.streams[0];
      };
      this.pc.onicecandidate = (evt) => {
        if (!evt.candidate) return;
        this.send({ type: 'ice', candidate: evt.candidate.candidate, mid: evt.candidate.sdpMid });
      };
      this.pc.onconnectionstatechange = () => {
        setStatus(`Connection: ${this.pc.connectionState}`);
      };
      this.pc.ondatachannel = (evt) => {
        this.dataChannel = evt.channel;
        this.dataChannel.binaryType = 'arraybuffer';
        this.dataChannel.onopen = () => this.attachInputCapture();
      };
    }

    async onSignalingMessage(msg) {
      if (msg.type === 'hello-ack') {
        this.viewerId = msg.viewerId;
        return;
      }
      if (msg.type === 'error') {
        setStatus(`Signaling error: ${msg.reason || 'unknown'}`);
        return;
      }
      if (msg.type === 'offer') {
        this.ensurePeerConnection();
        await this.pc.setRemoteDescription({ type: 'offer', sdp: msg.sdp });
        const answer = await this.pc.createAnswer();
        await this.pc.setLocalDescription(answer);
        this.send({ type: 'answer', sdp: this.pc.localDescription.sdp });
        setStatus('Negotiating…');
        return;
      }
      if (msg.type === 'ice' && this.pc) {
        try {
          await this.pc.addIceCandidate({ candidate: msg.candidate, sdpMid: msg.mid });
        } catch (e) {
          console.warn('addIceCandidate failed', e);
        }
        return;
      }
      if (msg.type === 'viewer-leave') {
        setStatus('The VDA ended the session.');
        this.close();
      }
    }

    send(msg) {
      if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(JSON.stringify(msg));
    }

    sendInput(fields) {
      if (this.dataChannel && this.dataChannel.readyState === 'open') {
        this.dataChannel.send(encodeInputEvent(fields));
      }
    }

    // Normalizes a pointer event's position within the <video> element to
    // [0,1] — matches the native CWA's coordinate convention exactly.
    normalizedPos(clientX, clientY) {
      const rect = videoEl().getBoundingClientRect();
      return {
        x: Math.min(1, Math.max(0, (clientX - rect.left) / rect.width)),
        y: Math.min(1, Math.max(0, (clientY - rect.top) / rect.height)),
      };
    }

    attachInputCapture() {
      const video = videoEl();
      video.tabIndex = 0; // make it focusable so key events target it

      video.addEventListener('click', () => video.focus());

      video.addEventListener('mousemove', (e) => {
        const { x, y } = this.normalizedPos(e.clientX, e.clientY);
        const dragging = e.buttons !== 0;
        this.sendInput({
          type: dragging ? InputType.MouseDrag : InputType.MouseMove,
          button: domButtonToNebula(e.button),
          modifiers: domModifiersToMask(e),
          x, y,
        });
      });
      video.addEventListener('mousedown', (e) => {
        const { x, y } = this.normalizedPos(e.clientX, e.clientY);
        this.sendInput({ type: InputType.MouseDown, button: domButtonToNebula(e.button), modifiers: domModifiersToMask(e), x, y });
      });
      video.addEventListener('mouseup', (e) => {
        const { x, y } = this.normalizedPos(e.clientX, e.clientY);
        this.sendInput({ type: InputType.MouseUp, button: domButtonToNebula(e.button), modifiers: domModifiersToMask(e), x, y });
      });
      video.addEventListener('contextmenu', (e) => e.preventDefault());
      video.addEventListener('wheel', (e) => {
        const { x, y } = this.normalizedPos(e.clientX, e.clientY);
        this.sendInput({
          type: InputType.Wheel, modifiers: domModifiersToMask(e),
          x, y, wheelX: Math.round(-e.deltaX / 4), wheelY: Math.round(-e.deltaY / 4),
        });
        e.preventDefault();
      }, { passive: false });

      video.addEventListener('keydown', (e) => {
        const keyCode = CODE_TO_KVK[e.code];
        if (keyCode === undefined) return;
        this.sendInput({ type: InputType.KeyDown, modifiers: domModifiersToMask(e), keyCode });
        e.preventDefault();
      });
      video.addEventListener('keyup', (e) => {
        const keyCode = CODE_TO_KVK[e.code];
        if (keyCode === undefined) return;
        this.sendInput({ type: InputType.KeyUp, modifiers: domModifiersToMask(e), keyCode });
        e.preventDefault();
      });

      setStatus('Connected — click the video to send mouse/keyboard input.');
    }

    close() {
      if (this.dataChannel) this.dataChannel.close();
      if (this.pc) this.pc.close();
      if (this.ws) this.ws.close();
    }
  }

  window.addEventListener('DOMContentLoaded', async () => {
    if (!getAccessToken() && !(await refreshTokens())) {
      window.location.href = '/';
      return;
    }
    const params = new URLSearchParams(window.location.search);
    const deviceUuid = params.get('device');
    const deviceName = params.get('name');
    if (!deviceUuid) {
      setStatus('No device specified. Go back to the dashboard and click Connect on a device.');
      return;
    }
    const session = new WatchSession(deviceUuid, deviceName);
    window.addEventListener('beforeunload', () => session.close());
    await session.start();
  });
})();
