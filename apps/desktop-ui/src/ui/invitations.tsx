import { useCallback, useEffect, useState, type FormEvent } from 'react';
import type { Account, DesktopApi, Invitation } from '../api/types';
import { useQuery } from '../state/useQuery';
import { dateLabel, Empty, ErrorNotice, Loading, Modal } from './common';

export function invitationStatus(item: Invitation, now: number) {
  if (item.accepted_at) return '已使用';
  if (item.revoked_at) return '已撤销';
  const expiry = Date.parse(item.expires_at);
  if (!Number.isFinite(expiry)) return '有效期未知';
  return expiry <= now ? '已过期' : '待接受';
}

export function Invitations({ account, api, busy, error, run, onClose }: {
  account: Account; api: DesktopApi; busy: boolean; error: string | null;
  run: (work: () => Promise<unknown>, refresh?: boolean) => Promise<boolean>; onClose: () => void;
}) {
  const load = useCallback(() => api.request({ op: 'invitations' }), [api]);
  const query = useQuery(load);
  const [email, setEmail] = useState('');
  const [issued, setIssued] = useState<(Invitation & { token: string }) | null>(null);
  const [revoke, setRevoke] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  async function create(event: FormEvent) {
    event.preventDefault();
    if (issued) return;
    const ok = await run(async () => {
      const result = await api.request({ op: 'create_invitation', email: email.trim() });
      setIssued(result);
    }, false);
    if (ok) { setEmail(''); query.reload(); }
  }
  return <Modal title="组织成员邀请" busy={busy} onClose={onClose}>
    <div className="modal-body">
      <p><strong>{account.workspace.name}</strong> · {account.tenant}</p>
      <p className="muted">邀请固定加入当前组织，角色为普通成员 USER。不会授予组织管理员权限，也不会自动授予设备访问权限。</p>
      <ErrorNotice message={[error, query.error].filter(Boolean).join('；') || null} />
      {issued ? <section className="invitation-code" aria-label="新建邀请码">
        <h3>邀请码仅显示这一次</h3>
        <p>发送给 {issued.email}。请通过可信渠道手动发送，不会自动复制到剪贴板。</p>
        <label>一次性邀请码<input readOnly value={issued.token} autoCapitalize="none" autoCorrect="off" spellCheck={false} autoComplete="off" onFocus={e => e.currentTarget.select()} /></label>
        <p className="footnote">到期：{dateLabel(issued.expires_at)} · 48 小时内仅可接受一次。对方还需要服务器地址 {account.manager_url}。</p>
        <button disabled={busy} onClick={() => setIssued(null)}>我已保存，隐藏邀请码</button>
      </section> : <form onSubmit={create} className="grant-form" autoCapitalize="none" autoCorrect="off" spellCheck={false}>
        <h3>邀请新成员</h3>
        <label>受邀邮箱<input autoCapitalize="none" autoCorrect="off" spellCheck={false} type="email" required maxLength={254} autoComplete="off" value={email} onChange={e => setEmail(e.target.value)} /></label>
        <button className="primary" type="submit" disabled={busy || query.loading}>{busy ? '正在处理…' : '创建邀请码'}</button>
      </form>}
      <div className="section-heading"><h3>邀请记录</h3><button disabled={busy || query.loading} onClick={query.reload}>刷新邀请</button></div>
      {query.loading ? <Loading /> : query.error ? <button disabled={busy} onClick={query.reload}>重新加载邀请</button> : !query.data?.length ? <Empty title="暂无邀请">创建邀请码后，只在此保留状态记录，不保留可再次查看的令牌。</Empty> :
        <div className="grant-list">{query.data.map(item => {
          const status = invitationStatus(item, now);
          const active = status === '待接受';
          return <div className={`grant-row ${active ? '' : 'grant-inactive'}`} key={item.id}>
            <div className="grow"><strong>{item.email}</strong><p>{status} · 普通成员 USER</p><p>创建：{dateLabel(item.created_at)}</p><p>到期：{dateLabel(item.expires_at)}</p></div>
            {!active ? <button disabled>不可撤销 · {status}</button> : revoke === item.id ? <>
              <button className="danger" disabled={busy} onClick={async () => {
                if (await run(() => api.request({ op: 'revoke_invitation', id: item.id }), false)) {
                  if (issued?.id === item.id) setIssued(null);
                  setRevoke(null); query.reload();
                }
              }}>确认撤销邀请</button><button disabled={busy} onClick={() => setRevoke(null)}>取消</button>
            </> : <button disabled={busy} onClick={() => setRevoke(item.id)}>撤销邀请</button>}
          </div>;
        })}</div>}
    </div>
  </Modal>;
}
