import { invoke, isTauri } from '@tauri-apps/api/core';
import type { DesktopApi } from './types';

export const desktopApi: DesktopApi = {
  request(request) {
    if (!isTauri()) {
      return Promise.reject(new Error('请在 NebulaDesk 桌面客户端中打开。浏览器预览请显式使用 ?demo=1。'));
    }
    return invoke('desktop_request', { request });
  },
};

export function errorMessage(error: unknown): string {
  const code = error && typeof error === 'object' && 'code' in error && typeof error.code === 'string' ? error.code : '';
  switch (code) {
    case 'network_timeout': return '连接服务器超时。请检查工作空间地址、网络或 VPN，并确认服务器在线；这不表示密码错误。';
    case 'network_connection': return '无法连接服务器。请检查工作空间地址、网络或 VPN，以及服务器的 HTTPS 端口是否可访问。';
    case 'tls_certificate': return '服务器证书校验失败。请检查 HTTPS 地址、系统时间及服务器证书的有效期和信任链。';
    case 'tls': return '无法建立 HTTPS 安全连接。请确认地址对应 HTTPS 服务，并检查服务器的 TLS 配置。';
    case 'network': return '与服务器通信失败。请检查网络和服务器状态后重试。';
    case 'invalid_credentials': return '登录失败。请检查工作空间标识、邮箱和密码，或确认账号未被停用。';
    case 'unauthorized': return '登录状态已失效，请重新登录。';
  }
  const message = error instanceof Error ? error.message : typeof error === 'string' ? error :
    error && typeof error === 'object' && 'message' in error && typeof error.message === 'string' ? error.message : '';
  return message ? `操作未完成：${message}` : '操作未完成，请稍后重试。';
}
