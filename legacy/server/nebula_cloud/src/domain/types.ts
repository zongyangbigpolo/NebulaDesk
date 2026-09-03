export type GrantRole = 'VIEWER' | 'CONTROLLER';
export type EffectiveRole = 'OWNER' | 'ADMIN' | GrantRole;
export type ConnectionAuditResult = 'ISSUED' | 'AUTHORIZED' | 'DENIED' | 'EXPIRED';
export type UserRole = 'USER' | 'ADMIN';

export interface UserRecord {
  id: string;
  email: string;
  passwordHash: string;
  displayName: string;
  role: UserRole;
  // Remaining "connect" credit in seconds — see DeviceService/ConnectService
  // and README.md's "Trial credit" section. Not metered for admins.
  creditSeconds: number;
  createdAt: Date;
}

export interface DeviceRecord {
  id: string;
  ownerUserId: string;
  name: string;
  relayDeviceId: string;
  relayTokenHash: string;
  // Plaintext application-layer session-encryption secret (NebulaCrypto PSK)
  // shared between this VDA and every CWA that connects to it — see
  // ARCHITECTURE.md §4a. Unlike relayTokenHash this must round-trip in the
  // clear so DeviceService can hand it to every authorized connecting viewer.
  psk: string;
  claimCodeHash: string | null;
  claimCodeExpiresAt: Date | null;
  groupId: string | null;
  createdAt: Date;
  lastSeenAt: Date | null;
}

export interface AccessGrantRecord {
  id: string;
  deviceId: string;
  granteeUserId: string;
  role: GrantRole;
  createdAt: Date;
  revokedAt: Date | null;
  createdByUserId: string;
}

export interface AccessGrantWithUser extends AccessGrantRecord {
  granteeUser: Pick<UserRecord, 'id' | 'email' | 'displayName' | 'createdAt'>;
}

export interface RefreshTokenRecord {
  id: string;
  userId: string;
  tokenHash: string;
  expiresAt: Date;
  revokedAt: Date | null;
  createdAt: Date;
}

export interface ConnectionAuditRecord {
  id: string;
  deviceId: string;
  requestedByUserId: string;
  issuedTokenHash: string;
  issuedAt: Date;
  expiresAt: Date;
  usedAt: Date | null;
  sourceIp: string | null;
  result: ConnectionAuditResult;
  reason: string | null;
}

export interface AccessibleDeviceRecord {
  device: DeviceRecord;
  role: EffectiveRole;
}

export interface AuthenticatedUser {
  userId: string;
  email: string;
  displayName: string;
  role: UserRole;
}

// A named collection of devices, managed by an admin, that a set of users can
// be granted blanket VIEWER/CONTROLLER access to (GroupMembershipRecord)
// instead of requiring one AccessGrant per device per user.
export interface GroupRecord {
  id: string;
  name: string;
  createdByUserId: string;
  createdAt: Date;
}

export interface GroupMembershipRecord {
  id: string;
  groupId: string;
  userId: string;
  role: GrantRole;
  createdAt: Date;
}

export interface GroupMembershipWithUser extends GroupMembershipRecord {
  user: Pick<UserRecord, 'id' | 'email' | 'displayName' | 'createdAt'>;
}

