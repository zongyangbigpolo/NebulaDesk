import { render, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it } from 'vitest';
import { App } from '../App';
import { account, fakeApi, host, resource, session } from './fixtures';

describe('desktop presentation and actions', () => {
  it('preserves exact login identifiers instead of applying native spelling corrections', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ account: () => null, login: () => account });
    render(<App api={api} />);
    const tenant = await screen.findByLabelText('工作空间标识');
    const form = tenant.closest('form');
    expect(form).toHaveAttribute('autocapitalize', 'none');
    expect(form).toHaveAttribute('autocorrect', 'off');
    expect(form).toHaveAttribute('spellcheck', 'false');
    await user.type(screen.getByLabelText('工作空间地址'), 'https://manager.test');
    await user.type(tenant, 'acme');
    await user.type(screen.getByLabelText('邮箱'), 'exact@example.test');
    await user.type(screen.getByLabelText('密码'), 'test-password');
    await user.click(screen.getByRole('button', { name: '登录' }));
    await waitFor(() => expect(api.calls).toContainEqual({
      op: 'login', manager_url: 'https://manager.test', tenant: 'acme',
      email: 'exact@example.test', password: 'test-password', allow_insecure_http: false,
    }));
  });
  it('renders empty resources without fabricated examples', async () => {
    render(<App api={fakeApi({ resources: () => [] })} />);
    expect(await screen.findByText('还没有可连接的资源')).toBeInTheDocument();
    expect(screen.queryByText('Photoshop')).not.toBeInTheDocument();
  });
  it('filters search and disables unsupported applications without connecting the desktop', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ resources: () => [resource, { ...resource, id: 'app', name: '不支持的应用', kind: 'APP', launch_supported: false }] });
    render(<App api={api} />);
    await screen.findByText('不支持的应用');
    expect(screen.getByRole('button', { name: '启动' })).toBeDisabled();
    expect(screen.getByText('暂不支持独立应用连接')).toBeVisible();
    await user.type(screen.getByRole('textbox', { name: '搜索资源' }), '找不到');
    expect(screen.getByText('没有匹配的资源')).toBeVisible();
    expect(api.calls.some(call => call.op === 'connect')).toBe(false);
  });
  it('uses typed resource/connect commands and displays failed connection errors', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ connect: () => Promise.reject({ code: 'offline', message: '远端设备已离线' }) });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '打开' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('远端设备已离线');
    expect(api.calls).toContainEqual({ op: 'connect', resource_id: resource.id });
    expect(screen.queryByText('已连接')).not.toBeInTheDocument();
  });
  it('launches a supported application by resource without selecting its host machine', async () => {
    const user = userEvent.setup();
    const application = { ...resource, id: 'published-editor', name: 'Remote Editor', kind: 'APP' as const, machine_id: null, os: null, owned: false };
    const api = fakeApi({
      resources: () => [application],
      connect: () => ({ ...session, resource_id: application.id, name: application.name, state: 'connecting' }),
    });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '启动' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'connect', resource_id: application.id }));
    expect(api.calls.filter(call => call.op === 'connect')).toHaveLength(1);
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    expect(screen.queryByText('暂不支持独立应用连接')).not.toBeInTheDocument();
  });
  it('keeps an asynchronous native failure visible after its window has closed', async () => {
    const user = userEvent.setup();
    let attempted = false;
    const api = fakeApi({
      connect: () => { attempted = true; return { ...session, state: 'connecting' }; },
      sessions: () => attempted ? [{ ...session, state: 'failed', error: 'Check screen recording permission on the remote computer.' }] : [],
    });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '打开' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Check screen recording permission on the remote computer.');
    expect(screen.getByRole('alert')).toHaveTextContent(resource.name);
  });
  it('keeps application keyboard adaptation opt-in and sends only a local profile choice', async () => {
    const user = userEvent.setup();
    const application = { ...resource, kind: 'APP' as const, name: 'Editor', machine_id: null, owned: false };
    const api = fakeApi({ resources: () => [application], resource: () => application });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '查看 Editor 详情' }));
    const selector = await screen.findByRole('combobox', { name: '键盘映射' });
    expect(selector).toHaveValue('physical');
    expect(screen.getByRole('heading', { name: '应用信息' })).toBeVisible();
    expect(screen.queryByRole('heading', { name: '设备信息' })).not.toBeInTheDocument();
    expect(screen.queryByText('系统信息未知')).not.toBeInTheDocument();
    expect(screen.getByText(/下次新连接生效/)).toBeVisible();
    await user.selectOptions(selector, 'terminal');
    await user.click(screen.getByRole('button', { name: '启动应用' }));
    await waitFor(() => expect(api.calls).toContainEqual({
      op: 'connect', resource_id: resource.id, keyboard_profile: 'terminal',
    }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });
  it('provides an explicit confirmed disconnect without requesting application close', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ sessions: () => [session] });
    render(<App api={api} />);
    expect(await screen.findByText('当前连接')).toBeVisible();
    await user.click(screen.getByRole('button', { name: '断开' }));
    const dialog = await screen.findByRole('dialog', { name: '断开会话' });
    expect(dialog).toHaveTextContent('这不会请求退出远端应用');
    expect(api.calls.some(call => call.op === 'disconnect_session')).toBe(false);
    await user.click(within(dialog).getByRole('button', { name: '断开' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'disconnect_session', session_id: session.session_id }));
    expect(api.calls.some(call => call.op === 'connect')).toBe(false);
  });
  it('keeps permissions unknown and requires stop confirmation', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ set_host_enabled: () => ({ ...host, running: false }) });
    render(<App api={api} />);
    await screen.findByRole('button', { name: '打开' });
    await user.click(screen.getByRole('button', { name: '本机共享' }));
    expect(await screen.findByText('屏幕录制：未知')).toBeVisible();
    expect(screen.getByText('辅助功能：未授权')).toBeVisible();
    const toggle = screen.getByRole('switch', { name: '允许远程连接' });
    await waitFor(() => expect(toggle).toBeEnabled());
    await user.click(toggle);
    expect(api.calls.some(r => r.op === 'set_host_enabled')).toBe(false);
    await user.click(screen.getByRole('button', { name: '停止共享' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'set_host_enabled', enabled: false }));
  });
  it('hides owner management from shared resource consumers', async () => {
    const user = userEvent.setup();
    const shared = { ...resource, owned: false, machine_id: null };
    render(<App api={fakeApi({ resources: () => [shared], resource: () => shared })} />);
    await user.click(await screen.findByRole('button', { name: `查看 ${resource.name} 详情` }));
    await screen.findByRole('button', { name: '连接桌面' });
    expect(screen.queryByRole('button', { name: '重命名' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '移除设备' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '管理访问权限' })).not.toBeInTheDocument();
  });
  it('opens only the selected native permission settings and surfaces unsupported errors', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ open_permission_settings: () => Promise.reject({ code: 'unsupported', message: '请手动打开系统设置' }) });
    render(<App api={api} />);
    await screen.findByRole('button', { name: '打开' });
    await user.click(screen.getByRole('button', { name: '本机共享' }));
    const button = await screen.findByRole('button', { name: '打开屏幕录制系统设置' });
    await waitFor(() => expect(button).toBeEnabled());
    await user.click(button);
    expect(await screen.findByRole('alert')).toHaveTextContent('请手动打开系统设置');
    expect(api.calls).toContainEqual({ op: 'open_permission_settings', permission: 'screen' });
  });
  it('sends exact email grants with independent permission choices', async () => {
    const user = userEvent.setup();
    const api = fakeApi({
      grant_access: request => ({ id: 'grant', resource_id: request.resource_id, user_id: 'recipient-id', group_id: null, role: request.role, allow_audio: request.allow_audio, allow_clipboard: request.allow_clipboard, allow_file_transfer: request.allow_file_transfer }),
    });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: `查看 ${resource.name} 详情` }));
    await user.click(await screen.findByRole('button', { name: '管理访问权限' }));
    const dialog = screen.getByRole('dialog');
    await screen.findByText('暂无额外授权');
    await user.type(within(dialog).getByLabelText('用户完整邮箱'), 'exact@example.test');
    await user.selectOptions(within(dialog).getByLabelText('访问角色'), 'CONTROLLER');
    await user.click(within(dialog).getByLabelText('剪贴板'));
    await user.click(within(dialog).getByRole('button', { name: '添加授权' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'grant_access', resource_id: resource.id, email: 'exact@example.test', role: 'CONTROLLER', allow_clipboard: true, allow_file_transfer: false, allow_audio: false }));
  });
  it('enrolls this computer with a transient backend-created token', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ create_enrollment: () => ({ token: 'transient', expires_at: '2026-09-08T10:00:00Z' }), enroll_local: () => host });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '添加设备' }));
    const dialog = screen.getByRole('dialog');
    await user.type(within(dialog).getByLabelText('设备名称'), '我的新电脑');
    await user.click(within(dialog).getByRole('button', { name: '添加设备' }));
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'enroll_local', manager_url: account.manager_url, name: '我的新电脑', token: 'transient', allow_insecure_http: false }));
    expect(screen.queryByText('transient')).not.toBeInTheDocument();
    expect(localStorage.length).toBe(0);
  });
  it('opens a native picker only for a connected authorized session', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ sessions: () => [session], send_files: () => null });
    render(<App api={api} />);
    await screen.findByRole('button', { name: '打开' });
    await waitFor(() => expect(api.calls.some(r => r.op === 'sessions')).toBe(true));
    await user.click(screen.getByRole('button', { name: '文件传输' }));
    const send = await screen.findByRole('button', { name: '选择文件发送' });
    await user.click(send);
    await waitFor(() => expect(api.calls).toContainEqual({ op: 'send_files', session_id: session.session_id }));
    expect(screen.getByText('还没有传输记录')).toBeVisible();
  });
  it('requires logout confirmation and clears the authenticated UI', async () => {
    const user = userEvent.setup();
    const api = fakeApi();
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    await user.click(screen.getByRole('button', { name: '退出登录' }));
    expect(api.calls.some(r => r.op === 'logout')).toBe(false);
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: '退出登录' }));
    expect(await screen.findByRole('heading', { name: '登录工作空间' })).toBeVisible();
    expect(screen.queryByRole('navigation')).not.toBeInTheDocument();
  });
  it('never offers global file transfer for an application even with stale grant metadata', async () => {
    const user = userEvent.setup();
    const api = fakeApi({
      resources: () => [{ ...resource, kind: 'APP', policy: { ...resource.policy, file_transfer: true } }],
      sessions: () => [session],
    });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: '文件传输' }));
    const send = await screen.findByRole('button', { name: '选择文件发送' });
    expect(send).toBeDisabled();
    await user.click(send);
    expect(api.calls.some(call => call.op === 'send_files')).toBe(false);
  });
  it('returns to login with a warning, not stale private data, when logout rejects', async () => {
    const user = userEvent.setup();
    const api = fakeApi({ logout: () => Promise.reject({ code: 'timeout', message: '退出请求超时' }) });
    render(<App api={api} />);
    await user.click(await screen.findByRole('button', { name: /账号与设置/ }));
    await user.click(screen.getByRole('button', { name: '退出登录' }));
    await user.click(within(screen.getByRole('dialog')).getByRole('button', { name: '退出登录' }));
    expect(await screen.findByRole('heading', { name: '登录工作空间' })).toBeVisible();
    expect(screen.getByRole('alert')).toHaveTextContent('未能确认退出操作或服务器会话撤销');
    expect(screen.getByRole('alert')).toHaveTextContent('退出请求超时');
    expect(screen.queryByRole('navigation')).not.toBeInTheDocument();
    expect(screen.queryByText(resource.name)).not.toBeInTheDocument();
    expect(screen.queryByText(account.email)).not.toBeInTheDocument();
  });
});
