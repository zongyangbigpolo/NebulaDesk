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
  CONNECT_CREDIT_COST_SECONDS: 0, // neutralize credit metering for tests unrelated to it
};

describe('Nebula Cloud groups & admin role', () => {
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

  it('bootstraps the first admin via INITIAL_ADMIN_EMAILS and lets them promote others', async () => {
    const app = await createApp();
    try {
      const admin = await registerAndLogin(app, { email: 'admin@example.com', displayName: 'Admin' });
      expect(admin.user.role).toBe('ADMIN');

      const regular = await registerAndLogin(app, { email: 'regular@example.com', displayName: 'Regular' });
      expect(regular.user.role).toBe('USER');

      // A non-admin can't promote themselves.
      const selfPromote = await app.inject({
        method: 'PATCH',
        url: `/admin/users/${regular.user.userId}/role`,
        headers: authHeader(regular.accessToken),
        payload: { role: 'ADMIN' },
      });
      expect(selfPromote.statusCode).toBe(403);

      // The admin can promote the regular user.
      const promote = await app.inject({
        method: 'PATCH',
        url: `/admin/users/${regular.user.userId}/role`,
        headers: authHeader(admin.accessToken),
        payload: { role: 'ADMIN' },
      });
      expect(promote.statusCode).toBe(200);
      expect(promote.json().user.role).toBe('ADMIN');

      // An admin can't demote themselves (must ask another admin).
      const selfDemote = await app.inject({
        method: 'PATCH',
        url: `/admin/users/${admin.user.userId}/role`,
        headers: authHeader(admin.accessToken),
        payload: { role: 'USER' },
      });
      expect(selfDemote.statusCode).toBe(400);

      const users = await app.inject({
        method: 'GET',
        url: '/admin/users',
        headers: authHeader(admin.accessToken),
      });
      expect(users.statusCode).toBe(200);
      expect(users.json().users).toHaveLength(2);
    } finally {
      await app.close();
    }
  });

  it('lets an admin group devices and grant a user blanket access via group membership', async () => {
    const app = await createApp();
    try {
      const admin = await registerAndLogin(app, { email: 'admin@example.com', displayName: 'Admin' });
      const owner = await registerAndLogin(app, { email: 'owner@example.com', displayName: 'Owner' });
      const member = await registerAndLogin(app, { email: 'member@example.com', displayName: 'Member' });
      const outsider = await registerAndLogin(app, { email: 'outsider@example.com', displayName: 'Outsider' });

      // A non-admin (the device owner) can't create a group.
      const forbiddenCreate = await app.inject({
        method: 'POST',
        url: '/admin/groups',
        headers: authHeader(owner.accessToken),
        payload: { name: 'Lab Macs' },
      });
      expect(forbiddenCreate.statusCode).toBe(403);

      const createGroup = await app.inject({
        method: 'POST',
        url: '/admin/groups',
        headers: authHeader(admin.accessToken),
        payload: { name: 'Lab Macs' },
      });
      expect(createGroup.statusCode).toBe(201);
      const groupId = createGroup.json().group.id;

      const createDevice = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: authHeader(owner.accessToken),
        payload: { name: 'Lab Mac mini' },
      });
      const deviceId = createDevice.json().device.id;

      // Before group assignment, the member can't see the device at all.
      const beforeAssign = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: authHeader(member.accessToken),
      });
      expect(beforeAssign.json().devices).toHaveLength(0);

      const assign = await app.inject({
        method: 'POST',
        url: `/admin/devices/${deviceId}/group`,
        headers: authHeader(admin.accessToken),
        payload: { groupId },
      });
      expect(assign.statusCode).toBe(200);
      expect(assign.json().groupId).toBe(groupId);

      const addMember = await app.inject({
        method: 'POST',
        url: `/admin/groups/${groupId}/members`,
        headers: authHeader(admin.accessToken),
        payload: { email: 'member@example.com', role: 'VIEWER' },
      });
      expect(addMember.statusCode).toBe(201);

      // The group member now sees the device with role VIEWER (via the group),
      // without ever having a direct AccessGrant.
      const afterAssign = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: authHeader(member.accessToken),
      });
      const listed = afterAssign.json().devices.find((d: { id: string }) => d.id === deviceId);
      expect(listed).toBeDefined();
      expect(listed.role).toBe('VIEWER');
      expect(listed.groupId).toBe(groupId);

      // The group member can issue a connect ticket purely on group membership.
      const connect = await app.inject({
        method: 'POST',
        url: `/devices/${deviceId}/connect`,
        headers: authHeader(member.accessToken),
      });
      expect(connect.statusCode).toBe(200);
      expect(connect.json().role).toBe('VIEWER');

      // A user with no grant and no group membership still can't see or connect.
      const outsiderList = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: authHeader(outsider.accessToken),
      });
      expect(outsiderList.json().devices).toHaveLength(0);
      const outsiderConnect = await app.inject({
        method: 'POST',
        url: `/devices/${deviceId}/connect`,
        headers: authHeader(outsider.accessToken),
      });
      expect(outsiderConnect.statusCode).toBe(404);

      // The admin sees every device fleet-wide, tagged role=ADMIN, even
      // though they neither own it nor are a group member.
      const adminList = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: authHeader(admin.accessToken),
      });
      const adminListed = adminList.json().devices.find((d: { id: string }) => d.id === deviceId);
      expect(adminListed.role).toBe('ADMIN');

      // Removing the membership revokes access immediately.
      const removeMember = await app.inject({
        method: 'DELETE',
        url: `/admin/groups/${groupId}/members/${member.user.userId}`,
        headers: authHeader(admin.accessToken),
      });
      expect(removeMember.statusCode).toBe(200);

      const afterRemove = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: authHeader(member.accessToken),
      });
      expect(afterRemove.json().devices).toHaveLength(0);
    } finally {
      await app.close();
    }
  });

  it('takes the higher of a direct grant and a group membership role (CONTROLLER wins over VIEWER)', async () => {
    const app = await createApp();
    try {
      const admin = await registerAndLogin(app, { email: 'admin@example.com', displayName: 'Admin' });
      const owner = await registerAndLogin(app, { email: 'owner@example.com', displayName: 'Owner' });
      const user = await registerAndLogin(app, { email: 'user@example.com', displayName: 'User' });

      const createDevice = await app.inject({
        method: 'POST',
        url: '/devices',
        headers: authHeader(owner.accessToken),
        payload: { name: 'Shared Mac' },
      });
      const deviceId = createDevice.json().device.id;

      // Direct grant: VIEWER only.
      await app.inject({
        method: 'POST',
        url: `/devices/${deviceId}/grants`,
        headers: authHeader(owner.accessToken),
        payload: { granteeEmail: 'user@example.com', role: 'VIEWER' },
      });

      const createGroup = await app.inject({
        method: 'POST',
        url: '/admin/groups',
        headers: authHeader(admin.accessToken),
        payload: { name: 'Controllers' },
      });
      const groupId = createGroup.json().group.id;

      await app.inject({
        method: 'POST',
        url: `/admin/devices/${deviceId}/group`,
        headers: authHeader(admin.accessToken),
        payload: { groupId },
      });

      // Group membership: CONTROLLER — should win over the VIEWER direct grant.
      await app.inject({
        method: 'POST',
        url: `/admin/groups/${groupId}/members`,
        headers: authHeader(admin.accessToken),
        payload: { email: 'user@example.com', role: 'CONTROLLER' },
      });

      const list = await app.inject({
        method: 'GET',
        url: '/devices',
        headers: authHeader(user.accessToken),
      });
      const listed = list.json().devices.find((d: { id: string }) => d.id === deviceId);
      expect(listed.role).toBe('CONTROLLER');
    } finally {
      await app.close();
    }
  });
});
