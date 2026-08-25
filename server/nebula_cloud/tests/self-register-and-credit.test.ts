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
  INITIAL_ADMIN_EMAILS: ['admin@example.com'],
  DEVICE_ENROLLMENT_TOKEN: 'enroll-secret-1234',
  DEFAULT_TRIAL_CREDIT_SECONDS: 600,
  CONNECT_CREDIT_COST_SECONDS: 600,
};

describe('Nebula Cloud self-registration & trial credit', () => {
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
      payload: { email: input.email, password, displayName: input.displayName },
    });
    expect(registerResponse.statusCode).toBe(201);

    const loginResponse = await app.inject({
      method: 'POST',
      url: '/auth/login',
      payload: { email: input.email, password },
    });
    expect(loginResponse.statusCode).toBe(200);
    return loginResponse.json();
  }

  function authHeader(accessToken: string) {
    return { authorization: `Bearer ${accessToken}` };
  }

  it('registers a new account with default trial credit, visible via /auth/me and /auth/register', async () => {
    const app = await createApp();
    try {
      const registerResponse = await app.inject({
        method: 'POST',
        url: '/auth/register',
        payload: { email: 'newbie@example.com', password: 'super-secret-pass', displayName: 'Newbie' },
      });
      expect(registerResponse.statusCode).toBe(201);
      expect(registerResponse.json().user.creditSeconds).toBe(600);

      const login = await app.inject({
        method: 'POST',
        url: '/auth/login',
        payload: { email: 'newbie@example.com', password: 'super-secret-pass' },
      });
      const me = await app.inject({ method: 'GET', url: '/auth/me', headers: authHeader(login.json().accessToken) });
      expect(me.statusCode).toBe(200);
      expect(me.json().user.creditSeconds).toBe(600);
    } finally {
      await app.close();
    }
  });

  it('lets a logged-in user self-register a device with the shared enrollment token, no admin action needed', async () => {
    const app = await createApp();
    try {
      const user = await registerAndLogin(app, { email: 'solo@example.com', displayName: 'Solo User' });

      const wrongToken = await app.inject({
        method: 'POST',
        url: '/devices/self-register',
        headers: authHeader(user.accessToken),
        payload: { name: 'My Mac mini', enrollmentToken: 'not-the-right-token' },
      });
      expect(wrongToken.statusCode).toBe(401);

      const registered = await app.inject({
        method: 'POST',
        url: '/devices/self-register',
        headers: authHeader(user.accessToken),
        payload: { name: 'My Mac mini', enrollmentToken: baseConfig.DEVICE_ENROLLMENT_TOKEN },
      });
      expect(registered.statusCode).toBe(201);
      const body = registered.json();
      expect(body.device.name).toBe('My Mac mini');
      expect(body.registration.relayToken).toBeTypeOf('string');
      expect(body.registration.psk).toBeTypeOf('string');
      // No claim code in the self-register response — the caller is already
      // authenticated, so there's nothing left to redeem.
      expect(body.registration.claimCode).toBeUndefined();

      // The registering user is immediately the owner and sees it in their list.
      const devices = await app.inject({ method: 'GET', url: '/devices', headers: authHeader(user.accessToken) });
      const listed = devices.json().devices.find((d: { id: string }) => d.id === body.device.id);
      expect(listed).toBeDefined();
      expect(listed.role).toBe('OWNER');
    } finally {
      await app.close();
    }
  });

  it('refuses self-registration when DEVICE_ENROLLMENT_TOKEN is unset (disabled by default)', async () => {
    const app = await createApp({ DEVICE_ENROLLMENT_TOKEN: '' });
    try {
      const user = await registerAndLogin(app, { email: 'solo@example.com', displayName: 'Solo User' });
      const attempt = await app.inject({
        method: 'POST',
        url: '/devices/self-register',
        headers: authHeader(user.accessToken),
        payload: { name: 'My Mac mini', enrollmentToken: 'anything' },
      });
      expect(attempt.statusCode).toBe(403);
    } finally {
      await app.close();
    }
  });

  it('deducts a flat credit cost per connect and denies once the balance runs out, admin tops it up', async () => {
    const app = await createApp({ DEFAULT_TRIAL_CREDIT_SECONDS: 600, CONNECT_CREDIT_COST_SECONDS: 600 });
    try {
      const admin = await registerAndLogin(app, { email: 'admin@example.com', displayName: 'Admin' });
      const user = await registerAndLogin(app, { email: 'trial@example.com', displayName: 'Trial User' });

      const registered = await app.inject({
        method: 'POST',
        url: '/devices/self-register',
        headers: authHeader(user.accessToken),
        payload: { name: 'Trial Mac', enrollmentToken: baseConfig.DEVICE_ENROLLMENT_TOKEN },
      });
      const deviceId = registered.json().device.id;

      // First connect succeeds and spends the entire 600s starting balance.
      const firstConnect = await app.inject({
        method: 'POST',
        url: `/devices/${deviceId}/connect`,
        headers: authHeader(user.accessToken),
      });
      expect(firstConnect.statusCode).toBe(200);
      expect(firstConnect.json().creditSecondsRemaining).toBe(0);

      // Second connect is denied: insufficient credit.
      const secondConnect = await app.inject({
        method: 'POST',
        url: `/devices/${deviceId}/connect`,
        headers: authHeader(user.accessToken),
      });
      expect(secondConnect.statusCode).toBe(402);
      expect(secondConnect.json().error).toBe('insufficient_credit');

      // A non-admin can't top up.
      const forbiddenTopUp = await app.inject({
        method: 'PATCH',
        url: `/admin/users/${user.user.userId}/credit`,
        headers: authHeader(user.accessToken),
        payload: { addSeconds: 600 },
      });
      expect(forbiddenTopUp.statusCode).toBe(403);

      // Admin tops up, connect works again.
      const topUp = await app.inject({
        method: 'PATCH',
        url: `/admin/users/${user.user.userId}/credit`,
        headers: authHeader(admin.accessToken),
        payload: { addSeconds: 600 },
      });
      expect(topUp.statusCode).toBe(200);
      expect(topUp.json().user.creditSeconds).toBe(600);

      const thirdConnect = await app.inject({
        method: 'POST',
        url: `/devices/${deviceId}/connect`,
        headers: authHeader(user.accessToken),
      });
      expect(thirdConnect.statusCode).toBe(200);
      expect(thirdConnect.json().creditSecondsRemaining).toBe(0);
    } finally {
      await app.close();
    }
  });

  it('never meters admin connects, regardless of their credit balance', async () => {
    const app = await createApp();
    try {
      const admin = await registerAndLogin(app, { email: 'admin@example.com', displayName: 'Admin' });
      const owner = await registerAndLogin(app, { email: 'owner@example.com', displayName: 'Owner' });

      const createDevice = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: authHeader(owner.accessToken),
        payload: { name: 'Owner Mac' },
      });
      const deviceId = createDevice.json().device.id;

      // Admin connects repeatedly — never denied, never decremented.
      for (let i = 0; i < 3; i++) {
        const connect = await app.inject({
          method: 'POST',
          url: `/devices/${deviceId}/connect`,
          headers: authHeader(admin.accessToken),
        });
        expect(connect.statusCode).toBe(200);
        expect(connect.json().role).toBe('ADMIN');
      }

      const me = await app.inject({ method: 'GET', url: '/auth/me', headers: authHeader(admin.accessToken) });
      expect(me.json().user.creditSeconds).toBe(600); // untouched
    } finally {
      await app.close();
    }
  });
});
