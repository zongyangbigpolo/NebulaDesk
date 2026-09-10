import { useCallback, useEffect, useState, type FormEvent, type ReactNode } from 'react';
import type { Account, DesktopApi, Machine, PublishedResource } from '../api/types';
import { canAdministerOrganization } from '../api/permissions';
import { useQuery } from '../state/useQuery';
import { dateLabel, Empty, ErrorNotice, grantStatus, Loading, Modal } from './common';
import { Invitations } from './invitations';
import { OrganizationAdmin } from './organization';

export type DialogState =
  | { kind: 'enroll' }
  | { kind: 'invitations' }
  | { kind: 'organization' }
  | { kind: 'rename'; machine: Machine }
  | { kind: 'remove'; machine: Machine }
  | { kind: 'stop' }
  | { kind: 'disconnect'; sessionId: string; name: string }
  | { kind: 'logout' }
  | { kind: 'access'; resourceId: string; name: string }
  | { kind: 'publish'; machineId: string; resourceKind: 'DESKTOP' | 'APP' }
  | { kind: 'edit'; resource: PublishedResource };
type Run = (work: () => Promise<unknown>, refresh?: boolean) => Promise<boolean>;
type Props = { dialog: DialogState; account: Account; api: DesktopApi; busy: boolean; error: string | null; run: Run; onClose: () => void; onCompleted?: () => void; onLogout: () => Promise<boolean> };
const admissionNotice = '授权变更会阻止后续连接；已建立的连接不会立即断开。需要立即断开时，请停止被控端共享。';

function Actions({ busy, blocked = false, onClose, label = '保存', danger = false }: { busy: boolean; blocked?: boolean; onClose: () => void; label?: string; danger?: boolean }) {
  return <footer className="modal-actions"><button type="button" disabled={busy} onClick={onClose}>取消</button><button type="submit" className={danger ? 'danger' : 'primary'} disabled={busy || blocked}>{busy ? '正在处理…' : label}</button></footer>;
}
export function HttpOptIn({ url, value, onChange }: { url: string; value: boolean; onChange: (value: boolean) => void }) {
  return url.trim().toLowerCase().startsWith('http:') ? <label className="checkbox warning"><input type="checkbox" checked={value} onChange={e => onChange(e.target.checked)} />仅开发环境：允许向回环或私有 IP 通过明文 HTTP 发送凭据。请勿在不可信网络使用。</label> : null;
}
export function ActionDialog(props: Props) {
  const { dialog, account, api, busy, error, run, onClose, onLogout } = props;
  const [name, setName] = useState(dialog.kind === 'rename' ? dialog.machine.name : dialog.kind === 'edit' ? dialog.resource.name : '');
  const [description, setDescription] = useState(dialog.kind === 'edit' ? dialog.resource.description : '');
  const [enabled, setEnabled] = useState(dialog.kind === 'edit' ? dialog.resource.enabled : true);
  const [path, setPath] = useState('');
  const [args, setArgs] = useState('');
  const [method, setMethod] = useState('account');
  const [managerUrl, setManagerUrl] = useState(account.manager_url);
  const [token, setToken] = useState('');
  const [http, setHttp] = useState(false);
  if (dialog.kind === 'access') return <AccessDialog {...props} resourceId={dialog.resourceId} name={dialog.name} />;
  if (dialog.kind === 'invitations') return canAdministerOrganization(account) ? <Invitations {...props} /> : null;
  if (dialog.kind === 'organization') return canAdministerOrganization(account) ? <OrganizationAdmin {...props} /> : null;
  const titles = { enroll: '添加这台电脑', rename: '重命名设备', remove: '移除设备', stop: '停止本机共享', disconnect: '断开会话', logout: '退出登录', publish: '发布资源', edit: '编辑发布信息' };
  let body: ReactNode;
  let label = '保存';
  let danger = false;
  switch (dialog.kind) {
    case 'enroll':
      label = '添加设备';
      body = <><p className="muted">将运行此客户端的电脑加入工作空间。不会添加其他远端设备。</p>
        <label>设备名称<input autoFocus required value={name} onChange={e => setName(e.target.value)} maxLength={120} placeholder="例如：我的 MacBook" /></label>
        <label>注册方式<select value={method} onChange={e => { setMethod(e.target.value); setToken(''); setHttp(false); }}><option value="account">使用当前账号注册</option><option value="token">使用管理员提供的设备令牌</option></select></label>
        {method === 'account' ? <div className="notice" aria-label="设备归属确认"><strong>确认设备归属</strong><p>目标工作空间：{account.workspace.name}（{account.workspace.kind === 'PERSONAL' ? '个人空间' : '组织'}）</p><p>工作空间标识：{account.tenant}</p><p>设备所有者：{account.display_name} · {account.email}</p><p>服务器：{account.manager_url}</p><p>归属固定为当前账号，不能指定其他所有者。已有本机绑定不会自动转移。</p></div> : <>
          <p className="notice">这是管理员设备注册模式，不是账号注册或组织成员邀请。目标工作空间与所有者由设备令牌决定，无法在此确认；请向管理员核实，不能据此认定属于当前账号。</p>
          <label>工作空间地址<input autoCapitalize="none" autoCorrect="off" spellCheck={false} type="url" required value={managerUrl} onChange={e => { setManagerUrl(e.target.value); setHttp(false); }} /></label><label>注册令牌<input autoCapitalize="none" autoCorrect="off" spellCheck={false} type="password" required value={token} autoComplete="off" onChange={e => setToken(e.target.value)} /></label>
        </>}
        <HttpOptIn url={method === 'account' ? account.manager_url : managerUrl} value={http} onChange={setHttp} /><p className="muted">注册令牌仅用于本次操作，不保存在浏览器中。注册完成后，请在本机共享中开启远程连接。</p></>;
      break;
    case 'rename': body = <label>设备名称<input autoFocus required value={name} onChange={e => setName(e.target.value)} maxLength={120} /></label>; break;
    case 'publish': case 'edit':
      label = dialog.kind === 'publish' ? '发布' : '保存';
      body = <>{dialog.kind === 'publish' && <div className="notice">{dialog.resourceKind === 'APP' ? '获授权用户可直接启动应用，无需选择电脑。仅支持应用窗口的设备可连接，不会改为共享整个桌面。应用使用设备当前登录账号，并非独立沙箱，请仅授权给可信用户。' : '发布完整桌面后，可通过访问权限添加指定用户。'}</div>}<label>资源名称<input autoFocus required value={name} onChange={e => setName(e.target.value)} maxLength={120} /></label><label>描述<textarea value={description} onChange={e => setDescription(e.target.value)} maxLength={1000} rows={3} /></label>{dialog.kind === 'publish' && dialog.resourceKind === 'APP' && <><label>应用启动路径<input required autoCapitalize="none" autoCorrect="off" spellCheck={false} value={path} onChange={e => setPath(e.target.value)} placeholder="设备上应用的完整路径" /></label><label>启动参数（每行一个，可选）<textarea autoCapitalize="none" autoCorrect="off" spellCheck={false} value={args} onChange={e => setArgs(e.target.value)} rows={3} /></label></>}{dialog.kind === 'edit' && <label className="checkbox"><input type="checkbox" checked={enabled} onChange={e => setEnabled(e.target.checked)} />允许发布此资源</label>}</>;
      break;
    case 'remove': danger = true; label = '确认移除'; body = <><p>移除“{dialog.machine.name}”将撤销该设备的共享资源。之后需要重新注册才能加入工作空间。此操作无法撤销。</p><p className="notice">{admissionNotice}</p></>; break;
    case 'stop': danger = true; label = '停止共享'; body = <p>停止本机后台服务后，其他设备无法再连接这台电脑，现有的入站连接也会中断。你的其他远程会话不受影响。</p>; break;
    case 'disconnect': danger = true; label = '断开'; body = <p>断开“{dialog.name}”的连接？这不会请求退出远端应用，也不会保存其中的未保存内容。未完成的文件传输可能被中断。</p>; break;
    case 'logout': danger = true; label = '退出登录'; body = <p>退出将关闭当前账号的所有远程会话并清除登录凭据。本机共享服务不会随之停止；如需停止，请先前往“本机共享”。</p>; break;
  }
  async function submit(event: FormEvent) {
    event.preventDefault();
    let ok = false;
    switch (dialog.kind) {
      case 'enroll':
        if ((method === 'account' ? account.manager_url : managerUrl).trim().toLowerCase().startsWith('http:') && !http) return;
        ok = await run(async () => {
          const enrollmentToken = method === 'account' ? (await api.request({ op: 'create_enrollment', name: name.trim() })).token : token.trim();
          await api.request({ op: 'enroll_local', manager_url: method === 'account' ? account.manager_url : managerUrl.trim(), name: name.trim(), token: enrollmentToken, allow_insecure_http: http });
        }); setToken(''); break;
      case 'rename': ok = await run(() => api.request({ op: 'rename_machine', machine_id: dialog.machine.id, name: name.trim() })); break;
      case 'remove': ok = await run(() => api.request({ op: 'remove_machine', machine_id: dialog.machine.id })); break;
      case 'stop': ok = await run(() => api.request({ op: 'set_host_enabled', enabled: false })); break;
      case 'disconnect': ok = await run(() => api.request({ op: 'disconnect_session', session_id: dialog.sessionId }), false); break;
      case 'logout': ok = await onLogout(); break;
      case 'publish': ok = await run(() => api.request({ op: 'publish_resource', machine_id: dialog.machineId, resource: { kind: dialog.resourceKind, name: name.trim(), description: description.trim(), ...(dialog.resourceKind === 'APP' ? { launch_path: path.trim(), launch_args: args.split('\n').filter(Boolean) } : {}) } })); break;
      case 'edit': ok = await run(() => api.request({ op: 'update_resource', resource_id: dialog.resource.id, changes: { name: name.trim(), description: description.trim(), enabled } })); break;
    }
    if (ok) { setToken(''); props.onCompleted?.(); onClose(); }
  }
  const httpBlocked = dialog.kind === 'enroll' && (method === 'account' ? account.manager_url : managerUrl).trim().toLowerCase().startsWith('http:') && !http;
  return <Modal title={titles[dialog.kind]} onClose={onClose} busy={busy}><form onSubmit={submit}><div className="modal-body"><ErrorNotice message={error} />{body}{dialog.kind === 'edit' && !enabled && <p className="notice">{admissionNotice}</p>}</div><Actions busy={busy} blocked={httpBlocked} onClose={onClose} label={label} danger={danger} /></form></Modal>;
}

function AccessDialog({ name, resourceId, api, busy, error, run, onClose }: Props & { name: string; resourceId: string }) {
  const load = useCallback(() => api.request({ op: 'grants', resource_id: resourceId }), [api, resourceId]);
  const query = useQuery(load);
  const [email, setEmail] = useState('');
  const [role, setRole] = useState('VIEWER');
  const [clipboard, setClipboard] = useState(false);
  const [files, setFiles] = useState(false);
  const [audio, setAudio] = useState(false);
  const [revoke, setRevoke] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  const viewer = role === 'VIEWER';
  async function grant(event: FormEvent) {
    event.preventDefault();
    const ok = await run(() => api.request({ op: 'grant_access', resource_id: resourceId, email: email.trim(), role, allow_clipboard: !viewer && clipboard, allow_file_transfer: !viewer && files, allow_audio: !viewer && audio }), false);
    if (ok) { setEmail(''); query.reload(); }
  }
  return <Modal title="管理访问权限" onClose={onClose} busy={busy}><div className="modal-body"><p className="muted">{name} · 仅向同一工作空间中的指定用户授权</p><ErrorNotice message={error ?? query.error} />{query.loading ? <Loading /> : query.error ? <button onClick={query.reload}>重新加载授权</button> : !query.data?.length ? <Empty title="暂无额外授权">没有额外授权记录不代表资源所有者没有权限。</Empty> : <div className="grant-list">{query.data.map(item => {
    const status = grantStatus(item, now);
    const current = status === 'active';
    return <div className={`grant-row ${current ? '' : 'grant-inactive'}`} key={item.id}>
      <div className="grow">
        <strong>{item.user_display_name || item.user_email || item.group_name || (item.user_id ? `用户 ${item.user_id}` : item.group_id ? `用户组 ${item.group_id}` : '未知授权对象')}</strong>
        {item.user_email && item.user_display_name && <p>{item.user_email}</p>}
        <p>{current ? '当前授权' : '历史授权'} · {item.role} · 剪贴板{item.allow_clipboard ? '允许' : '禁止'} · 文件{item.allow_file_transfer ? '允许' : '禁止'} · 音频{item.allow_audio ? '允许' : '禁止'}</p>
        {item.expires_at && <p>到期时间：{dateLabel(item.expires_at)}</p>}
      </div>
      {!current ? <span className="muted">{status === 'revoked' ? '已撤销' : status === 'expired' ? '已过期' : '有效期未知'}</span> : revoke === item.id ? <>
        <button disabled={busy} className="danger" onClick={async () => {
          if (await run(() => api.request({ op: 'revoke_access', entitlement_id: item.id }), false)) { setRevoke(null); query.reload(); }
        }}>确认撤销</button>
        <button disabled={busy} onClick={() => setRevoke(null)}>取消</button>
      </> : <button className="text-button danger-text" disabled={busy} onClick={() => setRevoke(item.id)}>撤销</button>}
    </div>;
  })}</div>}<p className="notice">{admissionNotice}</p>
      <form onSubmit={grant} className="grant-form"><h3>添加访问用户</h3><label>用户完整邮箱<input autoFocus type="email" required value={email} onChange={e => setEmail(e.target.value)} placeholder="name@company.com" autoComplete="off" /></label><label>访问角色<select value={role} onChange={e => { setRole(e.target.value); if (e.target.value === 'VIEWER') { setClipboard(false); setFiles(false); setAudio(false); } }}><option value="VIEWER">仅查看</option><option value="CONTROLLER">控制桌面</option><option value="ADMIN">资源管理员</option></select></label><div className="policy-options"><label className="checkbox"><input type="checkbox" disabled={viewer || busy} checked={clipboard} onChange={e => setClipboard(e.target.checked)} />剪贴板</label><label className="checkbox"><input type="checkbox" disabled={viewer || busy} checked={files} onChange={e => setFiles(e.target.checked)} />文件传输</label><label className="checkbox"><input type="checkbox" disabled={viewer || busy} checked={audio} onChange={e => setAudio(e.target.checked)} />音频</label></div>{viewer && <p className="muted">仅查看角色不允许输入、剪贴板、文件传输和音频。</p>}{role === 'ADMIN' && <p className="muted">此角色仅作用于当前资源，不授予工作空间用户目录管理权限。</p>}<p className="muted">精确匹配已启用的账号邮箱，不搜索或展示用户目录。</p><button className="primary" disabled={busy || query.loading} type="submit">{busy ? '正在处理…' : '添加授权'}</button></form>
    </div></Modal>;
}
