import { ForbiddenError, NotFoundError, ValidationError } from '../domain/errors';
import { AuthenticatedUser, GrantRole } from '../domain/types';
import { DataStore } from '../repositories/types';

// Group management is intentionally admin-only: groups are how an operator
// organizes every machine in the fleet and decides which users see which
// devices, so unlike per-device AccessGrant (which an owner can hand out
// themselves), only an admin curates groups/membership/device assignment.
export class GroupService {
  constructor(private readonly store: DataStore) {}

  private requireAdmin(actor: AuthenticatedUser): void {
    if (actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only an admin can manage groups');
    }
  }

  async createGroup(actor: AuthenticatedUser, name: string) {
    this.requireAdmin(actor);
    return this.store.createGroup({ name: name.trim(), createdByUserId: actor.userId });
  }

  async listGroups(actor: AuthenticatedUser) {
    this.requireAdmin(actor);
    const groups = await this.store.listGroups();
    return Promise.all(
      groups.map(async (group) => ({
        ...group,
        memberCount: (await this.store.listGroupMemberships(group.id)).length,
        deviceCount: (await this.store.listGroupDevices(group.id)).length,
      })),
    );
  }

  async getGroup(actor: AuthenticatedUser, groupId: string) {
    this.requireAdmin(actor);
    const group = await this.store.findGroupById(groupId);
    if (!group) {
      throw new NotFoundError('Group not found');
    }
    const [members, devices] = await Promise.all([
      this.store.listGroupMemberships(groupId),
      this.store.listGroupDevices(groupId),
    ]);
    return { group, members, devices };
  }

  async deleteGroup(actor: AuthenticatedUser, groupId: string) {
    this.requireAdmin(actor);
    const group = await this.store.findGroupById(groupId);
    if (!group) {
      throw new NotFoundError('Group not found');
    }
    await this.store.deleteGroup(groupId);
  }

  async addMember(actor: AuthenticatedUser, groupId: string, email: string, role: GrantRole) {
    this.requireAdmin(actor);
    const group = await this.store.findGroupById(groupId);
    if (!group) {
      throw new NotFoundError('Group not found');
    }
    const user = await this.store.findUserByEmail(email.trim().toLowerCase());
    if (!user) {
      throw new ValidationError('User must already have an account');
    }
    return this.store.upsertGroupMembership({ groupId, userId: user.id, role });
  }

  async removeMember(actor: AuthenticatedUser, groupId: string, userId: string) {
    this.requireAdmin(actor);
    const group = await this.store.findGroupById(groupId);
    if (!group) {
      throw new NotFoundError('Group not found');
    }
    await this.store.removeGroupMembership(groupId, userId);
  }

  async assignDeviceToGroup(actor: AuthenticatedUser, deviceId: string, groupId: string | null) {
    this.requireAdmin(actor);
    if (groupId) {
      const group = await this.store.findGroupById(groupId);
      if (!group) {
        throw new NotFoundError('Group not found');
      }
    }
    const device = await this.store.assignDeviceGroup(deviceId, groupId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }
    return device;
  }
}
