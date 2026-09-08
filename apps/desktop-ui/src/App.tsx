import { useCallback, useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { ArrowRightLeft, ArrowUpRight, ChevronRight, CircleHelp, Grid2X2, LogOut, Monitor, RefreshCw, Search, Settings } from 'lucide-react';
import type { Account, DesktopApi, Resource, Session } from './api/types';
import { canAdministerOrganization } from './api/permissions';
import { DesktopStore } from './state/store';
import { useQuery } from './state/useQuery';
import { dateLabel, Empty, ErrorNotice, Loading, Modal, sessionLabels, shortcutLabel, unavailable } from './ui/common';
import { Resources, type ResourceFilter } from './ui/resources';
import { LocalBinding, Sharing } from './ui/sharing';
import { Details } from './ui/details';
import { Transfers } from './ui/transfers';
import { ActionDialog, type DialogState } from './ui/dialogs';
import { Login } from './ui/login';

export function App({ api, demo = false }: { api: DesktopApi; demo?: boolean }) {
  const [store] = useState(() => new DesktopStore(api));
  const state = useSyncExternalStore(store.subscribe, store.snapshot);
  useEffect(() => { void store.start(); }, [store]);
  useEffect(() => {
    const poll = () => { if (document.visibilityState !== 'hidden') void store.poll(); };
    poll();
    const timer = window.setInterval(poll, 3000);
    document.addEventListener('visibilitychange', poll);
    return () => { clearInterval(timer); document.removeEventListener('visibilitychange', poll); };
  }, [store, state.account?.id]);
  return <>{demo && <div className="demo-badge">演示模式 · 示例数据，不连接真实设备</div>}{!state.ready ? <Loading /> : !state.account ? <Login api={api} busy={state.busy} error={state.error} demo={demo} onAuthenticate={request => store.authenticate(request)} clearError={store.clearError} /> : <Workspace key={`${state.account.id}:${state.account.manager_url}:${state.account.workspace.id}`} store={store} account={state.account} demo={demo} />}</>;
}

function Workspace({ store, account, demo }: { store: DesktopStore; account: Account; demo: boolean }) {
  const state = useSyncExternalStore(store.subscribe, store.snapshot);
  const api = store.api;
  const [page, setPage] = useState<'resources' | 'sharing' | 'transfers' | 'settings'>('resources');
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [dialog, setDialog] = useState<DialogState | null>(null);
  const [help, setHelp] = useState(false);
  const [demoFocus, setDemoFocus] = useState(false);
  const [demoPermission, setDemoPermission] = useState(false);
  const [filter, setFilter] = useState<ResourceFilter>('ALL');
  const [sort, setSort] = useState('recent');
  const [search, setSearch] = useState('');
  const searchRef = useRef<HTMLInputElement>(null);
  const resourceLoad = useCallback(() => selectedId ? api.request({ op: 'resource', id: selectedId }) : Promise.resolve(null), [api, selectedId]);
  const detail = useQuery(resourceLoad);
  const hostId = state.host?.machine_id;
  const sameManager = !!state.host?.manager_url && state.host.manager_url.replace(/\/+$/, '') === account.manager_url.replace(/\/+$/, '');
  const hostMachine = sameManager ? state.machines.find(m => m.id === hostId) : undefined;
  const canManageLocal = !!state.host?.enrolled && !!hostId && sameManager && (hostMachine?.owner_user_id === account.id || state.resources.some(r => r.machine_id === hostId && r.owned));
  const publishedLoad = useCallback(() => page === 'sharing' && hostId && canManageLocal ? api.request({ op: 'machine_resources', machine_id: hostId }) : Promise.resolve([]), [api, hostId, page, canManageLocal]);
  const published = useQuery(publishedLoad);
  const active = state.sessions.filter(s => ['connected', 'connecting'].includes(s.state));
  useEffect(() => {
    function keydown(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k' && !dialog && !help) {
        event.preventDefault(); setPage('resources'); setSelectedId(null); searchRef.current?.focus();
      }
    }
    window.addEventListener('keydown', keydown);
    return () => window.removeEventListener('keydown', keydown);
  }, [dialog, help]);
  function navigate(next: typeof page) { setPage(next); setSelectedId(null); }
  function openDialog(next: DialogState) { store.clearError(); setDialog(next); }
  function closeDialog() { setDialog(null); published.reload(); detail.reload(); }
  async function focus(session: Session) {
    if (await store.action(() => api.request({ op: 'focus_session', session_id: session.session_id }), false) && demo) setDemoFocus(true);
  }
  async function connect(resource: Resource) {
    if (unavailable(resource)) return;
    if (await store.action(() => api.request({ op: 'connect', resource_id: resource.id }), false) && demo) setDemoFocus(true);
  }
  const detailResource = detail.data;
  const detailMachine = state.machines.find(m => m.id === detailResource?.machine_id);
  const nav = [{ id: 'resources', label: '我的资源', Icon: Grid2X2 }, { id: 'sharing', label: '本机共享', Icon: Monitor }, { id: 'transfers', label: '文件传输', Icon: ArrowRightLeft }] as const;
  return <div className="app-shell">
    <aside className="sidebar"><div className="brand"><span className="brand-mark">N</span>NebulaDesk</div><div className="sidebar-label">工作空间</div><nav aria-label="主导航">{nav.map(({ id, label, Icon }) => <button key={id} className={page === id ? 'selected' : ''} aria-current={page === id ? 'page' : undefined} onClick={() => navigate(id)}><Icon size={18} />{label}{id === 'resources' && <span className="count">{state.resources.length}</span>}</button>)}</nav><div className="sidebar-rule" /><div className="sidebar-label">当前会话</div><div className="sidebar-sessions">{!active.length ? <p className="muted">暂无连接</p> : active.map(session => <button key={session.session_id} disabled={state.busy} onClick={() => void focus(session)}><span className={`dot ${session.state === 'connected' ? 'green' : ''}`} /><span className="grow">{session.name}<small>{sessionLabels[session.state]} · 点击返回窗口</small></span><ArrowUpRight size={14} /></button>)}</div><div className="workspace-status"><span className={`dot ${state.error ? '' : 'green'}`} /><div>{state.error ? '工作空间需要关注' : '已登录工作空间'}<small>{account.tenant}</small></div></div><button className="account-button" onClick={() => navigate('settings')}><span className="avatar">{(account.display_name || account.email).slice(0, 2).toUpperCase()}</span><span className="grow"><strong>{account.display_name || account.email}</strong><small>账号与设置</small></span><ChevronRight size={15} /></button></aside>
    <div className="main-column"><header className="toolbar"><span>{account.tenant}</span><div className="search-field"><Search size={15} /><input ref={searchRef} aria-label="搜索资源" placeholder="搜索资源" value={search} onChange={e => { setSearch(e.target.value); navigate('resources'); }} /><kbd>{shortcutLabel(navigator.platform)}</kbd></div><button className="icon-button" aria-label="刷新" disabled={state.busy || state.loading} onClick={() => { void store.refresh(); void store.poll(); detail.reload(); published.reload(); }}><RefreshCw size={16} /></button><button className="icon-button" aria-label="帮助" onClick={() => setHelp(true)}><CircleHelp size={17} /></button></header>
    <main className="content"><ErrorNotice message={state.error} />{state.loading && <Loading />}
      {page === 'resources' && !selectedId && <p className="workspace-identity">{account.workspace.name} · {account.workspace.kind === 'PERSONAL' ? '个人空间' : '组织'} · 当前账号 {account.display_name || account.email} · 登录标识 {account.tenant}</p>}
      {page === 'resources' && !selectedId && <Resources resources={state.resources} sessions={state.sessions} query={search} filter={filter} sort={sort} busy={state.busy} onFilter={setFilter} onSort={setSort} onAdd={() => openDialog({ kind: 'enroll' })} onDetails={resource => setSelectedId(resource.id)} onConnect={resource => void connect(resource)} onFocus={session => void focus(session)} />}
      {page === 'resources' && selectedId && (detail.loading ? <Loading /> : detail.error ? <><ErrorNotice message={detail.error} /><button onClick={() => setSelectedId(null)}>返回资源</button><button onClick={detail.reload}>重试</button></> : detailResource ? <Details resource={detailResource} machine={detailMachine} busy={state.busy} onBack={() => setSelectedId(null)} onConnect={() => void connect(detailResource)} onRename={() => { if (detailMachine) openDialog({ kind: 'rename', machine: detailMachine }); }} onRemove={() => { if (detailMachine) openDialog({ kind: 'remove', machine: detailMachine }); }} onAccess={() => openDialog({ kind: 'access', resourceId: detailResource.id, name: detailResource.name })} onPublish={() => { if (detailResource.machine_id) openDialog({ kind: 'publish', machineId: detailResource.machine_id, resourceKind: 'APP' }); }} onRefresh={detail.reload} /> : <Empty title="资源已不存在" />)}
      {page === 'sharing' && <><LocalBinding host={state.host} account={account} verified={canManageLocal} /><ErrorNotice message={published.error} />{published.loading && <Loading />}<Sharing host={state.host} machine={hostMachine} resources={canManageLocal ? published.data ?? [] : []} canManage={canManageLocal} busy={state.busy || published.loading} onEnroll={() => openDialog({ kind: 'enroll' })} onToggle={() => { if (state.host?.running) openDialog({ kind: 'stop' }); else void store.action(() => api.request({ op: 'set_host_enabled', enabled: true })); }} onAccess={resource => openDialog({ kind: 'access', resourceId: resource.id, name: resource.name })} onPublish={kind => { if (hostId) openDialog({ kind: 'publish', machineId: hostId, resourceKind: kind }); }} onEdit={resource => openDialog({ kind: 'edit', resource })} onPermission={permission => { void store.action(() => api.request({ op: 'open_permission_settings', permission }), false).then(ok => { if (ok && demo) setDemoPermission(true); }); }} /></>}
      {page === 'transfers' && <Transfers transfers={state.transfers} sessions={state.sessions} resources={state.resources} busy={state.busy} onSend={session => void store.action(() => api.request({ op: 'send_files', session_id: session.session_id }), false)} onFocus={session => void focus(session)} onDisconnect={session => openDialog({ kind: 'disconnect', sessionId: session.session_id, name: session.name })} />}
      {page === 'settings' && <>
        <div className="page-heading"><div><h1>账号与设置</h1><p>你的工作空间与此客户端的信息。</p></div><Settings size={25} /></div>
        <h3>当前账号</h3><dl className="panel facts">
          <div><dt>显示名称</dt><dd>{account.display_name || '未设置'}</dd></div><div><dt>邮箱</dt><dd>{account.email}</dd></div>
          <div><dt>工作空间</dt><dd>{account.workspace.name}</dd></div><div><dt>空间类型</dt><dd>{account.workspace.kind === 'PERSONAL' ? '个人空间' : '组织'}</dd></div>
          <div><dt>登录标识</dt><dd>{account.tenant}</dd></div><div><dt>Manager 地址</dt><dd>{account.manager_url}</dd></div><div><dt>角色</dt><dd>{account.role === 'OWNER' ? '空间所有者 OWNER' : account.role === 'ADMIN' ? '空间管理员 ADMIN' : account.role === 'USER' ? '普通成员 USER' : account.role || '未知'}</dd></div>
        </dl>
        <p className="footnote">下次登录请使用标识 {account.tenant}。同一邮箱在不同工作空间是独立账号。以当前账号添加的新设备归 {account.display_name || account.email} 所有，位于 {account.workspace.name}；已有设备不会因登录而变更归属。</p>
        {canAdministerOrganization(account) && <div className="panel settings-info"><h3>组织管理</h3><p>成员、用户组和邀请由当前 Manager 集中管理。使用 48 小时一次性邀请码加入组织；新成员不会自动获得设备访问权限。</p><div className="directory-actions"><button disabled={state.busy} onClick={() => openDialog({ kind: 'organization' })}>管理成员与用户组</button><button disabled={state.busy} onClick={() => openDialog({ kind: 'invitations' })}>管理组织邀请</button></div></div>}
        <h3>本机与连接</h3><LocalBinding host={state.host} account={account} verified={canManageLocal} />
        <div className="panel settings-info"><p>后台共享：{state.host ? state.host.running ? '运行中' : '已停止' : '未知'}</p><p>系统权限：{state.host ? Object.values(state.host.permissions).every(p => p === 'granted') ? '已授权' : '未全部确认，请在本机共享中查看' : '未知'}</p><p>连接在独立原生窗口中打开。音频与剪贴板设置位于对应会话窗口。</p><p>当前会话最早开始于：{dateLabel(active[0]?.started_at ?? null)}</p><button onClick={() => navigate('sharing')}>管理本机共享</button></div>
        <div className="danger-zone"><button className="danger" disabled={state.busy} onClick={() => openDialog({ kind: 'logout' })}><LogOut size={16} />退出登录</button><span>关闭远程会话，清除账号凭据；不会停止本机共享。</span></div>
      </>}
    </main></div>
    {dialog && <ActionDialog key={JSON.stringify(dialog)} dialog={dialog} account={account} api={api} busy={state.busy} error={state.error} run={(work, refresh) => store.action(work, refresh)} onClose={closeDialog} onCompleted={() => { if (dialog.kind === 'remove') setSelectedId(null); }} onLogout={() => store.logout()} />}
    {help && <Modal title="使用 NebulaDesk" onClose={() => setHelp(false)}><div className="modal-body"><h3>连接你的电脑</h3><p>在远端电脑登录同一工作空间，添加设备，并开启本机共享。被共享的资源将显示在“我的资源”。</p><h3>遇到连接问题</h3><p>检查远端电源、网络、后台服务与操作系统权限。离线或不支持的资源无法启动。错误会原样显示，不会自动切换为演示数据。</p><h3>窗口与隐私</h3><p>主窗口只负责管理。关闭独立会话窗口即可断开连接。退出登录关闭所有当前账号的会话，但共享服务需要单独停止。</p></div></Modal>}
    {demoFocus && <Modal title="独立会话窗口" onClose={() => setDemoFocus(false)}><div className="modal-body"><p>这是浏览器演示，不会连接真实设备或模拟远程视频。桌面客户端会在独立的原生窗口中打开或聚焦该会话。</p><button className="primary" onClick={() => { setDemoFocus(false); navigate('transfers'); }}>查看演示会话</button></div></Modal>}
    {demoPermission && <Modal title="系统权限设置" onClose={() => setDemoPermission(false)}><div className="modal-body"><p>演示模式不会打开操作系统设置。桌面客户端将打开所选权限的系统设置页面；请手动确认授权，然后刷新本机状态。</p></div></Modal>}
  </div>;
}
