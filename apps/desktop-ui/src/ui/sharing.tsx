import { Monitor, Plus, Shield } from 'lucide-react';
import type { LocalHost, Machine, PublishedResource } from '../api/types';
import { Empty, ErrorNotice } from './common';

export function Sharing({ host, machine, resources, canManage, busy, onEnroll, onToggle, onAccess, onPublish, onEdit, onPermission }: {
  host: LocalHost | null; machine?: Machine; resources: PublishedResource[]; canManage: boolean; busy: boolean;
  onEnroll: () => void; onToggle: () => void; onAccess: (resource: PublishedResource) => void;
  onPublish: (kind: 'DESKTOP' | 'APP') => void; onEdit: (resource: PublishedResource) => void;
  onPermission: (permission: 'screen' | 'input' | 'audio') => void;
}) {
  const desktops = resources.filter(r => r.kind === 'DESKTOP');
  const apps = resources.filter(r => r.kind === 'APP');
  return <>
    <div className="page-heading"><div><h1>本机共享</h1><p>决定谁能连接这台电脑，以及他们能使用什么。</p></div></div>
    {!host ? <Empty title="无法读取本机状态">请刷新重试。系统权限和后台服务状态尚未确认。</Empty> : !host.enrolled ? <Empty title="这台电脑尚未加入工作空间"><span>使用你的账号，或管理员提供的注册令牌添加本机。</span><button className="primary" onClick={onEnroll}><Plus size={16} />添加这台电脑</button></Empty> : <>
      <div className="panel host-status"><div className="host-icon"><Monitor size={38} strokeWidth={1.4} /></div><div className="grow"><h2>{host.name ?? '本机'}</h2><p>{[machine?.os, machine?.os_version, machine?.arch].filter(Boolean).join(' · ') || '系统信息未知'}</p><span><span className={`dot ${host.running ? 'green' : ''}`} />{host.running ? '后台服务运行中' : '后台服务已停止'}</span></div><div className="host-toggle"><label>允许远程连接<button type="button" role="switch" aria-checked={host.running} aria-label="允许远程连接" disabled={busy || !canManage} onClick={onToggle} className={`switch ${host.running ? 'on' : ''}`}><span /></button></label><small>关闭主窗口后仍可连接</small></div></div>
      <ErrorNotice message={host.error} />
      {!canManage && <div className="notice">无法确认本机属于当前账号。共享管理不可用，请核对账号和设备所有权。</div>}
      <div className="sharing-tabs"><span>共享内容</span><span className="muted">权限由工作空间统一管理</span></div>
      <div className="section-heading"><div><h2>完整桌面</h2><p>对方可以看到桌面，并使用已允许的功能。</p></div>{!desktops.length && canManage && <button onClick={() => onPublish('DESKTOP')}>发布桌面</button>}</div>
      {!desktops.length ? <div className="panel compact-empty">尚未发布完整桌面。</div> : <div className="panel">{desktops.map(resource => <div className="list-row" key={resource.id}><div className="avatar"><Monitor size={19} /></div><div className="grow"><strong>{resource.name}</strong><p>访问范围以实际授权为准</p></div><span className="tag">{resource.enabled ? '已发布' : '已停用'}</span><button disabled={busy || !canManage} onClick={() => onAccess(resource)}>管理访问权限</button><button className="text-button" disabled={busy || !canManage} onClick={() => onEdit(resource)}>修改</button></div>)}</div>}
      <div className="section-heading"><div><h2>共享应用</h2><p>管理应用发布信息。当前版本暂不支持独立应用串流。</p></div><button className="primary" disabled={!canManage} onClick={() => onPublish('APP')}><Plus size={15} />添加应用</button></div>
      {!apps.length ? <div className="panel compact-empty">还没有发布应用。发布信息不会开启整个桌面连接。</div> : <div className="panel">{apps.map(resource => <div className="list-row" key={resource.id}><div className="mini-app">{resource.name.slice(0, 1)}</div><div className="grow"><strong>{resource.name}</strong><p>独立应用连接暂不可用</p></div><span className="muted">{resource.enabled ? '已发布' : '未发布'}</span><button className="text-button" disabled={busy || !canManage} onClick={() => onAccess(resource)}>访问权限</button><button className="text-button" disabled={busy || !canManage} onClick={() => onEdit(resource)}>管理</button></div>)}</div>}
      <div className="notice permissions"><Shield size={20} /><strong>系统权限</strong>{([['screen', '屏幕录制'], ['audio', '系统音频'], ['input', '辅助功能']] as const).map(([key, label]) => <span key={key} className={host.permissions[key] === 'granted' ? 'allowed' : ''}>{label}：{host.permissions[key] === 'granted' ? '已授权' : host.permissions[key] === 'denied' ? '未授权' : '未知'}<button className="text-button" disabled={busy} aria-label={`打开${label}系统设置`} onClick={() => onPermission(key)}>系统设置</button></span>)}</div><p className="footnote">权限未知不代表已授权。请在本机系统设置中检查屏幕录制与辅助功能权限。</p>
    </>}
  </>;
}
