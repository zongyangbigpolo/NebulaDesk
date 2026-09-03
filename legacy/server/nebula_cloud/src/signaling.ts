import crypto from 'node:crypto';

import { FastifyInstance } from 'fastify';
import type { WebSocket } from 'ws';

import { AppConfig } from './config';
import { ConnectService } from './services/connect-service';
import { DeviceService } from './services/device-service';

// WebRTC signaling relay (P4 — see ROADMAP.md §12 in the main repo).
// Pure message pass-through: this never sees media, only small JSON blobs
// (SDP offer/answer + trickle ICE candidates). Authorization piggybacks on
// the same credentials the QUIC relay path already uses:
//   - a VDA's "hello" presents its long-lived relay token (DeviceService.verifyRelayToken)
//   - a viewer's "hello" presents the short-lived session JWT from
//     POST /devices/:id/connect (ConnectService.authorizeRelay, called
//     in-process — no actual HTTP round trip needed since we're already
//     inside the same server).
//
// Wire format (see core/inc/WebRtcGateway.h for the native VDA side):
//   -> {"type":"hello","role":"vda"|"viewer","deviceId":"...","token":"..."}
//   VDA  <- {"type":"viewer-join","viewerId":"..."}
//   VDA  -> {"type":"offer","viewerId":"...","sdp":"..."}
//   both <-> {"type":"ice","viewerId":"...","candidate":"...","mid":"..."}
//   viewer -> {"type":"answer","viewerId":"...","sdp":"..."}
//   either -> {"type":"viewer-leave","viewerId":"..."}

interface VdaConnection {
  socket: WebSocket;
  relayDeviceId: string;
  viewerIds: Set<string>;
}

interface ViewerConnection {
  socket: WebSocket;
  relayDeviceId: string;
  viewerId: string;
}

export interface RegisterSignalingOptions {
  config: Pick<AppConfig, 'RELAY_SHARED_SECRET'>;
  deviceService: DeviceService;
  connectService: ConnectService;
}

export function registerSignaling(app: FastifyInstance, options: RegisterSignalingOptions): void {
  const { config, deviceService, connectService } = options;

  // One VDA connection per relayDeviceId; any number of concurrent viewers
  // (this is genuinely concurrent, unlike the QUIC relay path — see
  // ROADMAP.md §3/§12 for why that distinction is safe here).
  const vdaByDevice = new Map<string, VdaConnection>();
  const viewersById = new Map<string, ViewerConnection>();

  function send(socket: WebSocket, msg: Record<string, unknown>): void {
    if (socket.readyState === socket.OPEN) socket.send(JSON.stringify(msg));
  }

  function dropViewer(viewerId: string, notifyVda: boolean): void {
    const viewer = viewersById.get(viewerId);
    if (!viewer) return;
    viewersById.delete(viewerId);
    const vda = vdaByDevice.get(viewer.relayDeviceId);
    if (vda) {
      vda.viewerIds.delete(viewerId);
      if (notifyVda) send(vda.socket, { type: 'viewer-leave', viewerId });
    }
  }

  function dropVda(relayDeviceId: string): void {
    const vda = vdaByDevice.get(relayDeviceId);
    if (!vda) return;
    vdaByDevice.delete(relayDeviceId);
    for (const viewerId of vda.viewerIds) {
      const viewer = viewersById.get(viewerId);
      if (viewer) {
        send(viewer.socket, { type: 'viewer-leave', viewerId });
        viewersById.delete(viewerId);
      }
    }
  }

  app.get('/ws/signaling', { websocket: true }, (socket, request) => {
    let role: 'vda' | 'viewer' | null = null;
    let relayDeviceId: string | null = null;
    let viewerId: string | null = null;
    let helloReceived = false;

    socket.on('message', (raw: Buffer) => {
      let msg: Record<string, unknown>;
      try {
        msg = JSON.parse(raw.toString());
      } catch {
        send(socket, { type: 'error', reason: 'malformed JSON' });
        return;
      }

      if (!helloReceived) {
        void (async () => {
          if (msg.type !== 'hello' || (msg.role !== 'vda' && msg.role !== 'viewer')) {
            send(socket, { type: 'error', reason: 'first message must be a hello' });
            socket.close();
            return;
          }
          const deviceId = String(msg.deviceId ?? '');
          const token = String(msg.token ?? '');

          if (msg.role === 'vda') {
            const device = await deviceService.verifyRelayToken(deviceId, token);
            if (!device) {
              send(socket, { type: 'error', reason: 'invalid device credentials' });
              socket.close();
              return;
            }
            // A new VDA connection replaces any stale one for the same device
            // (e.g. the VDA process restarted without a clean disconnect).
            dropVda(deviceId);
            role = 'vda';
            relayDeviceId = deviceId;
            vdaByDevice.set(deviceId, { socket, relayDeviceId: deviceId, viewerIds: new Set() });
            helloReceived = true;
            void deviceService.heartbeat(device.id).catch(() => {});
            request.log?.info?.({ deviceId }, 'signaling: VDA registered');
          } else {
            const result = await connectService.authorizeRelay({
              relaySecret: config.RELAY_SHARED_SECRET, // in-process call, we ARE the trusted verifier
              relayDeviceId: deviceId,
              sessionToken: token,
              sourceIp: request.ip,
            });
            if (!result.body.authorized) {
              send(socket, { type: 'error', reason: result.body.reason ?? 'not authorized' });
              socket.close();
              return;
            }
            const vda = vdaByDevice.get(deviceId);
            if (!vda) {
              send(socket, { type: 'error', reason: 'device is not online' });
              socket.close();
              return;
            }
            role = 'viewer';
            relayDeviceId = deviceId;
            viewerId = crypto.randomUUID();
            viewersById.set(viewerId, { socket, relayDeviceId: deviceId, viewerId });
            vda.viewerIds.add(viewerId);
            helloReceived = true;
            send(socket, { type: 'hello-ack', viewerId });
            send(vda.socket, { type: 'viewer-join', viewerId });
            request.log?.info?.({ deviceId, viewerId }, 'signaling: viewer joined');
          }
        })();
        return;
      }

      // Post-hello: route by role.
      if (role === 'vda' && relayDeviceId) {
        const targetViewerId = String(msg.viewerId ?? '');
        const viewer = viewersById.get(targetViewerId);
        if (viewer && viewer.relayDeviceId === relayDeviceId) send(viewer.socket, msg);
      } else if (role === 'viewer' && relayDeviceId && viewerId) {
        msg.viewerId = viewerId; // never trust a client-supplied viewerId for this direction
        if (msg.type === 'viewer-leave') {
          dropViewer(viewerId, true);
          socket.close();
          return;
        }
        const vda = vdaByDevice.get(relayDeviceId);
        if (vda) send(vda.socket, msg);
      }
    });

    socket.on('close', () => {
      if (role === 'vda' && relayDeviceId) dropVda(relayDeviceId);
      else if (role === 'viewer' && viewerId) dropViewer(viewerId, true);
    });
  });
}
