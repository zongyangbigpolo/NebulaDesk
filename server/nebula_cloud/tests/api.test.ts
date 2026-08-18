import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import { buildApp } from '../src/app';
import { AppConfig } from '../src/config';
import { InMemoryDataStore } from '../src/repositories/in-memory-store';

const baseConfig: AppConfig = {
  PORT: 4000,
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

describe('Nebula Cloud API', () => {
  let store: InMemoryDataStore;

  beforeEach(() => {
    store = new InMemoryDataStore();
  });

  afterEach(() => {
    store = new InMemoryDataStore();
  });

  async function createApp(configOverrides: Partial<AppConfig> = {}) {
    return buildApp({
      config: { ...baseConfig, ...configOverrides },
      dataStore: store,
    });
  }

  async function registerAndLogin(app: Awaited<ReturnType<typeof createApp>>, input: {
    email: string;
    password?: string;
    displayName: string;
  }) {
    const password = input.password ?? 'super-secret-pass';
    const registerResponse = await app.inject({
      method: 'POST',
      url: '/auth/register',
      payload: {
        email: input.email,
        password,
        displayName: input.displayName,
      },
    });
    expect(registerResponse.statusCode).toBe(201);

    const loginResponse = await app.inject({
      method: 'POST',
      url: '/auth/login',
      payload: {
        email: input.email,
        password,
      },
    });
    expect(loginResponse.statusCode).toBe(200);
    return loginResponse.json();
  }

  it('registers and logs in a user, then refreshes and logs out', async () => {
    const app = await createApp();
    try {
      const login = await registerAndLogin(app, {
        email: 'owner@example.com',
        displayName: 'Owner',
      });

      expect(login.accessToken).toBeTypeOf('string');
      expect(login.refreshToken).toBeTypeOf('string');
      expect(login.user.email).toBe('owner@example.com');

      const refreshResponse = await app.inject({
        method: 'POST',
        url: '/auth/refresh',
        payload: { refreshToken: login.refreshToken },
      });
      expect(refreshResponse.statusCode).toBe(200);
      const refreshed = refreshResponse.json();
      expect(refreshed.accessToken).not.toBe(login.accessToken);
      expect(refreshed.refreshToken).not.toBe(login.refreshToken);

      const logoutResponse = await app.inject({
        method: 'POST',
        url: '/auth/logout',
        payload: { refreshToken: refreshed.refreshToken },
      });
      expect(logoutResponse.statusCode).toBe(200);

      const refreshAfterLogout = await app.inject({
        method: 'POST',
        url: '/auth/refresh',
        payload: { refreshToken: refreshed.refreshToken },
      });
      expect(refreshAfterLogout.statusCode).toBe(401);
    } finally {
      await app.close();
    }
  });

  it('creates a device and redeems its claim code by rotating the relay token', async () => {
    const app = await createApp();
    try {
      const owner = await registerAndLogin(app, {
        email: 'owner@example.com',
        displayName: 'Owner',
      });

      const createDevice = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: { authorization: `Bearer ${owner.accessToken}` },
        payload: { name: 'Mac mini #1' },
      });
      expect(createDevice.statusCode).toBe(201);
      const created = createDevice.json();
      expect(created.registration.relayToken).toBeTypeOf('string');
      expect(created.registration.claimCode).toMatch(/^NEB-/);
      expect(created.registration.vdaCommand).toContain('--relay relay.nebula.test --relay-port 7100');

      const claim = await app.inject({
        method: 'POST',
        url: '/device-claims/redeem',
        payload: { claimCode: created.registration.claimCode },
      });
      expect(claim.statusCode).toBe(200);
      const redeemed = claim.json();
      expect(redeemed.device.relayDeviceId).toBe(created.device.relayDeviceId);
      expect(redeemed.registration.relayToken).not.toBe(created.registration.relayToken);

      const secondClaim = await app.inject({
        method: 'POST',
        url: '/device-claims/redeem',
        payload: { claimCode: created.registration.claimCode },
      });
      expect(secondClaim.statusCode).toBe(404);
    } finally {
      await app.close();
    }
  });

  it('supports grant creation, shared visibility, connect authorization, and revocation', async () => {
    const app = await createApp();
    try {
      const owner = await registerAndLogin(app, {
        email: 'owner@example.com',
        displayName: 'Owner',
      });
      const viewer = await registerAndLogin(app, {
        email: 'viewer@example.com',
        displayName: 'Viewer',
      });
      const outsider = await registerAndLogin(app, {
        email: 'outsider@example.com',
        displayName: 'Outsider',
      });

      const deviceResponse = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: { authorization: `Bearer ${owner.accessToken}` },
        payload: { name: 'Shared Mac' },
      });
      const device = deviceResponse.json().device;

      const grantResponse = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/grants`,
        headers: { authorization: `Bearer ${owner.accessToken}` },
        payload: { granteeEmail: 'viewer@example.com', role: 'VIEWER' },
      });
      expect(grantResponse.statusCode).toBe(201);
      const grant = grantResponse.json().grant;

      const viewerDevices = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: { authorization: `Bearer ${viewer.accessToken}` },
      });
      expect(viewerDevices.statusCode).toBe(200);
      expect(viewerDevices.json().devices).toEqual(
        expect.arrayContaining([
          expect.objectContaining({ id: device.id, role: 'VIEWER' }),
        ]),
      );

      const ownerConnect = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${owner.accessToken}` },
      });
      expect(ownerConnect.statusCode).toBe(200);
      expect(ownerConnect.json().sessionToken).toBeTypeOf('string');

      const viewerConnect = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${viewer.accessToken}` },
      });
      expect(viewerConnect.statusCode).toBe(200);

      const outsiderConnect = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${outsider.accessToken}` },
      });
      expect(outsiderConnect.statusCode).toBe(404);

      const revokeResponse = await app.inject({
        method: 'DELETE',
        url: `/grants/${grant.id}`,
        headers: { authorization: `Bearer ${owner.accessToken}` },
      });
      expect(revokeResponse.statusCode).toBe(200);

      const viewerConnectAfterRevoke = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${viewer.accessToken}` },
      });
      expect(viewerConnectAfterRevoke.statusCode).toBe(404);
    } finally {
      await app.close();
    }
  });

  it('authorizes relay callbacks with live DB checks and correct relay secret', async () => {
    const app = await createApp();
    try {
      const owner = await registerAndLogin(app, {
        email: 'owner@example.com',
        displayName: 'Owner',
      });
      const viewer = await registerAndLogin(app, {
        email: 'viewer@example.com',
        displayName: 'Viewer',
      });

      const deviceResponse = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: { authorization: `Bearer ${owner.accessToken}` },
        payload: { name: 'Relay Target' },
      });
      const device = deviceResponse.json().device;

      const grantResponse = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/grants`,
        headers: { authorization: `Bearer ${owner.accessToken}` },
        payload: { granteeEmail: 'viewer@example.com', role: 'CONTROLLER' },
      });
      const grantId = grantResponse.json().grant.id;

      const connectResponse = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${viewer.accessToken}` },
      });
      const connectPayload = connectResponse.json();

      const wrongSecret = await app.inject({
        method: 'POST',
        url: '/internal/authorize',
        headers: { 'x-relay-secret': 'wrong-secret' },
        payload: {
          deviceId: connectPayload.relayDeviceId,
          token: connectPayload.sessionToken,
        },
      });
      expect(wrongSecret.statusCode).toBe(401);

      const authorized = await app.inject({
        method: 'POST',
        url: '/internal/authorize',
        headers: { 'x-relay-secret': baseConfig.RELAY_SHARED_SECRET },
        payload: {
          deviceId: connectPayload.relayDeviceId,
          token: connectPayload.sessionToken,
        },
      });
      expect(authorized.statusCode).toBe(200);
      expect(authorized.json()).toEqual({ authorized: true });

      const reuse = await app.inject({
        method: 'POST',
        url: '/internal/authorize',
        headers: { 'x-relay-secret': baseConfig.RELAY_SHARED_SECRET },
        payload: {
          deviceId: connectPayload.relayDeviceId,
          token: connectPayload.sessionToken,
        },
      });
      expect(reuse.statusCode).toBe(200);
      expect(reuse.json().authorized).toBe(false);

      const secondConnect = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${viewer.accessToken}` },
      });
      const secondPayload = secondConnect.json();

      await app.inject({
        method: 'DELETE',
        url: `/grants/${grantId}`,
        headers: { authorization: `Bearer ${owner.accessToken}` },
      });

      const revoked = await app.inject({
        method: 'POST',
        url: '/internal/authorize',
        headers: { 'x-relay-secret': baseConfig.RELAY_SHARED_SECRET },
        payload: {
          deviceId: secondPayload.relayDeviceId,
          token: secondPayload.sessionToken,
        },
      });
      expect(revoked.statusCode).toBe(200);
      expect(revoked.json().authorized).toBe(false);
      expect(revoked.json().reason).toContain('no longer authorized');
    } finally {
      await app.close();
    }
  });

  it('returns authorized:false for expired relay session tokens', async () => {
    const app = await createApp({ SESSION_TOKEN_TTL_SECONDS: 1 });
    try {
      const owner = await registerAndLogin(app, {
        email: 'owner@example.com',
        displayName: 'Owner',
      });

      const deviceResponse = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: { authorization: `Bearer ${owner.accessToken}` },
        payload: { name: 'Expiring Mac' },
      });
      const device = deviceResponse.json().device;

      const connectResponse = await app.inject({
        method: 'POST',
        url: `/devices/${device.id}/connect`,
        headers: { authorization: `Bearer ${owner.accessToken}` },
      });
      const connectPayload = connectResponse.json();

      await new Promise((resolve) => setTimeout(resolve, 1100));

      const authorizeResponse = await app.inject({
        method: 'POST',
        url: '/internal/authorize',
        headers: { 'x-relay-secret': baseConfig.RELAY_SHARED_SECRET },
        payload: {
          deviceId: connectPayload.relayDeviceId,
          token: connectPayload.sessionToken,
        },
      });
      expect(authorizeResponse.statusCode).toBe(200);
      expect(authorizeResponse.json().authorized).toBe(false);
    } finally {
      await app.close();
    }
  });
});
