import { ForbiddenError, NotFoundError, ValidationError } from '../domain/errors';
import { AuthenticatedUser, GrantRole } from '../domain/types';
import { DataStore } from '../repositories/types';

export class GrantService {
  constructor(private readonly store: DataStore) {}

  async createOrUpdateGrant(input: {
    actor: AuthenticatedUser;
    deviceId: string;
    granteeEmail: string;
    role: GrantRole;
  }) {
    const device = await this.store.findDeviceById(input.deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }
    if (device.ownerUserId !== input.actor.userId && input.actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only the owner or an admin can manage grants');
    }
    const grantee = await this.store.findUserByEmail(input.granteeEmail.trim().toLowerCase());
    if (!grantee) {
      throw new ValidationError('Grantee must already have an account');
    }
    if (grantee.id === input.actor.userId) {
      throw new ValidationError('Owner access is implicit; do not create a grant for yourself');
    }
    return this.store.upsertAccessGrant({
      deviceId: input.deviceId,
      granteeUserId: grantee.id,
      role: input.role,
      createdByUserId: input.actor.userId,
      now: new Date(),
    });
  }

  async listGrants(actor: AuthenticatedUser, deviceId: string) {
    const device = await this.store.findDeviceById(deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }
    if (device.ownerUserId !== actor.userId && actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only the owner or an admin can view grants');
    }
    return this.store.listDeviceGrants(deviceId);
  }

  async revokeGrant(actor: AuthenticatedUser, grantId: string) {
    const grant = await this.store.findGrantById(grantId);
    if (!grant) {
      throw new NotFoundError('Grant not found');
    }
    const device = await this.store.findDeviceById(grant.deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }
    if (device.ownerUserId !== actor.userId && actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only the owner or an admin can revoke grants');
    }
    return this.store.revokeGrant(grantId, new Date());
  }
}
