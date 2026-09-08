import { act, render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { App } from '../App';
import type { Invitation, Workspace } from '../api/types';
import { account, deferred, fakeApi, host } from './fixtures';

const admin = { ...account, role: 'ADMIN' };
const password = 'independent-password-123';
const invitation: Invitation = { id: 'invite', email: 'member@example.test', created_at: '2026-01-01T00:00:00Z', expires_at: '2099-01-01T00:00:00Z', accepted_at: null, revoked_at: null };
type User = ReturnType<typeof userEvent.setup>;

async function beginSignup(user: User, invite = false) {
  await user.click(await screen.findByRole('button', { name: invite ? '我有组织邀请' : '创建账号' }));
  await user.type(screen.getByLabelText('工作空间地址'), 'https://manager.example.test');
  await user.click(screen.getByRole('button', { name: '继续' }));
  await screen.findByLabelText('显示名称');
}
async function fillIdentity(user: User, invite = false) {
  await user.type(screen.getByLabelText('显示名称'), '新用户');
  await user.type(screen.getByLabelText(invite ? '受邀邮箱' : '邮箱'), 'member@example.test');
  await user.type(screen.getByLabelText('密码'), password);
  await user.type(screen.getByLabelText('确认密码'), password);
}

describe('account onboarding', () => {
  it.each(['PERSONAL', 'ORGANIZATION'] as const)('creates a new %s workspace, logs in, and confirms fixed device ownership', async (kind: Workspace['kind']) => {
    const user = userEvent.setup();
    const created = { ...admin, id: 'new-user', display_name: '新用户', email: 'member@example.test', tenant: 'new-space', workspace: { ...account.workspace, slug: 'new-space', name: '新空间', kind } };
    const api = fakeApi({ account: () => null, registration_options: () => ({ self_registration_enabled: true }), register: () => created, resources: () => [], machines: () => [], local_host: () => ({ ...host, enrolled: false, machine_id: null }) });
    render(<App api={api} />);
    await beginSignup(user);
    await user.selectOptions(screen.getByLabelText('工作空间类型'), kind);
    await user.type(screen.getByLabelText('工作空间名称'), '新空间');
    const slug = screen.getByLabelText('工作空间标识');
    expect(slug).toHaveAttribute('autocapitalize', 'none');
    expect(slug).toHaveAttribute('autocorrect', 'off');
    expect(slug).toHaveAttribute('spellcheck', 'false');
    await user.type(slug, 'new-space');
    await fillIdentity(user);
    await user.click(screen.getByRole('button', { name: '创建并登录' }));
    await screen.findByRole('navigation');
    expect(api.calls).toContainEqual({ op: 'register', manager_url: account.manager_url, allow_insecure_http: false, workspace_slug: 'new-space', workspace_name: '新空间', workspace_kind: kind, display_name: '新用户', email: 'member@example.test', password });
    expect(api.calls.some(call => call.op === 'login')).toBe(false);
    expect(screen.getByText(/登录标识 new-space/)).toBeVisible();
    expect(api.calls.some(call => call.op === 'enroll_local' || call.op === 'set_host_enabled')).toBe(false);
    await user.click(screen.getByRole('button', { name: '添加设备' }));
    const confirmation = screen.getByLabelText('设备归属确认');
    expect(confirmation).toHaveTextContent('新空间');
    expect(confirmation).toHaveTextContent('new-space');
    expect(confirmation).toHaveTextContent('新用户 · member@example.test');
    expect(within(screen.getByRole('dialog')).queryByRole('textbox', { name: /所有者/ })).not.toBeInTheDocument();
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
  });

  it('joins only through an exact invitation, with no caller-selected workspace or role', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => null, accept_invitation: () => account });
    render(<App api={api} />);
    await beginSignup(user, true);
    expect(api.calls.some(call => call.op === 'registration_options')).toBe(false);
    const token = screen.getByLabelText('组织邀请码');
    expect(token).toHaveAttribute('maxlength', '128');
    for (const field of [token, screen.getByLabelText('受邀邮箱')]) {
      expect(field).toHaveAttribute('autocapitalize', 'none');
      expect(field).toHaveAttribute('autocorrect', 'off');
      expect(field).toHaveAttribute('spellcheck', 'false');
    }
    expect(screen.queryByLabelText('工作空间标识')).not.toBeInTheDocument();
    expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
    await user.type(token, 'exact-Invite_CODE');
    await fillIdentity(user, true);
    await user.click(screen.getByRole('button', { name: '接受邀请并登录' }));
    await screen.findByRole('navigation');
    expect(api.calls).toContainEqual({ op: 'accept_invitation', manager_url: account.manager_url, allow_insecure_http: false, token: 'exact-Invite_CODE', display_name: '新用户', email: 'member@example.test', password });
    await user.click(screen.getByRole('button', { name: /账号与设置/ }));
    expect(screen.getByText(/下次登录请使用标识 test-space/)).toBeVisible();
    expect(screen.queryByRole('button', { name: '管理组织邀请' })).not.toBeInTheDocument();
  });

  it('keeps disabled self-registration distinct from invitation joining and requires HTTP opt-in', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => null, registration_options: () => ({ self_registration_enabled: false }) });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '创建账号' }));
    await user.type(screen.getByLabelText('工作空间地址'), 'http://127.0.0.1:8080');
    expect(screen.getByRole('button', { name: '继续' })).toBeDisabled();
    await user.click(screen.getByRole('checkbox'));
    await user.click(screen.getByRole('button', { name: '继续' }));
    expect(await screen.findByRole('status')).toHaveTextContent('未开放自行注册');
    expect(screen.queryByRole('button', { name: '创建并登录' })).not.toBeInTheDocument();
    expect(api.calls).toContainEqual({ op: 'registration_options', manager_url: 'http://127.0.0.1:8080', allow_insecure_http: true });
    await user.click(screen.getByRole('button', { name: '我有组织邀请' }));
    expect(screen.getByRole('heading', { name: '凭邀请加入组织' })).toBeVisible();
  });

  it('ignores a capability response for an edited server and exposes retryable capability errors', async () => {
    const user = userEvent.setup();
    const old = deferred<{ self_registration_enabled: boolean }>();
    let reads = 0;
    const api = fakeApi({ account: () => null, registration_options: () => ++reads === 1 ? old.promise : Promise.reject(new Error('无法连接服务器')) });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '创建账号' }));
    const server = screen.getByLabelText('工作空间地址');
    await user.type(server, 'https://old.test');
    await user.click(screen.getByRole('button', { name: '继续' }));
    await user.clear(server);
    await user.type(server, 'https://new.test');
    await act(async () => old.resolve({ self_registration_enabled: true }));
    expect(screen.queryByLabelText('工作空间名称')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '继续' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('无法连接服务器');
    expect(screen.queryByLabelText('密码')).not.toBeInTheDocument();
  });

  it('validates identifier and password before sending and never turns duplicate errors into success', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => null, registration_options: () => ({ self_registration_enabled: true }), register: () => Promise.reject({ code: 'conflict', message: '该工作空间标识已存在' }) });
    render(<App api={api} />);
    await beginSignup(user);
    await user.type(screen.getByLabelText('工作空间名称'), '新空间');
    await user.type(screen.getByLabelText('工作空间标识'), 'Acme');
    await fillIdentity(user);
    expect(screen.getByLabelText('工作空间标识')).toBeInvalid();
    await user.click(screen.getByRole('button', { name: '创建并登录' }));
    expect(api.calls.some(call => call.op === 'register')).toBe(false);
    await user.clear(screen.getByLabelText('工作空间标识'));
    await user.type(screen.getByLabelText('工作空间标识'), 'acme');
    await user.clear(screen.getByLabelText('确认密码'));
    await user.type(screen.getByLabelText('确认密码'), 'different');
    expect(screen.getByRole('button', { name: '创建并登录' })).toBeDisabled();
    await user.clear(screen.getByLabelText('确认密码'));
    await user.type(screen.getByLabelText('确认密码'), password);
    await user.click(screen.getByRole('button', { name: '创建并登录' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('已存在');
    expect(screen.queryByRole('navigation')).not.toBeInTheDocument();
    expect(screen.getByLabelText('密码')).toHaveValue('');
    expect(screen.getByLabelText('确认密码')).toHaveValue('');
  });

  it('shows invitation acceptance failure, erases secret inputs, and retains no authenticated data', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => null, accept_invitation: () => Promise.reject(new Error('邀请码已过期或邮箱不匹配')) });
    render(<App api={api} />);
    await beginSignup(user, true);
    await user.type(screen.getByLabelText('组织邀请码'), 'expired-code');
    await fillIdentity(user, true);
    await user.click(screen.getByRole('button', { name: '接受邀请并登录' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('邀请码已过期或邮箱不匹配');
    expect(screen.getByLabelText('组织邀请码')).toHaveValue('');
    expect(screen.getByLabelText('密码')).toHaveValue('');
    expect(screen.queryByRole('navigation')).not.toBeInTheDocument();
  });
});

describe('organization invitations and actual local binding', () => {
  it('creates a selectable one-time code, lists metadata, and revokes only active invitations', async () => {
    const user = userEvent.setup();
    let items: Invitation[] = [
      { ...invitation, id: 'used', email: 'used@example.test', accepted_at: '2026-02-01T00:00:00Z' },
      { ...invitation, id: 'revoked', email: 'revoked@example.test', revoked_at: '2026-02-01T00:00:00Z' },
      { ...invitation, id: 'expired', email: 'expired@example.test', expires_at: '2000-01-01T00:00:00Z' },
    ];
    const api = fakeApi({ account: () => admin, invitations: () => items, create_invitation: () => { items = [...items, invitation]; return { ...invitation, token: 'visible-once-only' }; }, revoke_invitation: () => { items = items.map(item => ({ ...item, revoked_at: '2026-02-01T00:00:00Z' })); return null; } });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    await user.click(screen.getByRole('button', { name: '管理组织邀请' }));
    await screen.findByText('used@example.test');
    expect(screen.getByRole('button', { name: '不可撤销 · 已使用' })).toBeDisabled();
    expect(screen.getByRole('button', { name: '不可撤销 · 已撤销' })).toBeDisabled();
    expect(screen.getByRole('button', { name: '不可撤销 · 已过期' })).toBeDisabled();
    await user.type(screen.getByLabelText('受邀邮箱'), invitation.email);
    await user.click(screen.getByRole('button', { name: '创建邀请码' }));
    const code = await screen.findByLabelText('一次性邀请码');
    expect(code).toHaveValue('visible-once-only');
    expect(code).toHaveAttribute('readonly');
    expect(api.calls).toContainEqual({ op: 'create_invitation', email: invitation.email });
    await user.click(screen.getByRole('button', { name: '我已保存，隐藏邀请码' }));
    expect(screen.queryByDisplayValue('visible-once-only')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '刷新邀请' }));
    await waitFor(() => expect(screen.getByRole('button', { name: '撤销邀请' })).toBeEnabled());
    expect(screen.queryByLabelText('一次性邀请码')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: '撤销邀请' }));
    expect(api.calls.some(call => call.op === 'revoke_invitation')).toBe(false);
    await user.click(screen.getByRole('button', { name: '确认撤销邀请' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'revoke_invitation', id: 'invite' }));
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(0);
  });

  it.each([
    { ...account, role: 'USER' },
    { ...admin, workspace: { ...account.workspace, kind: 'PERSONAL' as const } },
  ])('hides organization invitation controls from ineligible accounts', async current => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => current });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    expect(screen.queryByRole('button', { name: '管理组织邀请' })).not.toBeInTheDocument();
    expect(api.calls.some(call => call.op === 'invitations')).toBe(false);
  });

  it('clears stale invitation records after a failed reload and shows create errors without a code', async () => {
    const user = userEvent.setup();
    let fail = false;
    const api = fakeApi({ account: () => admin, invitations: () => fail ? Promise.reject(new Error('邀请记录加载失败')) : [invitation], create_invitation: () => Promise.reject(new Error('该成员已存在')) });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    await user.click(screen.getByRole('button', { name: '管理组织邀请' }));
    await screen.findByText(invitation.email);
    await user.type(screen.getByLabelText('受邀邮箱'), 'other@example.test');
    await user.click(screen.getByRole('button', { name: '创建邀请码' }));
    expect(await within(screen.getByRole('dialog')).findByRole('alert')).toHaveTextContent('该成员已存在');
    expect(screen.queryByLabelText('一次性邀请码')).not.toBeInTheDocument();
    fail = true;
    await user.click(screen.getByRole('button', { name: '刷新邀请' }));
    await screen.findByRole('button', { name: '重新加载邀请' });
    expect(within(screen.getByRole('dialog')).getByRole('alert')).toHaveTextContent('邀请记录加载失败');
    expect(screen.queryByText(invitation.email)).not.toBeInTheDocument();
  });

  it('does not claim a matching device ID at a different Manager is owned by the current account', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ local_host: () => ({ ...host, manager_url: 'https://other-manager.test' }) });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '本机共享' }));
    const binding = await screen.findByLabelText('本机实际绑定');
    expect(binding).toHaveTextContent('https://other-manager.test');
    expect(binding).toHaveTextContent('其他工作空间 / 所有者未知');
    expect(binding).not.toHaveTextContent('已核对当前空间设备记录');
    expect(screen.getByRole('switch')).toBeDisabled();
    expect(api.calls.some(call => call.op === 'machine_resources')).toBe(false);
    await user.click(screen.getByRole('button', { name: /账号与设置/ }));
    expect(screen.getByLabelText('本机实际绑定')).toHaveTextContent('所有者未知');
  });
});
