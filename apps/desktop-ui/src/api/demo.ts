import type { Account, Commands, DesktopApi, Grant, LocalHost, Machine, PublishedResource, Request, Resource, Session, Transfer } from './types';

// This module is imported only for the explicit browser demo, never as an IPC fallback.
export function createDemoApi(): DesktopApi {
  let account: Account | null = { id: 'demo-owner', email: 'polo@example.test', display_name: 'Polo', role: 'owner', tenant: '个人空间', manager_url: 'https://manager.example.test' };
  let host: LocalHost = { enrolled: true, machine_id: 'local', manager_url: account.manager_url, name: 'Polo 的 MacBook Pro', running: true, permissions: { screen: 'unknown', input: 'unknown', audio: 'unknown' }, error: null };
  let machines: Machine[] = [
    { id: 'office', name: '办公室 Mac', os: 'macOS', os_version: '26', arch: 'arm64', status: 'ONLINE', owner_user_id: 'demo-owner', last_seen_at: new Date().toISOString(), capabilities: {} },
    { id: 'studio', name: '工作室电脑', os: 'Windows', os_version: '11', arch: 'x86_64', status: 'OFFLINE', owner_user_id: 'demo-owner', last_seen_at: '2026-09-07T10:42:00Z', capabilities: {} },
    { id: 'local', name: host.name!, os: 'macOS', os_version: '26', arch: 'arm64', status: 'ONLINE', owner_user_id: 'demo-owner', last_seen_at: null, capabilities: {} },
  ];
  let resources: Resource[] = [
    { id: 'office-desktop', name: '办公室 Mac', kind: 'DESKTOP', description: '随时回到熟悉的工作桌面。', machine_status: 'ONLINE', role: 'controller', policy: { input: true, audio: true, clipboard: true, file_transfer: true }, owner_name: 'Polo', owned: true, machine_id: 'office', os: 'macOS', os_version: '26', last_seen_at: new Date().toISOString(), launch_supported: true },
    { id: 'photoshop', name: 'Photoshop', kind: 'APP', description: '设计工具', machine_status: 'ONLINE', role: 'viewer', policy: { input: false, audio: false, clipboard: false, file_transfer: false }, owner_name: '林辰', owned: false, machine_id: null, os: null, os_version: null, last_seen_at: null, launch_supported: false },
    { id: 'studio-desktop', name: '工作室电脑', kind: 'DESKTOP', description: '工作室', machine_status: 'OFFLINE', role: 'controller', policy: { input: true, audio: true, clipboard: true, file_transfer: true }, owner_name: 'Polo', owned: true, machine_id: 'studio', os: 'Windows', os_version: '11', last_seen_at: '2026-09-07T10:42:00Z', launch_supported: true },
    ...['Excel', 'Blender'].map((name): Resource => ({ id: name.toLowerCase(), name, kind: 'APP', description: name === 'Excel' ? '财务工具' : '三维工作室', machine_status: 'ONLINE', role: null, policy: { input: false, audio: false, clipboard: false, file_transfer: false }, owner_name: null, owned: false, machine_id: null, os: null, os_version: null, last_seen_at: null, launch_supported: false })),
  ];
  let published: PublishedResource[] = [
    { id: 'local-desktop', machine_id: 'local', name: '完整桌面', kind: 'DESKTOP', description: '本机桌面', enabled: true, launch_path: null },
    { id: 'local-ps', machine_id: 'local', name: 'Photoshop', kind: 'APP', description: '设计工具', enabled: true, launch_path: '/Applications/Adobe Photoshop.app' },
    { id: 'local-blender', machine_id: 'local', name: 'Blender', kind: 'APP', description: '三维工作室', enabled: false, launch_path: '/Applications/Blender.app' },
  ];
  let sessions: Session[] = [{ session_id: 'demo-session', resource_id: 'office-desktop', name: '办公室 Mac', state: 'connected', path: 'direct', started_at: new Date(Date.now() - 24 * 60000).toISOString(), error: null, rtt_ms: 3 }];
  let transfers: Transfer[] = [];
  let grants: Grant[] = [];
  async function dispatch(request: Request): Promise<unknown> {
    await new Promise(resolve => setTimeout(resolve, 180));
    if (!account && !['account', 'login', 'logout'].includes(request.op)) throw new Error('请先登录。');
    switch (request.op) {
      case 'account': return account;
      case 'login': return account = { id: 'demo-owner', display_name: request.email.split('@')[0], email: request.email, manager_url: request.manager_url, tenant: request.tenant, role: 'owner' };
      case 'logout': account = null; sessions = []; transfers = []; return null;
      case 'resources': return [...resources];
      case 'machines': return [...machines];
      case 'resource': {
        const resource = resources.find(r => r.id === request.id);
        if (!resource) throw new Error('资源已不存在。');
        return { ...resource };
      }
      case 'local_host': return { ...host };
      case 'create_enrollment': return { token: 'demo-only-not-a-credential', expires_at: new Date(Date.now() + 600000).toISOString() };
      case 'enroll_local': host = { ...host, enrolled: true, name: request.name, manager_url: request.manager_url, machine_id: 'local', running: false }; return host;
      case 'set_host_enabled': host = { ...host, running: request.enabled }; return host;
      case 'sessions': return [...sessions];
      case 'transfers': return [...transfers];
      case 'connect': {
        const resource = resources.find(r => r.id === request.resource_id);
        if (!resource || !resource.launch_supported || resource.machine_status !== 'ONLINE') throw new Error('此资源目前无法连接。');
        const existing = sessions.find(s => s.resource_id === resource.id && ['connected', 'connecting'].includes(s.state));
        if (existing) return existing;
        const session: Session = { session_id: crypto.randomUUID(), resource_id: resource.id, name: resource.name, state: 'connecting', path: null, started_at: new Date().toISOString(), error: null, rtt_ms: null };
        sessions = [...sessions, session];
        return session;
      }
      case 'focus_session': if (!sessions.some(s => s.session_id === request.session_id)) throw new Error('会话已结束。'); return null;
      case 'disconnect_session': sessions = sessions.map(s => s.session_id === request.session_id ? { ...s, state: 'disconnected' } : s); return null;
      case 'send_files': transfers = [...transfers, { id: crypto.randomUUID(), session_id: request.session_id, name: '演示文件.pdf', direction: 'send', total: 2400000, transferred: 2400000, state: 'complete', error: null }]; return null;
      case 'rename_machine': machines = machines.map(m => m.id === request.machine_id ? { ...m, name: request.name } : m); resources = resources.map(r => r.machine_id === request.machine_id && r.kind === 'DESKTOP' ? { ...r, name: request.name } : r); if (host.machine_id === request.machine_id) host = { ...host, name: request.name }; return null;
      case 'remove_machine': machines = machines.filter(m => m.id !== request.machine_id); resources = resources.filter(r => r.machine_id !== request.machine_id); if (host.machine_id === request.machine_id) host = { ...host, enrolled: false, machine_id: null, running: false }; return null;
      case 'machine_resources': return published.filter(r => r.machine_id === request.machine_id);
      case 'publish_resource': {
        const resource: PublishedResource = { ...request.resource, id: crypto.randomUUID(), machine_id: request.machine_id, enabled: true, launch_path: request.resource.launch_path ?? null };
        published = [...published, resource]; return resource;
      }
      case 'update_resource': published = published.map(r => r.id === request.resource_id ? { ...r, ...request.changes } : r); return null;
      case 'grants': return grants.filter(g => g.resource_id === request.resource_id);
      case 'grant_access': {
        const grant: Grant = { id: crypto.randomUUID(), resource_id: request.resource_id, user_id: crypto.randomUUID(), user_email: request.email, group_id: null, role: request.role, allow_audio: request.allow_audio, allow_clipboard: request.allow_clipboard, allow_file_transfer: request.allow_file_transfer, revoked_at: null, expires_at: null };
        grants = [...grants, grant]; return grant;
      }
      case 'revoke_access': grants = grants.map(g => g.id === request.entitlement_id ? { ...g, revoked_at: new Date().toISOString() } : g); return null;
      case 'open_permission_settings': return null;
    }
  }
  return { async request<K extends keyof Commands>(request: Request<K>) {
    // The exhaustive discriminated dispatch mirrors the IPC command/result boundary.
    return await dispatch(request as Request) as Commands[K]['result'];
  } };
}
