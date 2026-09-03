import {
  AccessGrantWithUser,
  AccessibleDeviceRecord,
  ConnectionAuditRecord,
  ConnectionAuditResult,
  DeviceRecord,
  EffectiveRole,
  GrantRole,
  GroupMembershipWithUser,
  GroupRecord,
  RefreshTokenRecord,
  UserRecord,
  UserRole,
} from '../domain/types';

export interface DataStore {
  createUser(input: {
    email: string;
    passwordHash: string;
    displayName: string;
    role: UserRole;
    creditSeconds: number;
  }): Promise<UserRecord>;
  findUserByEmail(email: string): Promise<UserRecord | null>;
  findUserById(id: string): Promise<UserRecord | null>;
  listUsers(): Promise<UserRecord[]>;
  updateUserRole(userId: string, role: UserRole): Promise<UserRecord | null>;
  // Atomically deducts `amountSeconds` from the user's credit — only if they
  // have at least that much — and returns the updated record; returns null
  // if the user doesn't exist OR doesn't have enough credit (the caller
  // can't tell which from the return value alone, but by the time this is
  // called the user is already known to exist, see ConnectService).
  spendUserCredit(userId: string, amountSeconds: number): Promise<UserRecord | null>;
  // Admin top-up (see PATCH /admin/users/:id/credit) — adds (or, with a
  // negative amount, removes) credit; never goes below zero.
  addUserCredit(userId: string, amountSeconds: number): Promise<UserRecord | null>;

  createRefreshToken(input: { userId: string; tokenHash: string; expiresAt: Date }): Promise<RefreshTokenRecord>;
  findActiveRefreshToken(tokenHash: string, now: Date): Promise<RefreshTokenRecord | null>;
  revokeRefreshToken(tokenHash: string, now: Date): Promise<void>;

  createDevice(input: {
    ownerUserId: string;
    name: string;
    relayDeviceId: string;
    relayTokenHash: string;
    psk: string;
    claimCodeHash: string;
    claimCodeExpiresAt: Date;
  }): Promise<DeviceRecord>;
  findDeviceById(id: string): Promise<DeviceRecord | null>;
  findDeviceByRelayDeviceId(relayDeviceId: string): Promise<DeviceRecord | null>;
  // Devices owned by, or accessible to, `userId` — either via a direct
  // AccessGrant or via membership in the group the device belongs to.
  listAccessibleDevices(userId: string): Promise<AccessibleDeviceRecord[]>;
  // Every device regardless of ownership — admin-only view (see DeviceService).
  listAllDevices(): Promise<DeviceRecord[]>;
  getEffectiveRole(userId: string, deviceId: string): Promise<EffectiveRole | null>;
  deleteDevice(id: string): Promise<void>;
  touchDeviceHeartbeat(id: string, seenAt: Date): Promise<DeviceRecord | null>;
  assignDeviceGroup(deviceId: string, groupId: string | null): Promise<DeviceRecord | null>;
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

  // Groups: admin-curated device collections + blanket member access (see
  // server/nebula_cloud/README.md's "Groups & admin role" section).
  createGroup(input: { name: string; createdByUserId: string }): Promise<GroupRecord>;
  listGroups(): Promise<GroupRecord[]>;
  findGroupById(id: string): Promise<GroupRecord | null>;
  deleteGroup(id: string): Promise<void>;
  upsertGroupMembership(input: {
    groupId: string;
    userId: string;
    role: GrantRole;
  }): Promise<GroupMembershipWithUser>;
  removeGroupMembership(groupId: string, userId: string): Promise<void>;
  listGroupMemberships(groupId: string): Promise<GroupMembershipWithUser[]>;
  getGroupMembership(groupId: string, userId: string): Promise<GroupMembershipWithUser | null>;
  listGroupDevices(groupId: string): Promise<DeviceRecord[]>;
}

