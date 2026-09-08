import { useState, type FormEvent } from 'react';
import { LockKeyhole } from 'lucide-react';
import type { Commands } from '../api/types';
import { ErrorNotice } from './common';
import { HttpOptIn } from './dialogs';

export function Login({ busy, error, demo, onLogin }: { busy: boolean; error: string | null; demo: boolean; onLogin: (input: Commands['login']['args']) => Promise<boolean> }) {
  const [managerUrl, setManagerUrl] = useState('');
  const [tenant, setTenant] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [http, setHttp] = useState(false);
  async function submit(event: FormEvent) {
    event.preventDefault();
    const pending = onLogin({ manager_url: managerUrl.trim(), tenant: tenant.trim(), email: email.trim(), password, allow_insecure_http: http });
    setPassword('');
    await pending;
  }
  return <main className="login-page"><section className="login-intro"><div className="brand"><span className="brand-mark">N</span>NebulaDesk</div><div><h1>你的工作空间，<br />始终在身边。</h1><p>连接自己的电脑，访问团队共享的资源。<br />熟悉的桌面，在独立的原生窗口中呈现。</p></div><small>安全连接 · 独立会话 · 清晰授权</small></section><section className="login-form"><form onSubmit={submit}><LockKeyhole size={28} /><h1>登录工作空间</h1><p>使用管理员提供的地址和你的账号。</p>{demo && <div className="notice">演示模式：请勿输入真实账号或密码。</div>}<ErrorNotice message={error} /><label>工作空间地址<input autoFocus type="url" required value={managerUrl} onChange={e => setManagerUrl(e.target.value)} placeholder="https://manager.company.com" /></label><label>工作空间标识<input required value={tenant} onChange={e => setTenant(e.target.value)} placeholder="管理员提供的租户标识" /></label><label>邮箱<input type="email" required value={email} onChange={e => setEmail(e.target.value)} autoComplete="username" /></label><label>密码<input type="password" required value={password} onChange={e => setPassword(e.target.value)} autoComplete="current-password" /></label><HttpOptIn url={managerUrl} value={http} onChange={setHttp} /><button className="primary" disabled={busy || (managerUrl.toLowerCase().startsWith('http:') && !http)}>{busy ? '正在登录…' : '登录'}</button><p className="footnote">凭据由桌面后端管理，不写入浏览器存储。</p></form></section></main>;
}
