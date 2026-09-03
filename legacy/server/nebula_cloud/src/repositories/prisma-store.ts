import {
  AccessGrantRole,
  ConnectionAuditResult,
  PrismaClient,
  UserRole as PrismaUserRole,
} from '@prisma/client';

import {
  AccessGrantWithUser,
  AccessibleDeviceRecord,
  ConnectionAuditRecord,
  DeviceRecord,
  EffectiveRole,
  GroupMembershipWithUser,
  GroupRecord,
  RefreshTokenRecord,
  UserRecord,
  UserRole,
} from '../domain/types';
import { DataStore } from './types';

function toUserRecord(user: {
  id: string;
  email: string;
  passwordHash: string;
  displayName: string;
  role: PrismaUserRole;
  creditSeconds: number;
  createdAt: Date;
}): UserRecord {
  return { ...user, role: user.role };
}

function toDeviceRecord(device: {
  id: string;
  ownerUserId: string;
  name: string;
  relayDeviceId: string;
  relayTokenHash: string;
  psk: string;
  claimCodeHash: string | null;
  claimCodeExpiresAt: Date | null;
  groupId: string | null;
  createdAt: Date;
  lastSeenAt: Date | null;
}): DeviceRecord {
  return { ...device };
}

function toGroupRecord(group: { id: string; name: string; createdByUserId: string; createdAt: Date }): GroupRecord {
  return { ...group };
}

function toMembershipWithUser(membership: {
  id: string;
  groupId: string;
  userId: string;
  role: AccessGrantRole;
  createdAt: Date;
  user: { id: string; email: string; displayName: string; createdAt: Date };
}): GroupMembershipWithUser {
  return {
    id: membership.id,
    groupId: membership.groupId,
    userId: membership.userId,
    role: fromGrantRole(membership.role),
    createdAt: membership.createdAt,
    user: {
      id: membership.user.id,
      email: membership.user.email,
      displayName: membership.user.displayName,
      createdAt: membership.user.createdAt,
    },
  };
}

function toRefreshTokenRecord(token: {
  id: string;
  userId: string;
  tokenHash: string;
  expiresAt: Date;
  revokedAt: Date | null;
  createdAt: Date;
}): RefreshTokenRecord {
  return { ...token };
}

function toConnectionAuditRecord(audit: {
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
}): ConnectionAuditRecord {
  return {
    ...audit,
    result: audit.result,
  };
}

function toGrantRole(role: 'VIEWER' | 'CONTROLLER'): AccessGrantRole {
  return role === 'CONTROLLER' ? AccessGrantRole.CONTROLLER : AccessGrantRole.VIEWER;
}

function fromGrantRole(role: AccessGrantRole): 'VIEWER' | 'CONTROLLER' {
  return role;
}

// Combines device ownership, a direct AccessGrant, and (if the device belongs
// to one) a GroupMembership on that group into a single effective role — a
// CONTROLLER grant from either source outranks a VIEWER grant from the other.
// Mirrors InMemoryDataStore.getEffectiveRole's precedence exactly.
function computeRoleFromRelations(
  userId: string,
  device: {
    ownerUserId: string;
    accessGrants: { role: AccessGrantRole }[];
    group: { memberships: { role: AccessGrantRole }[] } | null;
  },
): EffectiveRole | null {
  if (device.ownerUserId === userId) {
    return 'OWNER';
  }
  const grantRole = device.accessGrants[0]?.role ?? null;
  const groupRole = device.group?.memberships[0]?.role ?? null;
  if (!grantRole && !groupRole) {
    return null;
  }
  if (grantRole === AccessGrantRole.CONTROLLER || groupRole === AccessGrantRole.CONTROLLER) {
    return 'CONTROLLER';
  }
  return 'VIEWER';
}

function toUserRole(role: UserRole): PrismaUserRole {
  return role === 'ADMIN' ? PrismaUserRole.ADMIN : PrismaUserRole.USER;
}

export class PrismaDataStore implements DataStore {
  constructor(private readonly prisma: PrismaClient) {}

  async createUser(input: {
    email: string;
    passwordHash: string;
    displayName: string;
    role: UserRole;
    creditSeconds: number;
  }): Promise<UserRecord> {
    const user = await this.prisma.user.create({ data: { ...input, role: toUserRole(input.role) } });
    return toUserRecord(user);
  }

  async findUserByEmail(email: string): Promise<UserRecord | null> {
    const user = await this.prisma.user.findUnique({ where: { email } });
    return user ? toUserRecord(user) : null;
  }

  async findUserById(id: string): Promise<UserRecord | null> {
    const user = await this.prisma.user.findUnique({ where: { id } });
    return user ? toUserRecord(user) : null;
  }

  async listUsers(): Promise<UserRecord[]> {
    const users = await this.prisma.user.findMany({ orderBy: { createdAt: 'asc' } });
    return users.map(toUserRecord);
  }

  async updateUserRole(userId: string, role: UserRole): Promise<UserRecord | null> {
    const existing = await this.prisma.user.findUnique({ where: { id: userId } });
    if (!existing) {
      return null;
    }
    const user = await this.prisma.user.update({ where: { id: userId }, data: { role: toUserRole(role) } });
    return toUserRecord(user);
  }

  async spendUserCredit(userId: string, amountSeconds: number): Promise<UserRecord | null> {
    // Atomic conditional decrement: the WHERE guard (creditSeconds >= amount)
    // makes this race-safe under concurrent connect requests without a
    // separate transaction/lock — either the DB applies the decrement or it
    // doesn't, there's no read-then-write window for two requests to both
    // pass a stale balance check.
    const result = await this.prisma.user.updateMany({
      where: { id: userId, creditSeconds: { gte: amountSeconds } },
      data: { creditSeconds: { decrement: amountSeconds } },
    });
    if (result.count === 0) {
      return null;
    }
    return this.findUserById(userId);
  }

  async addUserCredit(userId: string, amountSeconds: number): Promise<UserRecord | null> {
    const existing = await this.prisma.user.findUnique({ where: { id: userId } });
    if (!existing) {
      return null;
    }
    const newBalance = Math.max(0, existing.creditSeconds + amountSeconds);
    const user = await this.prisma.user.update({ where: { id: userId }, data: { creditSeconds: newBalance } });
    return toUserRecord(user);
  }

  async createRefreshToken(input: { userId: string; tokenHash: string; expiresAt: Date }): Promise<RefreshTokenRecord> {
    const token = await this.prisma.refreshToken.create({ data: input });
    return toRefreshTokenRecord(token);
  }

  async findActiveRefreshToken(tokenHash: string, now: Date): Promise<RefreshTokenRecord | null> {
    const token = await this.prisma.refreshToken.findFirst({
      where: {
        tokenHash,
        revokedAt: null,
        expiresAt: { gt: now },
      },
    });
    return token ? toRefreshTokenRecord(token) : null;
  }

  async revokeRefreshToken(tokenHash: string, now: Date): Promise<void> {
    await this.prisma.refreshToken.updateMany({
      where: { tokenHash, revokedAt: null },
      data: { revokedAt: now },
    });
  }

  async createDevice(input: {
    ownerUserId: string;
    name: string;
    relayDeviceId: string;
    relayTokenHash: string;
    psk: string;
    claimCodeHash: string;
    claimCodeExpiresAt: Date;
  }): Promise<DeviceRecord> {
    const device = await this.prisma.device.create({ data: input });
    return toDeviceRecord(device);
  }

  async findDeviceById(id: string): Promise<DeviceRecord | null> {
    const device = await this.prisma.device.findUnique({ where: { id } });
    return device ? toDeviceRecord(device) : null;
  }

  async findDeviceByRelayDeviceId(relayDeviceId: string): Promise<DeviceRecord | null> {
    const device = await this.prisma.device.findUnique({ where: { relayDeviceId } });
    return device ? toDeviceRecord(device) : null;
  }

  async listAccessibleDevices(userId: string): Promise<AccessibleDeviceRecord[]> {
    const devices = await this.prisma.device.findMany({
      where: {
        OR: [
          { ownerUserId: userId },
          {
            accessGrants: {
              some: {
                granteeUserId: userId,
                revokedAt: null,
              },
            },
          },
          {
            group: {
              memberships: {
                some: { userId },
              },
            },
          },
        ],
      },
      include: {
        accessGrants: {
          where: { granteeUserId: userId, revokedAt: null },
          take: 1,
        },
        group: {
          include: {
            memberships: {
              where: { userId },
              take: 1,
            },
          },
        },
      },
      orderBy: { createdAt: 'asc' },
    });

    return devices
      .map((device) => ({
        device: toDeviceRecord(device),
        role: computeRoleFromRelations(userId, device),
      }))
      // The OR/where above already guarantees a match, but computeRoleFromRelations'
      // return type is nullable for getEffectiveRole's sake — narrow it back out here.
      .filter((entry): entry is AccessibleDeviceRecord => entry.role !== null);
  }

  async listAllDevices(): Promise<DeviceRecord[]> {
    const devices = await this.prisma.device.findMany({ orderBy: { createdAt: 'asc' } });
    return devices.map(toDeviceRecord);
  }

  async getEffectiveRole(userId: string, deviceId: string): Promise<EffectiveRole | null> {
    const device = await this.prisma.device.findUnique({
      where: { id: deviceId },
      include: {
        accessGrants: {
          where: { granteeUserId: userId, revokedAt: null },
          take: 1,
        },
        group: {
          include: {
            memberships: {
              where: { userId },
              take: 1,
            },
          },
        },
      },
    });

    if (!device) {
      return null;
    }
    return computeRoleFromRelations(userId, device);
  }

  async deleteDevice(id: string): Promise<void> {
    await this.prisma.device.deleteMany({ where: { id } });
  }

  async touchDeviceHeartbeat(id: string, seenAt: Date): Promise<DeviceRecord | null> {
    const existing = await this.prisma.device.findUnique({ where: { id } });
    if (!existing) {
      return null;
    }
    const device = await this.prisma.device.update({
      where: { id },
      data: { lastSeenAt: seenAt },
    });
    return toDeviceRecord(device);
  }

  async assignDeviceGroup(deviceId: string, groupId: string | null): Promise<DeviceRecord | null> {
    const existing = await this.prisma.device.findUnique({ where: { id: deviceId } });
    if (!existing) {
      return null;
    }
    const device = await this.prisma.device.update({ where: { id: deviceId }, data: { groupId } });
    return toDeviceRecord(device);
  }

  async redeemClaimCode(input: {
    claimCodeHash: string;
    now: Date;
    newRelayTokenHash: string;
  }): Promise<DeviceRecord | null> {
    const device = await this.prisma.device.findFirst({
      where: {
        claimCodeHash: input.claimCodeHash,
        claimCodeExpiresAt: { gt: input.now },
      },
    });

    if (!device) {
      return null;
    }

    const updated = await this.prisma.device.update({
      where: { id: device.id },
      data: {
        claimCodeHash: null,
        claimCodeExpiresAt: null,
        relayTokenHash: input.newRelayTokenHash,
      },
    });

    return toDeviceRecord(updated);
  }

  async upsertAccessGrant(input: {
    deviceId: string;
    granteeUserId: string;
    role: 'VIEWER' | 'CONTROLLER';
    createdByUserId: string;
    now: Date;
  }): Promise<AccessGrantWithUser> {
    const grant = await this.prisma.accessGrant.upsert({
      where: {
        deviceId_granteeUserId: {
          deviceId: input.deviceId,
          granteeUserId: input.granteeUserId,
        },
      },
      create: {
        deviceId: input.deviceId,
        granteeUserId: input.granteeUserId,
        role: toGrantRole(input.role),
        createdByUserId: input.createdByUserId,
      },
      update: {
        role: toGrantRole(input.role),
        revokedAt: null,
        createdByUserId: input.createdByUserId,
      },
      include: {
        granteeUser: true,
      },
    });

    return {
      id: grant.id,
      deviceId: grant.deviceId,
      granteeUserId: grant.granteeUserId,
      role: fromGrantRole(grant.role),
      createdAt: grant.createdAt,
      revokedAt: grant.revokedAt,
      createdByUserId: grant.createdByUserId,
      granteeUser: {
        id: grant.granteeUser.id,
        email: grant.granteeUser.email,
        displayName: grant.granteeUser.displayName,
        createdAt: grant.granteeUser.createdAt,
      },
    };
  }

  async listDeviceGrants(deviceId: string): Promise<AccessGrantWithUser[]> {
    const grants = await this.prisma.accessGrant.findMany({
      where: { deviceId, revokedAt: null },
      include: { granteeUser: true },
      orderBy: { createdAt: 'asc' },
    });

    return grants.map((grant) => ({
      id: grant.id,
      deviceId: grant.deviceId,
      granteeUserId: grant.granteeUserId,
      role: fromGrantRole(grant.role),
      createdAt: grant.createdAt,
      revokedAt: grant.revokedAt,
      createdByUserId: grant.createdByUserId,
      granteeUser: {
        id: grant.granteeUser.id,
        email: grant.granteeUser.email,
        displayName: grant.granteeUser.displayName,
        createdAt: grant.granteeUser.createdAt,
      },
    }));
  }

  async findGrantById(id: string): Promise<AccessGrantWithUser | null> {
    const grant = await this.prisma.accessGrant.findUnique({
      where: { id },
      include: { granteeUser: true },
    });
    if (!grant) {
      return null;
    }
    return {
      id: grant.id,
      deviceId: grant.deviceId,
      granteeUserId: grant.granteeUserId,
      role: fromGrantRole(grant.role),
      createdAt: grant.createdAt,
      revokedAt: grant.revokedAt,
      createdByUserId: grant.createdByUserId,
      granteeUser: {
        id: grant.granteeUser.id,
        email: grant.granteeUser.email,
        displayName: grant.granteeUser.displayName,
        createdAt: grant.granteeUser.createdAt,
      },
    };
  }

  async revokeGrant(id: string, now: Date): Promise<AccessGrantWithUser | null> {
    const existing = await this.findGrantById(id);
    if (!existing) {
      return null;
    }
    const grant = await this.prisma.accessGrant.update({
      where: { id },
      data: { revokedAt: now },
      include: { granteeUser: true },
    });
    return {
      id: grant.id,
      deviceId: grant.deviceId,
      granteeUserId: grant.granteeUserId,
      role: fromGrantRole(grant.role),
      createdAt: grant.createdAt,
      revokedAt: grant.revokedAt,
      createdByUserId: grant.createdByUserId,
      granteeUser: {
        id: grant.granteeUser.id,
        email: grant.granteeUser.email,
        displayName: grant.granteeUser.displayName,
        createdAt: grant.granteeUser.createdAt,
      },
    };
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
    const audit = await this.prisma.connectionAudit.create({
      data: {
        id: input.id,
        deviceId: input.deviceId,
        requestedByUserId: input.requestedByUserId,
        issuedTokenHash: input.issuedTokenHash,
        issuedAt: input.issuedAt,
        expiresAt: input.expiresAt,
        sourceIp: input.sourceIp,
        result: input.result,
        reason: input.reason ?? null,
      },
    });
    return toConnectionAuditRecord(audit);
  }

  async findConnectionAuditById(id: string): Promise<ConnectionAuditRecord | null> {
    const audit = await this.prisma.connectionAudit.findUnique({ where: { id } });
    return audit ? toConnectionAuditRecord(audit) : null;
  }

  async updateConnectionAudit(
    id: string,
    patch: Partial<Pick<ConnectionAuditRecord, 'usedAt' | 'result' | 'reason'>>,
  ): Promise<ConnectionAuditRecord | null> {
    const existing = await this.prisma.connectionAudit.findUnique({ where: { id } });
    if (!existing) {
      return null;
    }
    const audit = await this.prisma.connectionAudit.update({
      where: { id },
      data: {
        usedAt: patch.usedAt,
        result: patch.result,
        reason: patch.reason,
      },
    });
    return toConnectionAuditRecord(audit);
  }

  async createGroup(input: { name: string; createdByUserId: string }): Promise<GroupRecord> {
    const group = await this.prisma.group.create({ data: input });
    return toGroupRecord(group);
  }

  async listGroups(): Promise<GroupRecord[]> {
    const groups = await this.prisma.group.findMany({ orderBy: { createdAt: 'asc' } });
    return groups.map(toGroupRecord);
  }

  async findGroupById(id: string): Promise<GroupRecord | null> {
    const group = await this.prisma.group.findUnique({ where: { id } });
    return group ? toGroupRecord(group) : null;
  }

  async deleteGroup(id: string): Promise<void> {
    // Devices keep existing (FK is ON DELETE SET NULL); only the group and
    // its memberships go away.
    await this.prisma.group.deleteMany({ where: { id } });
  }

  async upsertGroupMembership(input: { groupId: string; userId: string; role: 'VIEWER' | 'CONTROLLER' }): Promise<GroupMembershipWithUser> {
    const membership = await this.prisma.groupMembership.upsert({
      where: { groupId_userId: { groupId: input.groupId, userId: input.userId } },
      create: { groupId: input.groupId, userId: input.userId, role: toGrantRole(input.role) },
      update: { role: toGrantRole(input.role) },
      include: { user: true },
    });
    return toMembershipWithUser(membership);
  }

  async removeGroupMembership(groupId: string, userId: string): Promise<void> {
    await this.prisma.groupMembership.deleteMany({ where: { groupId, userId } });
  }

  async listGroupMemberships(groupId: string): Promise<GroupMembershipWithUser[]> {
    const memberships = await this.prisma.groupMembership.findMany({
      where: { groupId },
      include: { user: true },
      orderBy: { createdAt: 'asc' },
    });
    return memberships.map(toMembershipWithUser);
  }

  async getGroupMembership(groupId: string, userId: string): Promise<GroupMembershipWithUser | null> {
    const membership = await this.prisma.groupMembership.findUnique({
      where: { groupId_userId: { groupId, userId } },
      include: { user: true },
    });
    return membership ? toMembershipWithUser(membership) : null;
  }

  async listGroupDevices(groupId: string): Promise<DeviceRecord[]> {
    const devices = await this.prisma.device.findMany({ where: { groupId }, orderBy: { createdAt: 'asc' } });
    return devices.map(toDeviceRecord);
  }
}
