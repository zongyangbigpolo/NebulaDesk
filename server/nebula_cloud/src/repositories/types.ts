import {
  AccessGrantWithUser,
  AccessibleDeviceRecord,
  ConnectionAuditRecord,
  ConnectionAuditResult,
  DeviceRecord,
  EffectiveRole,
  GrantRole,
  RefreshTokenRecord,
  UserRecord,
} from '../domain/types';

export interface DataStore {
  createUser(input: { email: string; passwordHash: string; displayName: string }): Promise<UserRecord>;
  findUserByEmail(email: string): Promise<UserRecord | null>;
  findUserById(id: string): Promise<UserRecord | null>;

  createRefreshToken(input: { userId: string; tokenHash: string; expiresAt: Date }): Promise<RefreshTokenRecord>;
  findActiveRefreshToken(tokenHash: string, now: Date): Promise<RefreshTokenRecord | null>;
  revokeRefreshToken(tokenHash: string, now: Date): Promise<void>;

  createDevice(input: {
    ownerUserId: string;
    name: string;
    relayDeviceId: string;
    relayTokenHash: string;
    claimCodeHash: string;
    claimCodeExpiresAt: Date;
  }): Promise<DeviceRecord>;
  findDeviceById(id: string): Promise<DeviceRecord | null>;
  findDeviceByRelayDeviceId(relayDeviceId: string): Promise<DeviceRecord | null>;
  listAccessibleDevices(userId: string): Promise<AccessibleDeviceRecord[]>;
  getEffectiveRole(userId: string, deviceId: string): Promise<EffectiveRole | null>;
  deleteDevice(id: string): Promise<void>;
  touchDeviceHeartbeat(id: string, seenAt: Date): Promise<DeviceRecord | null>;
  redeemClaimCode(input: {
    claimCodeHash: string;
    now: Date;
    newRelayTokenHash: string;
  }): Promise<DeviceRecord | null>;

  upsertAccessGrant(input: {
    deviceId: string;
    granteeUserId: string;
    role: GrantRole;
    createdByUserId: string;
    now: Date;
  }): Promise<AccessGrantWithUser>;
  listDeviceGrants(deviceId: string): Promise<AccessGrantWithUser[]>;
  findGrantById(id: string): Promise<AccessGrantWithUser | null>;
  revokeGrant(id: string, now: Date): Promise<AccessGrantWithUser | null>;

  createConnectionAudit(input: {
    id: string;
    deviceId: string;
    requestedByUserId: string;
    issuedTokenHash: string;
    issuedAt: Date;
    expiresAt: Date;
    sourceIp: string | null;
    result: ConnectionAuditResult;
    reason?: string | null;
  }): Promise<ConnectionAuditRecord>;
  findConnectionAuditById(id: string): Promise<ConnectionAuditRecord | null>;
  updateConnectionAudit(
    id: string,
    patch: Partial<Pick<ConnectionAuditRecord, 'usedAt' | 'result' | 'reason'>>,
  ): Promise<ConnectionAuditRecord | null>;
}
