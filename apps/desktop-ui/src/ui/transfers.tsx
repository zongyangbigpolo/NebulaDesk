import { ArrowDownToLine, ArrowUpFromLine, File } from 'lucide-react';
import type { Resource, Session, Transfer } from '../api/types';
import { Empty, ErrorNotice, sessionLabels } from './common';

const bytes = (value: number) => value < 1024 ? `${value} B` : value < 1048576 ? `${(value / 1024).toFixed(1)} KB` : `${(value / 1048576).toFixed(1)} MB`;
const labels: Record<Transfer['state'], string> = { offered: '等待接收', transferring: '传输中', complete: '已完成', failed: '失败' };
export function Transfers({ transfers, sessions, resources, busy, onSend, onFocus, onDisconnect }: {
  transfers: Transfer[]; sessions: Session[]; resources: Resource[]; busy: boolean;
  onSend: (session: Session) => void; onFocus: (session: Session) => void; onDisconnect: (session: Session) => void;
}) {
  return <>
    <div className="page-heading"><div><h1>文件传输</h1><p>通过已连接的会话，安全传送你的文件。</p></div></div>
    <h3>会话与发送</h3>
    {!sessions.length ? <div className="panel compact-empty">暂无会话。先从“我的资源”连接一台电脑。</div> : <div className="panel">{sessions.map(session => {
      const resource = resources.find(r => r.id === session.resource_id);
      const canSend = session.state === 'connected' && resource?.policy.file_transfer === true;
      const active = ['connected', 'connecting'].includes(session.state);
      return <div className="session-row" key={session.session_id}><div className="list-row"><div className="grow"><strong>{session.name}</strong><p>{sessionLabels[session.state]}{session.rtt_ms !== null ? ` · ${session.rtt_ms} ms` : ''}</p></div><button disabled={busy || !active} onClick={() => onFocus(session)}>返回会话</button><button disabled={busy || !canSend} title={!canSend ? '需要已连接且允许文件传输的会话' : undefined} onClick={() => onSend(session)}>选择文件发送</button><button className="text-button danger-text" disabled={busy || !active} onClick={() => onDisconnect(session)}>断开</button></div><ErrorNotice message={session.error} /></div>;
    })}</div>}
    <div className="section-heading"><h3>传输记录</h3><span>仅当前客户端会话</span></div>
    {!transfers.length ? <Empty title="还没有传输记录">连接支持文件传输的桌面后，使用上方按钮选择文件。取消文件选择不会发送任何内容。</Empty> : <div className="panel">{transfers.map(transfer => <div className="transfer-row" key={transfer.id}><div className="list-row"><File size={24} /><div className="grow"><strong>{transfer.name}</strong><p>{transfer.direction === 'send' ? <ArrowUpFromLine size={13} /> : <ArrowDownToLine size={13} />}{transfer.direction === 'send' ? '发送' : '接收'} · {bytes(transfer.transferred)} / {bytes(transfer.total)}</p></div><span>{labels[transfer.state]}</span></div><progress aria-label={`${transfer.name} 传输进度`} max={Math.max(transfer.total, 1)} value={transfer.transferred} /><ErrorNotice message={transfer.error} /></div>)}</div>}
  </>;
}
