import type { Account } from './types';

export function canAdministerOrganization(account: Account) {
  return account.workspace.kind === 'ORGANIZATION' && ['ADMIN', 'OWNER'].includes(account.role);
}
