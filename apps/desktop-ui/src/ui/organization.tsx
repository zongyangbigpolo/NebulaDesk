import { useCallback, useState, type FormEvent } from 'react';
import type { Account, DesktopApi, DirectoryUser, Group } from '../api/types';
import { useQuery } from '../state/useQuery';
import { Empty, ErrorNotice, Loading, Modal } from './common';

type Props = {
  account: Account; api: DesktopApi; busy: boolean; error: string | null;
  run: (work: () => Promise<unknown>, refresh?: boolean) => Promise<boolean>; onClose: () => void;
};
const exact = { autoCapitalize: 'none', autoCorrect: 'off', spellCheck: false } as const;

export function OrganizationAdmin(props: Props) {
  const [tab, setTab] = useState<'users' | 'groups'>('users');
  return <Modal title="组织成员与用户组" onClose={props.onClose} busy={props.busy}>
    <div className="modal-body">
      <p><strong>{props.account.workspace.name}</strong> · {props.account.tenant}</p>
      <p className="muted">组织目录保存在当前 Manager。用户组用于集中组织成员；成员加入后不会自动获得全部设备权限。</p>
      <div className="directory-tabs" role="group" aria-label="组织管理视图">
        <button disabled={props.busy} aria-pressed={tab === 'users'} onClick={() => setTab('users')}>成员</button>
        <button disabled={props.busy} aria-pressed={tab === 'groups'} onClick={() => setTab('groups')}>用户组</button>
      </div>
      {tab === 'users' ? <Members {...props} /> : <Groups {...props} />}
    </div>
  </Modal>;
}

function UserLabel({ user }: { user: DirectoryUser }) {
  return <div className="grow"><strong>{user.display_name || user.email}</strong><p>{user.email}</p><p>{user.role} · {user.disabled ? '已停用' : '已启用'}</p></div>;
}

function Members({ account, api, busy, error, run }: Props) {
  const load = useCallback(() => api.request({ op: 'users' }), [api]);
  const query = useQuery(load);
  const [confirm, setConfirm] = useState<string | null>(null);
  return <section aria-label="组织成员">
    <div className="section-heading"><h3>组织成员</h3><button disabled={busy || query.loading} onClick={query.reload}>刷新成员</button></div>
    <ErrorNotice message={[error, query.error].filter(Boolean).join('；') || null} />
    <p className="footnote">新成员请使用组织邀请加入。停用会阻止后续登录和连接，并撤销刷新凭据；已建立的设备连接不保证立即中断。</p>
    {query.loading ? <Loading /> : query.error ? <button disabled={busy} onClick={query.reload}>重新加载成员</button> : !query.data?.length ? <Empty title="暂无成员" /> :
      query.data.map(user => <div className="grant-row directory-row" key={user.id}>
        <UserLabel user={user} />
        {user.id === account.id ? <span className="tag">当前账号</span> : user.role === 'OWNER' ? <span className="muted">所有者账号</span> : confirm === user.id ?
          <div className="directory-actions"><button disabled={busy} className={user.disabled ? 'primary' : 'danger'} onClick={async () => {
            if (await run(() => api.request({ op: 'set_user_disabled', user_id: user.id, disabled: !user.disabled }), false)) {
              setConfirm(null); query.reload();
            }
          }}>{user.disabled ? '确认启用成员' : '确认停用成员'}</button><button disabled={busy} onClick={() => setConfirm(null)}>取消</button></div> :
          <button disabled={busy} onClick={() => setConfirm(user.id)}>{user.disabled ? '启用成员' : '停用成员'}</button>}
      </div>)}
  </section>;
}

function Groups(props: Props) {
  const { api, busy, error, run } = props;
  const load = useCallback(() => api.request({ op: 'groups' }), [api]);
  const query = useQuery(load);
  const [name, setName] = useState('');
  const [selected, setSelected] = useState<Group | null>(null);
  const [confirm, setConfirm] = useState<string | null>(null);
  const nameValid = !!name.trim() && new TextEncoder().encode(name.trim()).length <= 200 && !/[\u0000-\u001f\u007f-\u009f]/.test(name);
  async function create(event: FormEvent) {
    event.preventDefault();
    if (!nameValid) return;
    if (await run(() => api.request({ op: 'create_group', name: name.trim() }), false)) {
      setName(''); query.reload();
    }
  }
  if (selected) return <GroupMembers key={selected.id} {...props} group={selected} onBack={() => { setSelected(null); query.reload(); }} />;
  return <section aria-label="组织用户组">
    <div className="section-heading"><h3>用户组</h3><button disabled={busy || query.loading} onClick={query.reload}>刷新用户组</button></div>
    <ErrorNotice message={[error, query.error].filter(Boolean).join('；') || null} />
    {query.loading ? <Loading /> : query.error ? <button disabled={busy} onClick={query.reload}>重新加载用户组</button> : !query.data?.length ? <Empty title="暂无用户组">创建用户组，再添加此组织中的已启用成员。</Empty> :
      query.data.map(group => <div className="grant-row directory-row" key={group.id}>
        <div className="grow"><strong>{group.name}</strong></div>
        {confirm === group.id ? <div className="directory-confirm"><p>删除“{group.name}”及其成员关系？该组的资源授权也会失效，成员账号不会删除。</p><div className="directory-actions">
          <button disabled={busy} className="danger" onClick={async () => {
            if (await run(() => api.request({ op: 'delete_group', group_id: group.id }), false)) { setConfirm(null); query.reload(); }
          }}>确认删除用户组</button><button disabled={busy} onClick={() => setConfirm(null)}>取消</button>
        </div></div> : <div className="directory-actions"><button disabled={busy} onClick={() => setSelected(group)}>管理成员</button><button disabled={busy} onClick={() => setConfirm(group.id)}>删除用户组</button></div>}
      </div>)}
    <form className="grant-form" onSubmit={create} {...exact}>
      <h3>创建用户组</h3><label>用户组名称<input {...exact} required maxLength={200} value={name} onChange={e => setName(e.target.value)} aria-describedby="group-name-help" /></label>
      <p className="footnote" id="group-name-help">名称在组织内唯一，最多 200 字节（中文字符通常占 3 字节）。不允许控制字符。</p>
      <button className="primary" disabled={busy || query.loading || !nameValid}>创建用户组</button>
    </form>
  </section>;
}

function GroupMembers({ api, busy, error, run, group, onBack }: Props & { group: Group; onBack: () => void }) {
  const load = useCallback(async () => {
    const [members, users] = await Promise.all([api.request({ op: 'group_members', group_id: group.id }), api.request({ op: 'users' })]);
    return { members, users };
  }, [api, group.id]);
  const query = useQuery(load);
  const [userId, setUserId] = useState('');
  const [confirm, setConfirm] = useState<string | null>(null);
  const available = (query.data?.users ?? []).filter(user => !user.disabled && !query.data?.members.some(member => member.id === user.id));
  async function add(event: FormEvent) {
    event.preventDefault();
    if (!available.some(user => user.id === userId)) return;
    if (await run(() => api.request({ op: 'add_group_member', group_id: group.id, user_id: userId }), false)) {
      setUserId(''); query.reload();
    }
  }
  return <section aria-label="用户组成员">
    <button disabled={busy} onClick={onBack}>返回用户组</button>
    <div className="section-heading"><h3>{group.name}</h3><button disabled={busy || query.loading} onClick={query.reload}>刷新组成员</button></div>
    <ErrorNotice message={[error, query.error].filter(Boolean).join('；') || null} />
    <p className="footnote">组内成员可能通过用户组获得资源访问权限。移出后仍可能保留直接授权；已建立的连接不保证立即中断。</p>
    {query.loading ? <Loading /> : query.error ? <button disabled={busy} onClick={query.reload}>重新加载组成员</button> : <>
      {!query.data?.members.length ? <Empty title="组内暂无成员" /> : query.data.members.map(user => <div className="grant-row directory-row" key={user.id}>
        <UserLabel user={user} />
        {confirm === user.id ? <div className="directory-actions"><button className="danger" disabled={busy} onClick={async () => {
          if (await run(() => api.request({ op: 'remove_group_member', group_id: group.id, user_id: user.id }), false)) {
            setConfirm(null); query.reload();
          }
        }}>确认移出用户组</button><button disabled={busy} onClick={() => setConfirm(null)}>取消</button></div> :
          <button disabled={busy} onClick={() => setConfirm(user.id)}>移出用户组</button>}
      </div>)}
      <form onSubmit={add} className="grant-form">
        <h3>添加组成员</h3><label>组织成员<select value={userId} onChange={e => setUserId(e.target.value)} required disabled={busy || !available.length}>
          <option value="">选择已启用的组织成员</option>{available.map(user => <option key={user.id} value={user.id}>{user.display_name || user.email} · {user.email}</option>)}
        </select></label>
        {!available.length && <p className="footnote">没有可添加的已启用成员。请先通过组织邀请添加账号，或在成员目录启用账号。</p>}
        <button className="primary" disabled={busy || !available.some(user => user.id === userId)}>添加到用户组</button>
      </form>
    </>}
  </section>;
}
