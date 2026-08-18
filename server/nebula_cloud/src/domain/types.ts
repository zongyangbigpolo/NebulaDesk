export type GrantRole = 'VIEWER' | 'CONTROLLER';
export type EffectiveRole = 'OWNER' | GrantRole;
export type ConnectionAuditResult = 'ISSUED' | 'AUTHORIZED' | 'DENIED' | 'EXPIRED';

export interface UserRecord {
  id: string;
  email: string;
  passwordHash: string;
  displayName: string;
  createdAt: Date;
}

export interface DeviceRecord {
  id: string;
  ownerUserId: string;
  name: string;
  relayDeviceId: string;
  relayTokenHash: string;
  claimCodeHash: string | null;
  claimCodeExpiresAt: Date | null;
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
}
