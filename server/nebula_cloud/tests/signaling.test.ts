import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import WebSocket from 'ws';

import { buildApp } from '../src/app';
import { AppConfig } from '../src/config';
import { InMemoryDataStore } from '../src/repositories/in-memory-store';

const baseConfig: AppConfig = {
  PORT: 0,
  HOST: '127.0.0.1',
  DATABASE_URL: 'postgresql://unused',
  JWT_ACCESS_SECRET: 'access-secret-1234567890',
  JWT_REFRESH_SECRET: 'refresh-secret-1234567890',
  JWT_SESSION_SECRET: 'session-secret-1234567890',
  ACCESS_TOKEN_TTL_SECONDS: 900,
  REFRESH_TOKEN_TTL_SECONDS: 60 * 60 * 24,
  SESSION_TOKEN_TTL_SECONDS: 60,
  CLAIM_CODE_TTL_SECONDS: 3600,
  DEVICE_ONLINE_WINDOW_SECONDS: 120,
  RELAY_PUBLIC_HOST: 'relay.nebula.test',
  RELAY_PUBLIC_PORT: 7100,
  RELAY_SHARED_SECRET: 'relay-shared-secret-1234',
};

// Waits for exactly one JSON message from `ws`, optionally filtered by type.
function nextMessage(ws: WebSocket, type?: string): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => reject(new Error(`timed out waiting for message type=${type}`)), 5000);
    const handler = (raw: WebSocket.RawData) => {
      const msg = JSON.parse(raw.toString());
      if (type && msg.type !== type) return; // keep waiting
      clearTimeout(timeout);
      ws.off('message', handler);
      resolve(msg);
    };
    ws.on('message', handler);
  });
}

function waitOpen(ws: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    ws.once('open', () => resolve());
    ws.once('error', reject);
  });
}

describe('Nebula Cloud WebRTC signaling', () => {
  let store: InMemoryDataStore;

  beforeEach(() => {
    store = new InMemoryDataStore();
  });

  afterEach(() => {
    store = new InMemoryDataStore();
  });

  it('routes hello/viewer-join/offer/answer/ice/viewer-leave between a VDA and a viewer', async () => {
    const app = await buildApp({ config: baseConfig, dataStore: store });
    const address = await app.listen({ port: 0, host: '127.0.0.1' });
    const wsBase = address.replace('http://', 'ws://');

    try {
      // --- Set up an owner account + device via the normal HTTP API ---
      const register = await app.inject({
        method: 'POST',
        url: '/auth/register',
        payload: { email: 'owner@example.com', password: 'super-secret-pass', displayName: 'Owner' },
      });
      expect(register.statusCode).toBe(201);

      const login = await app.inject({
        method: 'POST',
        url: '/auth/login',
        payload: { email: 'owner@example.com', password: 'super-secret-pass' },
      });
      expect(login.statusCode).toBe(200);
      const { accessToken } = login.json();

      const deviceResp = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: { authorization: `Bearer ${accessToken}` },
        payload: { name: 'Signaling Test Mac' },
      });
      expect(deviceResp.statusCode).toBe(201);
      const { device, registration } = deviceResp.json();

      const connectResp = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${accessToken}` },
      });
      expect(connectResp.statusCode).toBe(200);
      const { sessionToken } = connectResp.json();

      // --- VDA connects and registers ---
      const vdaWs = new WebSocket(`${wsBase}/ws/signaling`);
      await waitOpen(vdaWs);
      vdaWs.send(JSON.stringify({
        type: 'hello', role: 'vda', deviceId: registration.relayDeviceId, token: registration.relayToken,
      }));

      // --- Viewer connects and registers ---
      const viewerWs = new WebSocket(`${wsBase}/ws/signaling`);
      await waitOpen(viewerWs);
      viewerWs.send(JSON.stringify({
        type: 'hello', role: 'viewer', deviceId: registration.relayDeviceId, token: sessionToken,
      }));

      const helloAck = await nextMessage(viewerWs, 'hello-ack');
      const viewerId = helloAck.viewerId as string;
      expect(viewerId).toBeTypeOf('string');

      const viewerJoin = await nextMessage(vdaWs, 'viewer-join');
      expect(viewerJoin.viewerId).toBe(viewerId);

      // --- VDA sends an offer; viewer must receive it tagged with viewerId ---
      vdaWs.send(JSON.stringify({ type: 'offer', viewerId, sdp: 'v=0...FAKE-OFFER' }));
      const offer = await nextMessage(viewerWs, 'offer');
      expect(offer.sdp).toBe('v=0...FAKE-OFFER');

      // --- Viewer answers; VDA must receive it (server stamps the real viewerId) ---
      viewerWs.send(JSON.stringify({ type: 'answer', sdp: 'v=0...FAKE-ANSWER' }));
      const answer = await nextMessage(vdaWs, 'answer');
      expect(answer.sdp).toBe('v=0...FAKE-ANSWER');
      expect(answer.viewerId).toBe(viewerId);

      // --- Trickle ICE both directions ---
      vdaWs.send(JSON.stringify({ type: 'ice', viewerId, candidate: 'candidate:from-vda', mid: '0' }));
      const iceToViewer = await nextMessage(viewerWs, 'ice');
      expect(iceToViewer.candidate).toBe('candidate:from-vda');

      viewerWs.send(JSON.stringify({ type: 'ice', candidate: 'candidate:from-viewer', mid: '0' }));
      const iceToVda = await nextMessage(vdaWs, 'ice');
      expect(iceToVda.candidate).toBe('candidate:from-viewer');
      expect(iceToVda.viewerId).toBe(viewerId);

      // --- Viewer leaving must notify the VDA ---
      viewerWs.send(JSON.stringify({ type: 'viewer-leave' }));
      const leaveMsg = await nextMessage(vdaWs, 'viewer-leave');
      expect(leaveMsg.viewerId).toBe(viewerId);

      vdaWs.close();
    } finally {
      await app.close();
    }
  });

  it('rejects a viewer hello with an invalid/expired session token', async () => {
    const app = await buildApp({ config: baseConfig, dataStore: store });
    const address = await app.listen({ port: 0, host: '127.0.0.1' });
    const wsBase = address.replace('http://', 'ws://');

    try {
      const viewerWs = new WebSocket(`${wsBase}/ws/signaling`);
      await waitOpen(viewerWs);
      viewerWs.send(JSON.stringify({
        type: 'hello', role: 'viewer', deviceId: 'nebula-does-not-exist', token: 'not-a-real-jwt',
      }));
      const error = await nextMessage(viewerWs, 'error');
      expect(error.reason).toBeTypeOf('string');
    } finally {
      await app.close();
    }
  });

  it('rejects a VDA hello with the wrong relay token', async () => {
    const app = await buildApp({ config: baseConfig, dataStore: store });
    const address = await app.listen({ port: 0, host: '127.0.0.1' });
    const wsBase = address.replace('http://', 'ws://');

    try {
      const login = await (async () => {
        await app.inject({
          method: 'POST',
          url: '/auth/register',
          payload: { email: 'owner2@example.com', password: 'super-secret-pass', displayName: 'Owner2' },
        });
        const r = await app.inject({
          method: 'POST',
          url: '/auth/login',
          payload: { email: 'owner2@example.com', password: 'super-secret-pass' },
        });
        return r.json();
      })();

      const deviceResp = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: { authorization: `Bearer ${login.accessToken}` },
        payload: { name: 'Another Mac' },
      });
      const { registration } = deviceResp.json();

      const vdaWs = new WebSocket(`${wsBase}/ws/signaling`);
      await waitOpen(vdaWs);
      vdaWs.send(JSON.stringify({
        type: 'hello', role: 'vda', deviceId: registration.relayDeviceId, token: 'totally-wrong-token',
      }));
      const error = await nextMessage(vdaWs, 'error');
      expect(error.reason).toBe('invalid device credentials');
    } finally {
      await app.close();
    }
  });
});
