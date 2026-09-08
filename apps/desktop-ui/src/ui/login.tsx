import { useEffect, useRef, useState, type FormEvent } from 'react';
import { LockKeyhole } from 'lucide-react';
import type { AuthenticationRequest, DesktopApi, Workspace } from '../api/types';
import { errorMessage } from '../api/desktop';
import { ErrorNotice } from './common';
import { HttpOptIn } from './dialogs';

export const exactInput = { autoCapitalize: 'none', autoCorrect: 'off', spellCheck: false } as const;
const byteLength = (value: string) => new TextEncoder().encode(value).length;
const invalidName = (value: string) => byteLength(value) > 200 || /[\u0000-\u001f\u007f-\u009f]/.test(value);
type Mode = 'login' | 'register' | 'invite';
type Props = { api: DesktopApi; busy: boolean; error: string | null; demo: boolean; onAuthenticate: (request: AuthenticationRequest) => Promise<boolean>; clearError: () => void };

export function Login(props: Props) {
  const [mode, setMode] = useState<Mode>('login');
  const [managerUrl, setManagerUrl] = useState('');
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const editedServer = useRef(false);
  useEffect(() => {
    let active = true;
    props.api.request({ op: 'connection_settings' }).then(settings => {
      if (active && !editedServer.current) setManagerUrl(settings.manager_url ?? '');
    }, error => { if (active) setSettingsError(errorMessage(error)); });
    return () => { active = false; };
  }, [props.api]);
  function updateManagerUrl(value: string) { editedServer.current = true; setManagerUrl(value); setSettingsError(null); }
  function changeMode(next: Mode) { props.clearError(); setMode(next); }
  return <main className="login-page">
    <section className="login-intro"><div className="brand"><span className="brand-mark">N</span>NebulaDesk</div><div><h1>你的工作空间，<br />始终在身边。</h1><p>连接自己的电脑，访问团队共享的资源。<br />熟悉的桌面，在独立的原生窗口中呈现。</p></div><small>安全连接 · 独立会话 · 清晰授权</small></section>
    <section className="login-form"><AccountForm key={mode} {...props} error={props.error ?? settingsError} mode={mode} changeMode={changeMode} managerUrl={managerUrl} setManagerUrl={updateManagerUrl} /></section>
  </main>;
}

function AccountForm({ api, busy, error, demo, onAuthenticate, mode, changeMode, managerUrl, setManagerUrl }: Props & { mode: Mode; changeMode: (mode: Mode) => void; managerUrl: string; setManagerUrl: (value: string) => void }) {
  const [tenant, setTenant] = useState('');
  const [workspaceName, setWorkspaceName] = useState('');
  const [kind, setKind] = useState<Workspace['kind']>('PERSONAL');
  const [displayName, setDisplayName] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [token, setToken] = useState('');
  const [http, setHttp] = useState(false);
  const [serverReady, setServerReady] = useState(false);
  const [checking, setChecking] = useState(false);
  const [disabledRegistration, setDisabledRegistration] = useState(false);
  const [localError, setLocalError] = useState<string | null>(null);
  const checkSequence = useRef(0);
  const signup = mode !== 'login';
  const httpBlocked = managerUrl.trim().toLowerCase().startsWith('http:') && !http;
  const pending = busy || checking;
  const mismatch = signup && confirm.length > 0 && password !== confirm;
  const fieldError = !signup || !serverReady ? null :
    invalidName(displayName) || (mode === 'register' && invalidName(workspaceName)) ? '名称最多 200 字节（中文字符通常占 3 字节），不能包含控制字符。' :
      byteLength(email.trim()) > 254 ? '邮箱最多 254 字节。' :
        mode === 'invite' && byteLength(token) > 128 ? '邀请码最多 128 字节。请完整粘贴管理员提供的邀请码，不要加入额外文字。' :
        byteLength(password) > 1024 ? '密码最多 1024 字节。' : null;
  const heading = mode === 'login' ? '登录工作空间' : mode === 'register' ? '创建账号与工作空间' : '凭邀请加入组织';
  function resetServer() {
    ++checkSequence.current;
    setChecking(false); setServerReady(false); setDisabledRegistration(false); setLocalError(null);
    setPassword(''); setConfirm(''); setToken('');
  }
  async function submit(event: FormEvent) {
    event.preventDefault();
    if (pending || httpBlocked) return;
    setLocalError(null);
    const server = { manager_url: managerUrl.trim(), allow_insecure_http: http };
    if (signup && !serverReady) {
      if (mode === 'invite') { setServerReady(true); return; }
      const sequence = ++checkSequence.current;
      setChecking(true);
      try {
        const options = await api.request({ op: 'registration_options', ...server });
        if (sequence !== checkSequence.current) return;
        setDisabledRegistration(!options.self_registration_enabled);
        setServerReady(options.self_registration_enabled);
      } catch (failure) {
        if (sequence === checkSequence.current) setLocalError(errorMessage(failure));
      } finally {
        if (sequence === checkSequence.current) setChecking(false);
      }
      return;
    }
    if (fieldError) { setLocalError(fieldError); return; }
    if (signup && (Array.from(password).length < 12 || password !== confirm || !displayName.trim())) {
      setLocalError('请填写显示名称，使用至少 12 个字符的密码，并确认两次密码一致。');
      return;
    }
    const identity = { display_name: displayName.trim(), email: email.trim(), password };
    const request: AuthenticationRequest = mode === 'login' ? { op: 'login', ...server, tenant: tenant.trim(), email: email.trim(), password } :
      mode === 'register' ? { op: 'register', ...server, ...identity, workspace_slug: tenant.trim(), workspace_name: workspaceName.trim(), workspace_kind: kind } :
        { op: 'accept_invitation', ...server, ...identity, token: token.trim() };
    setPassword(''); setConfirm(''); setToken('');
    await onAuthenticate(request);
  }
  return <form onSubmit={submit} {...exactInput}>
    <LockKeyhole size={28} /><h1>{heading}</h1>
    <p>{mode === 'login' ? '使用工作空间地址、标识和你的账号登录。' : mode === 'register' ? '先连接服务器，再创建一个全新的个人或组织空间。' : '使用组织管理员提供的邀请码创建此组织内的账号。'}</p>
    {demo && <div className="notice">演示模式：仅示例数据，请勿输入真实账号、密码或邀请码。</div>}
    <ErrorNotice message={fieldError ?? localError ?? error} />
    <fieldset disabled={busy}>
      <label>工作空间地址<input {...exactInput} autoFocus type="url" required value={managerUrl} onChange={e => { resetServer(); setManagerUrl(e.target.value); setHttp(false); }} placeholder="https://manager.company.com" readOnly={signup && serverReady} /></label>
      {!serverReady && <HttpOptIn url={managerUrl} value={http} onChange={value => { resetServer(); setHttp(value); }} />}
      {signup && serverReady && <button type="button" className="text-button" onClick={resetServer}>更换服务器</button>}
      {disabledRegistration && <div className="notice" role="status">此服务器未开放自行注册。已有账号可登录；加入现有组织请使用管理员邀请。</div>}
      {(!signup || serverReady) && <>
        {mode === 'register' && <>
          <label>工作空间类型<select value={kind} onChange={e => setKind(e.target.value as Workspace['kind'])}><option value="PERSONAL">个人空间</option><option value="ORGANIZATION">创建新组织</option></select></label>
          <p className="footnote">你将成为新空间的管理员。输入已有组织标识不会加入该组织；加入现有组织需要邀请。</p>
          <label>工作空间名称<input required value={workspaceName} onChange={e => setWorkspaceName(e.target.value)} maxLength={200} /></label>
        </>}
        {mode !== 'invite' && <label>工作空间标识<input {...exactInput} required value={tenant} onChange={e => setTenant(e.target.value)} maxLength={signup ? 63 : 256} pattern={signup ? '[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?' : undefined} aria-describedby={signup ? 'slug-help' : undefined} placeholder={signup ? '例如 acme-team' : '例如 acme'} /></label>}
        {mode === 'register' && <p className="footnote" id="slug-help">1–63 位小写字母、数字或中间连字符。下次登录需要此标识。</p>}
        {mode === 'invite' && <><label>组织邀请码<input {...exactInput} required type="password" autoComplete="off" maxLength={128} value={token} onChange={e => setToken(e.target.value)} /></label><p className="footnote">邀请码有效期 48 小时，仅可用一次。组织与普通成员角色由邀请固定，不能自行选择。</p></>}
        {signup && <label>显示名称<input required value={displayName} onChange={e => setDisplayName(e.target.value)} maxLength={200} autoComplete="name" /></label>}
        <label>{mode === 'invite' ? '受邀邮箱' : '邮箱'}<input {...exactInput} type="email" required value={email} onChange={e => setEmail(e.target.value)} autoComplete="username" maxLength={signup ? 254 : 320} /></label>
        {signup && <p className="footnote">{mode === 'invite' ? '必须与邀请中的邮箱一致。' : ''}同一邮箱在不同工作空间是独立账号，并非通用账号。</p>}
        <label>密码<input {...exactInput} type="password" required value={password} onChange={e => setPassword(e.target.value)} minLength={signup ? 12 : undefined} maxLength={signup ? 1024 : 4096} autoComplete={signup ? 'new-password' : 'current-password'} aria-describedby={signup ? 'password-help' : undefined} /></label>
        {signup && <><p id="password-help" className="footnote">密码至少 12 个字符，最多 1024 字节。请使用独立密码，不要使用邀请码作为密码。</p><label>确认密码<input {...exactInput} type="password" required value={confirm} onChange={e => setConfirm(e.target.value)} autoComplete="new-password" aria-invalid={mismatch} /></label>{mismatch && <p className="warning" role="status">两次密码不一致。</p>}</>}
      </>}
      <button className="primary" disabled={pending || httpBlocked || mismatch || !!fieldError || (signup && serverReady && Array.from(password).length < 12)}>
        {pending ? '正在处理…' : signup && !serverReady ? disabledRegistration ? '重新检查注册状态' : '继续' : mode === 'login' ? '登录' : mode === 'register' ? '创建并登录' : '接受邀请并登录'}
      </button>
    </fieldset>
    <div className="auth-links">
      {mode !== 'login' && <button type="button" disabled={pending} onClick={() => changeMode('login')}>返回登录</button>}
      {mode !== 'register' && <button type="button" disabled={pending} onClick={() => changeMode('register')}>创建账号</button>}
      {mode !== 'invite' && <button type="button" disabled={pending} onClick={() => changeMode('invite')}>我有组织邀请</button>}
    </div>
    <p className="footnote">地址可由桌面部署配置提供，本次运行会记住最近成功登录的服务器。凭据不写入浏览器存储。账号注册不会自动添加或启动本机。</p>
  </form>;
}
