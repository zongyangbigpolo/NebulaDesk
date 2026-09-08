import { ArrowLeft, ArrowUpRight, Monitor, ShieldCheck } from 'lucide-react';
import type { Machine, Resource } from '../api/types';
import { dateLabel, unavailable } from './common';

export function Details({ resource, machine, busy, onBack, onConnect, onRename, onRemove, onAccess, onPublish, onRefresh }: {
  resource: Resource; machine?: Machine; busy: boolean;
  onBack: () => void; onConnect: () => void; onRename: () => void; onRemove: () => void;
  onAccess: () => void; onPublish: () => void; onRefresh: () => void;
}) {
  const reason = unavailable(resource);
  const canManageMachine = resource.owned && !!machine;
  return <>
    <button className="text-button back" onClick={onBack}><ArrowLeft size={15} />我的资源</button>
    <div className="device-hero"><Monitor size={76} strokeWidth={1.2} /><div className="grow"><h1>{resource.name}</h1><p>{resource.owned ? '我的设备' : `${resource.owner_name ?? '未知所有者'}共享`} · {[resource.os, resource.os_version].filter(Boolean).join(' ') || '系统信息未知'}</p><p>{reason ?? '在线，可以连接'}</p></div><button disabled={busy || !!reason} onClick={onConnect}>{resource.kind === 'APP' ? '启动应用' : '连接桌面'} <ArrowUpRight size={16} /></button><small>连接将在独立窗口中打开</small></div>
    <div className="details-grid"><section><h3>设备信息</h3><dl className="panel facts"><div><dt>设备名称</dt><dd>{machine?.name ?? resource.name}</dd>{canManageMachine && <button className="text-button" disabled={busy} onClick={onRename}>重命名</button>}</div><div><dt>所有者</dt><dd>{resource.owner_name ?? '未知'}{resource.owned ? '（我）' : ''}</dd></div><div><dt>操作系统</dt><dd>{[resource.os, resource.os_version, machine?.arch].filter(Boolean).join(' · ') || '未知'}</dd></div><div><dt>最近在线</dt><dd>{dateLabel(resource.last_seen_at)}</dd></div></dl></section><section><h3>你可以使用</h3><div className="panel capabilities">{([['input', '键盘和鼠标'], ['audio', '系统声音'], ['clipboard', '文字和图片剪贴板'], ['file_transfer', '双向文件传输']] as const).map(([key, label]) => <div key={key}><ShieldCheck size={17} className={resource.policy[key] ? 'allowed' : 'muted'} /><span>{label}</span><small>{resource.policy[key] ? '已允许' : '未允许'}</small></div>)}<p className="muted">权限由设备所有者设置</p></div></section></div>
    {resource.owned && <div className="owner-actions"><button onClick={onAccess}>管理访问权限</button>{resource.machine_id && <button onClick={onPublish}>发布资源</button>}</div>}
    <div className="notice help"><div><strong>连接不上？</strong><p>确认远端电脑已开机、连接网络，并开启了 NebulaDesk 的远程连接。</p></div><button className="text-button" onClick={onRefresh} disabled={busy}>刷新设备状态</button></div>
    {canManageMachine && <div className="danger-zone"><button className="text-button danger-text" disabled={busy} onClick={onRemove}>移除设备</button><span>移除后，这台电脑需要重新加入你的账号。</span></div>}
  </>;
}
