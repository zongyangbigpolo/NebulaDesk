import { useEffect, useRef, type ReactNode } from 'react';
import { AlertCircle, Monitor, AppWindow, X, LoaderCircle } from 'lucide-react';
import type { Resource, Session } from '../api/types';

export const sessionLabels: Record<Session['state'], string> = { connecting: '正在连接', connected: '已连接', disconnected: '已断开', failed: '连接失败' };
export function online(resource: Resource) { return resource.machine_status.toUpperCase() === 'ONLINE'; }
export function machineStatusLabel(resource: Resource) {
  return online(resource) ? '在线' : resource.machine_status.toUpperCase() === 'OFFLINE' ? '离线' : '状态未知';
}
export function shortcutLabel(platform: string) { return /mac/i.test(platform) ? '⌘ K' : 'Ctrl K'; }
export function unavailable(resource: Resource) {
  if (!resource.launch_supported) return resource.kind === 'APP' ? '暂不支持独立应用连接' : '此资源暂不支持连接';
  if (!online(resource)) return resource.machine_status.toUpperCase() === 'OFFLINE' ? '设备离线' : '设备状态未知';
  return null;
}
export function dateLabel(value: string | null) {
  if (!value) return '未知';
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? '未知' : date.toLocaleString('zh-CN', { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
}
export function ErrorNotice({ message }: { message: string | null }) {
  return message ? <div role="alert" className="notice error"><AlertCircle size={17} /><span>{message}</span></div> : null;
}
export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return <div className="empty"><Monitor size={34} /><h3>{title}</h3><p>{children}</p></div>;
}
export function Loading() { return <div className="loading" role="status"><LoaderCircle size={18} className="spin" />正在加载…</div>; }
export function ResourceArt({ resource }: { resource: Pick<Resource, 'kind' | 'os' | 'name'> }) {
  return <div className={`resource-art ${resource.kind === 'APP' ? 'app-art' : resource.os?.toLowerCase().includes('windows') ? 'studio-art' : ''}`} aria-hidden="true">
    {resource.kind === 'APP' ? <div className="app-tile">{resource.name === 'Photoshop' ? 'Ps' : resource.name.slice(0, 1) || <AppWindow />}</div> : <Monitor size={68} strokeWidth={1.2} />}
  </div>;
}
export function Modal({ title, children, onClose, busy = false }: { title: string; children: ReactNode; onClose: () => void; busy?: boolean }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const previous = document.activeElement;
    ref.current?.showModal();
    return () => { if (previous instanceof HTMLElement) previous.focus(); };
  }, []);
  return <dialog ref={ref} aria-label={title} onCancel={event => { event.preventDefault(); if (!busy) onClose(); }}>
    <header className="modal-header"><h2>{title}</h2><button type="button" className="icon-button" aria-label="关闭对话框" disabled={busy} onClick={onClose}><X size={20} /></button></header>
    {children}
  </dialog>;
}
