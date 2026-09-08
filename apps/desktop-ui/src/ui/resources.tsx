import { ArrowUpRight, Plus } from 'lucide-react';
import type { Resource, Session } from '../api/types';
import { dateLabel, Empty, machineStatusLabel, online, ResourceArt, sessionLabels, unavailable } from './common';

export type ResourceFilter = 'ALL' | 'DESKTOP' | 'APP';
export function Resources({ resources, sessions, query, filter, sort, busy, onFilter, onSort, onAdd, onDetails, onConnect, onFocus }: {
  resources: Resource[]; sessions: Session[]; query: string; filter: ResourceFilter; sort: string; busy: boolean;
  onFilter: (value: ResourceFilter) => void; onSort: (value: string) => void;
  onAdd: () => void; onDetails: (resource: Resource) => void; onConnect: (resource: Resource) => void; onFocus: (session: Session) => void;
}) {
  const recent = (r: Resource) => Math.max(0, ...sessions.filter(s => s.resource_id === r.id).map(s => Date.parse(s.started_at) || 0));
  const visible = resources.filter(r => (filter === 'ALL' || r.kind === filter) && `${r.name} ${r.description} ${r.owner_name ?? ''}`.toLowerCase().includes(query.trim().toLowerCase()))
    .sort((a, b) => sort === 'name' ? a.name.localeCompare(b.name, 'zh-CN') : recent(b) - recent(a));
  const active = sessions.filter(s => ['connected', 'connecting'].includes(s.state));
  const cards = visible.slice(0, filter === 'ALL' ? 3 : visible.length);
  const rest = filter === 'ALL' ? visible.slice(3) : [];
  return <>
    <div className="page-heading"><div><h1>我的资源</h1><p>你的电脑，以及共享给你的应用。</p></div><button className="primary" onClick={onAdd}><Plus size={17} />添加设备</button></div>
    <div className="filter-row"><div className="segments" aria-label="资源类型">{([['ALL', '全部'], ['DESKTOP', '桌面'], ['APP', '应用']] as const).map(([value, label]) => <button key={value} aria-pressed={filter === value} onClick={() => onFilter(value)}>{label}</button>)}</div><select aria-label="资源排序" value={sort} onChange={e => onSort(e.target.value)}><option value="recent">最近使用</option><option value="name">按名称</option></select></div>
    {active.slice(0, 2).map(session => <div className="active-session" key={session.session_id}><div className="small-monitor"><ArrowUpRight size={23} /></div><div className="grow"><strong>{session.name}</strong><p><span className={`dot ${session.state === 'connected' ? 'green' : ''}`} />{sessionLabels[session.state]} · {session.path === 'direct' ? '直连' : session.path === 'relay' ? '中继连接' : '正在建立安全连接'}</p></div><button disabled={busy} onClick={() => onFocus(session)}>返回会话 <ArrowUpRight size={14} /></button></div>)}
    <div className="section-heading"><h3>{sort === 'recent' ? '最近使用' : '所有资源'}</h3><span>{resources.filter(r => r.kind === 'DESKTOP').length} 台电脑 · {resources.filter(r => r.kind === 'APP').length} 个应用</span></div>
    {!visible.length ? <Empty title={resources.length ? '没有匹配的资源' : '还没有可连接的资源'}>{resources.length ? '试试其他关键词或资源类型。' : '添加自己的设备，或请所有者通过你的账号邮箱授予访问权限。'}</Empty> : <div className="resource-grid">{cards.map(resource => {
      const session = active.find(s => s.resource_id === resource.id);
      const reason = unavailable(resource);
      return <article className="resource-card" key={resource.id}>
        <button className="art-button" aria-label={`查看 ${resource.name} 详情`} onClick={() => onDetails(resource)}><ResourceArt resource={resource} /><span className="art-badge"><span className={`dot ${(session ? session.state === 'connected' : online(resource)) ? 'green' : ''}`} />{session ? sessionLabels[session.state] : machineStatusLabel(resource)}</span></button>
        <div className="card-body"><button className="name-button" onClick={() => onDetails(resource)}>{resource.name}</button><p>{resource.owned ? '我的设备' : `${resource.owner_name ?? '未知所有者'}共享`} · {resource.os ?? (resource.description || '应用')}</p><footer><span>{reason ?? (recent(resource) ? `上次使用：${dateLabel(new Date(recent(resource)).toISOString())}` : '尚未使用')}</span><button className="text-button" disabled={busy || !!reason} title={reason ?? undefined} onClick={() => onConnect(resource)}>{resource.kind === 'APP' ? '启动' : '打开'} <ArrowUpRight size={13} /></button></footer></div>
      </article>;
    })}</div>}
    {!!rest.length && <><div className="section-heading"><h3>更多资源</h3></div><div className="panel resource-list">{rest.map(resource => <div className="list-row" key={resource.id}><div className="mini-app">{resource.name.slice(0, 1)}</div><button className="name-button grow" onClick={() => onDetails(resource)}>{resource.name}</button><span className="muted description">{resource.description}</span><span className="muted">{unavailable(resource) ?? '可连接'}</span><button className="text-button" disabled={busy || !!unavailable(resource)} onClick={() => onConnect(resource)}>启动 <ArrowUpRight size={14} /></button></div>)}</div></>}
    <p className="footnote">连接会在独立的原生窗口中打开。关闭会话窗口即可断开，不影响其他连接。</p>
  </>;
}
