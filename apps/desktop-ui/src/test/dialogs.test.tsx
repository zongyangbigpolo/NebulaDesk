import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';
import { ActionDialog, type DialogState } from '../ui/dialogs';
import { account, fakeApi, machine, type Handlers } from './fixtures';
import type { Grant, PublishedResource } from '../api/types';

const published: PublishedResource = { id: 'published', machine_id: machine.id, kind: 'APP', name: '测试应用', description: '原描述', enabled: true, launch_path: '/Applications/Example.app' };
function mount(dialog: DialogState, handlers: Handlers, success = true) {
  const api = fakeApi(handlers);
  const close = vi.fn();
  render(<ActionDialog dialog={dialog} account={account} api={api} busy={false} error={null} onClose={close} onLogout={async () => true} run={async work => { await work(); return success; }} />);
  return { api, close };
}
describe('management command dialogs', () => {
  it('publishes APP metadata without invoking connect', async () => {
    const user = userEvent.setup();
    const { api, close } = mount({ kind: 'publish', machineId: machine.id, resourceKind: 'APP' }, { publish_resource: () => published });
    expect(screen.getByText(/无需选择电脑/)).toBeVisible();
    expect(screen.getByText(/并非独立沙箱/)).toBeVisible();
    expect(screen.getByLabelText('应用启动路径')).toHaveAttribute('autocorrect', 'off');
    expect(screen.getByLabelText('启动参数（每行一个，可选）')).toHaveAttribute('spellcheck', 'false');
    await user.type(screen.getByLabelText('资源名称'), '测试应用');
    await user.type(screen.getByLabelText('描述'), '设计工具');
    await user.type(screen.getByLabelText('应用启动路径'), '/Applications/Example.app');
    await user.type(screen.getByLabelText('启动参数（每行一个，可选）'), '--safe\n--windowed');
    await user.click(screen.getByRole('button', { name: '发布' }));
    expect(api.calls).toContainEqual({ op: 'publish_resource', machine_id: machine.id, resource: { kind: 'APP', name: '测试应用', description: '设计工具', launch_path: '/Applications/Example.app', launch_args: ['--safe', '--windowed'] } });
    expect(api.calls.some(r => r.op === 'connect')).toBe(false);
    expect(close).toHaveBeenCalledOnce();
  });
  it('updates resource fields and enabled state', async () => {
    const user = userEvent.setup();
    const { api } = mount({ kind: 'edit', resource: published }, { update_resource: () => null });
    await user.clear(screen.getByLabelText('资源名称'));
    await user.type(screen.getByLabelText('资源名称'), '新名称');
    await user.click(screen.getByLabelText('允许发布此资源'));
    expect(screen.getByText(/已建立的连接不会立即断开/)).toBeVisible();
    await user.click(screen.getByRole('button', { name: '保存' }));
    expect(api.calls).toContainEqual({ op: 'update_resource', resource_id: published.id, changes: { name: '新名称', description: '原描述', enabled: false } });
  });
  it('does not close a form when the mutation fails', async () => {
    const user = userEvent.setup();
    const { close } = mount({ kind: 'rename', machine }, { rename_machine: () => null }, false);
    await user.click(screen.getByRole('button', { name: '保存' }));
    expect(close).not.toHaveBeenCalled();
    expect(screen.getByRole('dialog', { name: '重命名设备' })).toBeVisible();
  });
  it('only removes a machine after explicit confirmation', async () => {
    const user = userEvent.setup();
    const { api } = mount({ kind: 'remove', machine }, { remove_machine: () => null });
    expect(screen.getByText(/已建立的连接不会立即断开/)).toBeVisible();
    expect(api.calls).toEqual([]);
    await user.click(screen.getByRole('button', { name: '确认移除' }));
    expect(api.calls).toContainEqual({ op: 'remove_machine', machine_id: machine.id });
  });
  it('confirms and revokes the exact entitlement id', async () => {
    const user = userEvent.setup();
    let revokedAt: string | null = null;
    const { api } = mount({ kind: 'access', resourceId: published.id, name: published.name }, {
      grants: () => [{ id: 'entitlement', resource_id: published.id, user_id: 'recipient', group_id: null, role: 'VIEWER', allow_audio: false, allow_clipboard: false, allow_file_transfer: false, revoked_at: revokedAt, expires_at: null }],
      revoke_access: () => { revokedAt = new Date().toISOString(); return null; },
    });
    await user.click(await screen.findByRole('button', { name: '撤销' }));
    expect(screen.getByText(/已建立的连接不会立即断开/)).toBeVisible();
    expect(api.calls.some(r => r.op === 'revoke_access')).toBe(false);
    await user.click(screen.getByRole('button', { name: '确认撤销' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'revoke_access', entitlement_id: 'entitlement' }));
    expect(await screen.findByText('已撤销')).toBeVisible();
    expect(screen.getByText(/历史授权/)).toBeVisible();
    expect(screen.queryByRole('button', { name: '撤销' })).not.toBeInTheDocument();
    expect(screen.queryByText(/当前授权/)).not.toBeInTheDocument();
  });
  it('marks expired grants as historical with no revoke action', async () => {
    mount({ kind: 'access', resourceId: published.id, name: published.name }, {
      grants: () => [{ id: 'expired', resource_id: published.id, user_id: 'recipient', group_id: null, role: 'VIEWER', allow_audio: false, allow_clipboard: false, allow_file_transfer: false, revoked_at: null, expires_at: '2020-01-01T00:00:00Z' }],
    });
    expect(await screen.findByText('已过期')).toBeVisible();
    expect(screen.queryByRole('button', { name: '撤销' })).not.toBeInTheDocument();
  });
  it('expires a grant while the access dialog remains open', async () => {
    vi.useFakeTimers();
    try {
      const grant: Grant = { id: 'expiring', resource_id: published.id, user_id: 'recipient', group_id: null, role: 'VIEWER', allow_audio: false, allow_clipboard: false, allow_file_transfer: false, revoked_at: null, expires_at: new Date(Date.now() + 500).toISOString() };
      mount({ kind: 'access', resourceId: published.id, name: published.name }, { grants: () => [grant] });
      await act(async () => { await Promise.resolve(); });
      expect(screen.getByRole('button', { name: '撤销' })).toBeVisible();
      await act(async () => { vi.advanceTimersByTime(1000); });
      expect(screen.getByText('已过期')).toBeVisible();
      expect(screen.queryByRole('button', { name: '撤销' })).not.toBeInTheDocument();
    } finally { vi.useRealTimers(); }
  });
  it('resets and disables all capability flags when switching to VIEWER', async () => {
    const user = userEvent.setup();
    const { api } = mount({ kind: 'access', resourceId: published.id, name: published.name }, {
      grant_access: request => ({ id: 'grant', user_id: 'recipient', group_id: null, ...request }),
    });
    await screen.findByText('暂无额外授权');
    await user.type(screen.getByLabelText('用户完整邮箱'), 'viewer@example.test');
    await user.selectOptions(screen.getByLabelText('访问角色'), 'CONTROLLER');
    await user.click(screen.getByLabelText('文件传输'));
    await user.click(screen.getByLabelText('音频'));
    await user.selectOptions(screen.getByLabelText('访问角色'), 'VIEWER');
    expect(screen.getByLabelText('文件传输')).toBeDisabled();
    expect(screen.getByLabelText('文件传输')).not.toBeChecked();
    await user.click(screen.getByRole('button', { name: '添加授权' }));
    expect(api.calls).toContainEqual({ op: 'grant_access', resource_id: published.id, email: 'viewer@example.test', role: 'VIEWER', allow_clipboard: false, allow_file_transfer: false, allow_audio: false });
  });
  it('renders enriched recipient names and email without exposing a user directory', async () => {
    mount({ kind: 'access', resourceId: published.id, name: published.name }, {
      grants: () => [{ id: 'grant', resource_id: published.id, user_id: 'opaque-id', user_email: 'recipient@example.test', user_display_name: '收件人', group_id: null, role: 'VIEWER', allow_audio: false, allow_clipboard: false, allow_file_transfer: false }],
    });
    expect(await screen.findByText('收件人')).toBeVisible();
    expect(screen.getByText('recipient@example.test')).toBeVisible();
    expect(screen.queryByText('用户 opaque-id')).not.toBeInTheDocument();
  });
});
