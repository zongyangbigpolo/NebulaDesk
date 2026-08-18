import crypto from 'node:crypto';

import {
  AccessGrantRecord,
  AccessGrantWithUser,
  AccessibleDeviceRecord,
  ConnectionAuditRecord,
  DeviceRecord,
  EffectiveRole,
  RefreshTokenRecord,
  UserRecord,
} from '../domain/types';
import { DataStore } from './types';

function cloneDate(value: Date | null): Date | null {
  return value ? new Date(value) : null;
}

export class InMemoryDataStore implements DataStore {
  private readonly users = new Map<string, UserRecord>();
  private readonly usersByEmail = new Map<string, string>();
  private readonly devices = new Map<string, DeviceRecord>();
  private readonly devicesByRelayDeviceId = new Map<string, string>();
  private readonly grants = new Map<string, AccessGrantRecord>();
  private readonly grantByDeviceAndUser = new Map<string, string>();
  private readonly refreshTokens = new Map<string, RefreshTokenRecord>();
  private readonly connectionAudits = new Map<string, ConnectionAuditRecord>();

  async createUser(input: { email: string; passwordHash: string; displayName: string }): Promise<UserRecord> {
    const id = crypto.randomUUID();
    const user: UserRecord = {
      id,
      email: input.email,
      passwordHash: input.passwordHash,
      displayName: input.displayName,
      createdAt: new Date(),
    };
    this.users.set(id, user);
    this.usersByEmail.set(user.email, user.id);
    return this.copyUser(user);
  }

  async findUserByEmail(email: string): Promise<UserRecord | null> {
    const id = this.usersByEmail.get(email);
    return id ? this.findUserById(id) : null;
  }

  async findUserById(id: string): Promise<UserRecord | null> {
    const user = this.users.get(id);
    return user ? this.copyUser(user) : null;
  }

  async createRefreshToken(input: { userId: string; tokenHash: string; expiresAt: Date }): Promise<RefreshTokenRecord> {
    const record: RefreshTokenRecord = {
      id: crypto.randomUUID(),
      userId: input.userId,
      tokenHash: input.tokenHash,
      expiresAt: new Date(input.expiresAt),
      revokedAt: null,
      createdAt: new Date(),
    };
    this.refreshTokens.set(record.tokenHash, record);
    return this.copyRefreshToken(record);
  }

  async findActiveRefreshToken(tokenHash: string, now: Date): Promise<RefreshTokenRecord | null> {
    const record = this.refreshTokens.get(tokenHash);
    if (!record || record.revokedAt || record.expiresAt <= now) {
      return null;
    }
    return this.copyRefreshToken(record);
  }

  async revokeRefreshToken(tokenHash: string, now: Date): Promise<void> {
    const record = this.refreshTokens.get(tokenHash);
    if (record && !record.revokedAt) {
      record.revokedAt = new Date(now);
    }
  }

  async createDevice(input: {
    ownerUserId: string;
    name: string;
    relayDeviceId: string;
    relayTokenHash: string;
    claimCodeHash: string;
    claimCodeExpiresAt: Date;
  }): Promise<DeviceRecord> {
    const record: DeviceRecord = {
      id: crypto.randomUUID(),
      ownerUserId: input.ownerUserId,
      name: input.name,
      relayDeviceId: input.relayDeviceId,
      relayTokenHash: input.relayTokenHash,
      claimCodeHash: input.claimCodeHash,
      claimCodeExpiresAt: new Date(input.claimCodeExpiresAt),
      createdAt: new Date(),
      lastSeenAt: null,
    };
    this.devices.set(record.id, record);
    this.devicesByRelayDeviceId.set(record.relayDeviceId, record.id);
    return this.copyDevice(record);
  }

  async findDeviceById(id: string): Promise<DeviceRecord | null> {
    const device = this.devices.get(id);
    return device ? this.copyDevice(device) : null;
  }

  async findDeviceByRelayDeviceId(relayDeviceId: string): Promise<DeviceRecord | null> {
    const id = this.devicesByRelayDeviceId.get(relayDeviceId);
    return id ? this.findDeviceById(id) : null;
  }

  async listAccessibleDevices(userId: string): Promise<AccessibleDeviceRecord[]> {
    const results: AccessibleDeviceRecord[] = [];
    for (const device of this.devices.values()) {
      const role = await this.getEffectiveRole(userId, device.id);
      if (role) {
        results.push({ device: this.copyDevice(device), role });
      }
    }
    results.sort((a, b) => a.device.createdAt.getTime() - b.device.createdAt.getTime());
    return results;
  }

  async getEffectiveRole(userId: string, deviceId: string): Promise<EffectiveRole | null> {
    const device = this.devices.get(deviceId);
    if (!device) {
      return null;
    }
    if (device.ownerUserId === userId) {
      return 'OWNER';
    }
    const grantId = this.grantByDeviceAndUser.get(`${deviceId}:${userId}`);
    if (!grantId) {
      return null;
    }
    const grant = this.grants.get(grantId);
    if (!grant || grant.revokedAt) {
      return null;
    }
    return grant.role;
  }

  async deleteDevice(id: string): Promise<void> {
    const device = this.devices.get(id);
    if (!device) {
      return;
    }
    this.devices.delete(id);
    this.devicesByRelayDeviceId.delete(device.relayDeviceId);

    for (const [grantId, grant] of [...this.grants.entries()]) {
      if (grant.deviceId === id) {
        this.grants.delete(grantId);
        this.grantByDeviceAndUser.delete(`${grant.deviceId}:${grant.granteeUserId}`);
      }
    }

    for (const [auditId, audit] of [...this.connectionAudits.entries()]) {
      if (audit.deviceId === id) {
        this.connectionAudits.delete(auditId);
      }
    }
  }

  async touchDeviceHeartbeat(id: string, seenAt: Date): Promise<DeviceRecord | null> {
    const device = this.devices.get(id);
    if (!device) {
      return null;
    }
    device.lastSeenAt = new Date(seenAt);
    return this.copyDevice(device);
  }

  async redeemClaimCode(input: {
    claimCodeHash: string;
    now: Date;
    newRelayTokenHash: string;
  }): Promise<DeviceRecord | null> {
    for (const device of this.devices.values()) {
      if (
        device.claimCodeHash === input.claimCodeHash
        && device.claimCodeExpiresAt
        && device.claimCodeExpiresAt > input.now
      ) {
        device.claimCodeHash = null;
        device.claimCodeExpiresAt = null;
        device.relayTokenHash = input.newRelayTokenHash;
        return this.copyDevice(device);
      }
    }
    return null;
  }

  async upsertAccessGrant(input: {
    deviceId: string;
    granteeUserId: string;
    role: 'VIEWER' | 'CONTROLLER';
    createdByUserId: string;
    now: Date;
  }): Promise<AccessGrantWithUser> {
    const key = `${input.deviceId}:${input.granteeUserId}`;
    const existingId = this.grantByDeviceAndUser.get(key);
    const grant: AccessGrantRecord = existingId
      ? this.grants.get(existingId)!
      : {
          id: crypto.randomUUID(),
          deviceId: input.deviceId,
          granteeUserId: input.granteeUserId,
          role: input.role,
          createdAt: new Date(input.now),
          revokedAt: null,
          createdByUserId: input.createdByUserId,
        };

    grant.role = input.role;
    grant.revokedAt = null;
    grant.createdByUserId = input.createdByUserId;
    this.grants.set(grant.id, grant);
    this.grantByDeviceAndUser.set(key, grant.id);
    return this.enrichGrant(grant);
  }

  async listDeviceGrants(deviceId: string): Promise<AccessGrantWithUser[]> {
    const grants = [...this.grants.values()]
      .filter((grant) => grant.deviceId === deviceId && !grant.revokedAt)
      .sort((a, b) => a.createdAt.getTime() - b.createdAt.getTime());
    return Promise.all(grants.map((grant) => this.enrichGrant(grant)));
  }

  async findGrantById(id: string): Promise<AccessGrantWithUser | null> {
    const grant = this.grants.get(id);
    return grant ? this.enrichGrant(grant) : null;
  }

  async revokeGrant(id: string, now: Date): Promise<AccessGrantWithUser | null> {
    const grant = this.grants.get(id);
    if (!grant) {
      return null;
    }
    grant.revokedAt = new Date(now);
    return this.enrichGrant(grant);
  }

  async createConnectionAudit(input: {
    id: string;
    deviceId: string;
    requestedByUserId: string;
    issuedTokenHash: string;
    issuedAt: Date;
    expiresAt: Date;
    sourceIp: string | null;
    result: 'ISSUED' | 'AUTHORIZED' | 'DENIED' | 'EXPIRED';
    reason?: string | null;
  }): Promise<ConnectionAuditRecord> {
    const record: ConnectionAuditRecord = {
      id: input.id,
      deviceId: input.deviceId,
      requestedByUserId: input.requestedByUserId,
      issuedTokenHash: input.issuedTokenHash,
      issuedAt: new Date(input.issuedAt),
      expiresAt: new Date(input.expiresAt),
      usedAt: null,
      sourceIp: input.sourceIp,
      result: input.result,
      reason: input.reason ?? null,
    };
    this.connectionAudits.set(record.id, record);
    return this.copyConnectionAudit(record);
  }

  async findConnectionAuditById(id: string): Promise<ConnectionAuditRecord | null> {
    const record = this.connectionAudits.get(id);
    return record ? this.copyConnectionAudit(record) : null;
  }

  async updateConnectionAudit(
    id: string,
    patch: Partial<Pick<ConnectionAuditRecord, 'usedAt' | 'result' | 'reason'>>,
  ): Promise<ConnectionAuditRecord | null> {
    const record = this.connectionAudits.get(id);
    if (!record) {
      return null;
    }
    if (patch.usedAt !== undefined) {
      record.usedAt = cloneDate(patch.usedAt);
    }
    if (patch.result !== undefined) {
      record.result = patch.result;
    }
    if (patch.reason !== undefined) {
      record.reason = patch.reason;
    }
    return this.copyConnectionAudit(record);
  }

  private copyUser(user: UserRecord): UserRecord {
    return { ...user, createdAt: new Date(user.createdAt) };
  }

  private copyDevice(device: DeviceRecord): DeviceRecord {
    return {
      ...device,
      claimCodeExpiresAt: cloneDate(device.claimCodeExpiresAt),
      createdAt: new Date(device.createdAt),
      lastSeenAt: cloneDate(device.lastSeenAt),
    };
  }

  private copyRefreshToken(record: RefreshTokenRecord): RefreshTokenRecord {
    return {
      ...record,
      createdAt: new Date(record.createdAt),
      expiresAt: new Date(record.expiresAt),
      revokedAt: cloneDate(record.revokedAt),
    };
  }

  private copyConnectionAudit(record: ConnectionAuditRecord): ConnectionAuditRecord {
    return {
      ...record,
      expiresAt: new Date(record.expiresAt),
      issuedAt: new Date(record.issuedAt),
      usedAt: cloneDate(record.usedAt),
    };
  }

  private async enrichGrant(grant: AccessGrantRecord): Promise<AccessGrantWithUser> {
    const user = this.users.get(grant.granteeUserId);
    if (!user) {
      throw new Error(`Missing user ${grant.granteeUserId}`);
    }
    return {
      ...grant,
      createdAt: new Date(grant.createdAt),
      revokedAt: cloneDate(grant.revokedAt),
      granteeUser: {
        id: user.id,
        email: user.email,
        displayName: user.displayName,
        createdAt: new Date(user.createdAt),
      },
    };
  }
}
