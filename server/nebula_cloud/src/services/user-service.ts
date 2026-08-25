import { ForbiddenError, NotFoundError, ValidationError } from '../domain/errors';
import { AuthenticatedUser, UserRole } from '../domain/types';
import { DataStore } from '../repositories/types';

// Admin-only user management: listing accounts, promoting/demoting the
// ADMIN role after the initial bootstrap (see AuthService.register and
// config.INITIAL_ADMIN_EMAILS for how the very first admin is created), and
// topping up a user's trial "connect" credit (see ConnectService and
// README.md's "Trial credit" section).
export class UserService {
  constructor(private readonly store: DataStore) {}

  async listUsers(actor: AuthenticatedUser) {
    if (actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only an admin can list all users');
    }
    return this.store.listUsers();
  }

  async setUserRole(actor: AuthenticatedUser, targetUserId: string, role: UserRole) {
    if (actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only an admin can change a user\'s role');
    }
    if (targetUserId === actor.userId && role !== 'ADMIN') {
      throw new ValidationError('Admins cannot demote themselves; ask another admin to do it');
    }
    const updated = await this.store.updateUserRole(targetUserId, role);
    if (!updated) {
      throw new NotFoundError('User not found');
    }
    return updated;
  }

  async addCredit(actor: AuthenticatedUser, targetUserId: string, amountSeconds: number) {
    if (actor.role !== 'ADMIN') {
      throw new ForbiddenError('Only an admin can adjust a user\'s credit');
    }
    const updated = await this.store.addUserCredit(targetUserId, amountSeconds);
    if (!updated) {
      throw new NotFoundError('User not found');
    }
    return updated;
  }
}
