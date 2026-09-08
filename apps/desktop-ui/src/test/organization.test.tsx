import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { App } from '../App';
import type { DirectoryUser, Group } from '../api/types';
import { account, deferred, fakeApi } from './fixtures';

const admin = { ...account, role: 'ADMIN' };
const self: DirectoryUser = { id: admin.id, email: admin.email, display_name: admin.display_name, role: 'ADMIN', disabled: false };
const member: DirectoryUser = { id: 'member', email: 'member@example.test', display_name: '组织成员甲', role: 'USER', disabled: false };
const group: Group = { id: 'team', name: '设计团队' };
type User = ReturnType<typeof userEvent.setup>;
async function openDirectory(user: User) {
  await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
  await user.click(screen.getByRole('button', { name: '管理成员与用户组' }));
  return screen.getByRole('dialog');
}

describe('organization directory management', () => {
  it('preserves legacy organization OWNER as an administrator superset', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => ({ ...admin, role: 'OWNER' }), users: () => [self] });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    expect(screen.getByText('空间所有者 OWNER')).toBeVisible();
    expect(screen.getByRole('button', { name: '管理组织邀请' })).toBeVisible();
    await user.click(screen.getByRole('button', { name: '管理成员与用户组' }));
    await within(screen.getByRole('dialog')).findByText(self.email);
    expect(api.calls).toContainEqual({ op: 'users' });
  });
  it('lists actual members and confirms disable and enable without offering self-disable', async () => {
    const user = userEvent.setup();
    let people = [self, member];
    const api = fakeApi({ account: () => admin, users: () => people, set_user_disabled: request => { people = people.map(person => person.id === request.user_id ? { ...person, disabled: request.disabled } : person); return null; } });
    render(<App api={api} />);
    const dialog = await openDirectory(user);
    await within(dialog).findByText(member.email);
    expect(within(dialog).getByText('当前账号')).toBeVisible();
    expect(within(dialog).getAllByRole('button', { name: '停用成员' })).toHaveLength(1);
    await user.click(within(dialog).getByRole('button', { name: '停用成员' }));
    expect(api.calls.some(call => call.op === 'set_user_disabled')).toBe(false);
    await user.click(within(dialog).getByRole('button', { name: '确认停用成员' }));
    await within(dialog).findByText('USER · 已停用');
    expect(api.calls).toContainEqual({ op: 'set_user_disabled', user_id: 'member', disabled: true });
    await user.click(within(dialog).getByRole('button', { name: '启用成员' }));
    await user.click(within(dialog).getByRole('button', { name: '确认启用成员' }));
    await within(dialog).findByText('USER · 已启用');
    expect(api.calls).toContainEqual({ op: 'set_user_disabled', user_id: 'member', disabled: false });
  });

  it('creates groups, adds/removes selected organization members and confirms deletion', async () => {
    const user = userEvent.setup();
    let groups: Group[] = [];
    let members: DirectoryUser[] = [];
    const disabled = { ...member, id: 'disabled', email: 'disabled@example.test', disabled: true };
    const api = fakeApi({
      account: () => admin, users: () => [self, member, disabled], groups: () => groups,
      create_group: request => { groups = [{ ...group, name: request.name }]; return groups[0]; },
      group_members: () => members,
      add_group_member: () => { members = [member]; return null; },
      remove_group_member: () => { members = []; return null; },
      delete_group: () => { groups = []; return null; },
    });
    render(<App api={api} />);
    const dialog = await openDirectory(user);
    await user.click(within(dialog).getByRole('button', { name: '用户组' }));
    await within(dialog).findByText('暂无用户组');
    const name = within(dialog).getByLabelText('用户组名称');
    expect(name).toHaveAttribute('autocapitalize', 'none');
    expect(name).toHaveAttribute('autocorrect', 'off');
    expect(name).toHaveAttribute('spellcheck', 'false');
    await user.type(name, group.name);
    await user.click(within(dialog).getByRole('button', { name: '创建用户组' }));
    await within(dialog).findByRole('button', { name: '管理成员' });
    expect(api.calls).toContainEqual({ op: 'create_group', name: group.name });
    await user.click(within(dialog).getByRole('button', { name: '管理成员' }));
    await within(dialog).findByText('组内暂无成员');
    expect(within(dialog).queryByRole('option', { name: /disabled@example.test/ })).not.toBeInTheDocument();
    await user.selectOptions(within(dialog).getByLabelText('组织成员'), 'member');
    await user.click(within(dialog).getByRole('button', { name: '添加到用户组' }));
    await within(dialog).findByRole('button', { name: '移出用户组' });
    expect(api.calls).toContainEqual({ op: 'add_group_member', group_id: group.id, user_id: member.id });
    expect(within(dialog).queryByRole('option', { name: /member@example.test/ })).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole('button', { name: '移出用户组' }));
    expect(api.calls.some(call => call.op === 'remove_group_member')).toBe(false);
    await user.click(within(dialog).getByRole('button', { name: '确认移出用户组' }));
    await within(dialog).findByText('组内暂无成员');
    expect(api.calls).toContainEqual({ op: 'remove_group_member', group_id: group.id, user_id: member.id });
    await user.click(within(dialog).getByRole('button', { name: '返回用户组' }));
    await user.click(await within(dialog).findByRole('button', { name: '删除用户组' }));
    expect(api.calls.some(call => call.op === 'delete_group')).toBe(false);
    expect(within(dialog).getByText(/成员账号不会删除/)).toBeVisible();
    await user.click(within(dialog).getByRole('button', { name: '确认删除用户组' }));
    await within(dialog).findByText('暂无用户组');
    expect(api.calls).toContainEqual({ op: 'delete_group', group_id: group.id });
  });

  it.each([
    account,
    { ...admin, workspace: { ...account.workspace, kind: 'PERSONAL' as const } },
    { ...admin, role: 'OWNER', workspace: { ...account.workspace, kind: 'PERSONAL' as const } },
  ])('does not expose or enumerate the directory for ineligible accounts', async current => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => current });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    expect(screen.queryByRole('button', { name: '管理成员与用户组' })).not.toBeInTheDocument();
    expect(api.calls.some(call => ['users', 'groups', 'group_members'].includes(call.op))).toBe(false);
  });

  it('shows mutation errors without optimistic changes and clears old members when reload fails', async () => {
    const user = userEvent.setup();
    let failRead = false;
    const api = fakeApi({ account: () => admin, users: () => failRead ? Promise.reject(new Error('目录读取失败')) : [self, member], set_user_disabled: () => Promise.reject(new Error('此账号不能停用')) });
    render(<App api={api} />);
    const dialog = await openDirectory(user);
    await user.click(await within(dialog).findByRole('button', { name: '停用成员' }));
    await user.click(within(dialog).getByRole('button', { name: '确认停用成员' }));
    expect(await within(dialog).findByRole('alert')).toHaveTextContent('此账号不能停用');
    expect(within(dialog).getByText('USER · 已启用')).toBeVisible();
    failRead = true;
    await user.click(within(dialog).getByRole('button', { name: '刷新成员' }));
    await within(dialog).findByRole('button', { name: '重新加载成员' });
    expect(within(dialog).getByRole('alert')).toHaveTextContent('目录读取失败');
    expect(within(dialog).queryByText(member.email)).not.toBeInTheDocument();
  });

  it('rejects oversized group names locally and surfaces duplicate group errors', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => admin, users: () => [self], groups: () => [], create_group: () => Promise.reject(new Error('该用户组已存在')) });
    render(<App api={api} />);
    const dialog = await openDirectory(user);
    await user.click(within(dialog).getByRole('button', { name: '用户组' }));
    await within(dialog).findByText('暂无用户组');
    const name = within(dialog).getByLabelText('用户组名称');
    await user.type(name, '组'.repeat(67));
    expect(within(dialog).getByRole('button', { name: '创建用户组' })).toBeDisabled();
    await user.clear(name);
    await user.type(name, 'existing');
    await user.click(within(dialog).getByRole('button', { name: '创建用户组' }));
    expect(await within(dialog).findByRole('alert')).toHaveTextContent('该用户组已存在');
    expect(within(dialog).queryByRole('button', { name: '管理成员' })).not.toBeInTheDocument();
  });

  it('never installs the previous group response after switching to another group', async () => {
    const user = userEvent.setup();
    const stale = deferred<DirectoryUser[]>();
    const api = fakeApi({ account: () => admin, users: () => [self], groups: () => [group, { id: 'other', name: '另一组' }], group_members: request => request.group_id === group.id ? stale.promise : [] });
    render(<App api={api} />);
    const dialog = await openDirectory(user);
    await user.click(within(dialog).getByRole('button', { name: '用户组' }));
    await within(dialog).findByText(group.name);
    await user.click(within(dialog).getAllByRole('button', { name: '管理成员' })[0]);
    await user.click(within(dialog).getByRole('button', { name: '返回用户组' }));
    await within(dialog).findByText('另一组');
    await user.click(within(dialog).getAllByRole('button', { name: '管理成员' })[1]);
    await within(dialog).findByText('组内暂无成员');
    await act(async () => stale.resolve([member]));
    expect(within(dialog).queryByText(member.email)).not.toBeInTheDocument();
    expect(within(dialog).getByText('另一组')).toBeVisible();
  });
});

describe('backend-provided Manager address', () => {
  it('uses a configured address across login and signup without browser persistence', async () => {
    const user = userEvent.setup();
    render(<App api={fakeApi({ account: () => null, connection_settings: () => ({ manager_url: 'https://cloud.example.test' }) })} />);
    await waitFor(() => expect(screen.getByLabelText('工作空间地址')).toHaveValue('https://cloud.example.test'));
    await user.click(screen.getByRole('button', { name: '创建账号' }));
    expect(screen.getByLabelText('工作空间地址')).toHaveValue('https://cloud.example.test');
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
  });
  it('does not overwrite an address already entered while backend config is loading', async () => {
    const user = userEvent.setup();
    const settings = deferred<{ manager_url: string | null }>();
    render(<App api={fakeApi({ account: () => null, connection_settings: () => settings.promise })} />);
    await user.type(await screen.findByLabelText('工作空间地址'), 'https://chosen.example.test');
    await act(async () => settings.resolve({ manager_url: 'https://configured.example.test' }));
    expect(screen.getByLabelText('工作空间地址')).toHaveValue('https://chosen.example.test');
  });
});
