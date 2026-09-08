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
  const message = error instanceof Error ? error.message : typeof error === 'string' ? error :
    error && typeof error === 'object' && 'message' in error && typeof error.message === 'string' ? error.message : '';
  return message ? `操作未完成：${message}` : '操作未完成，请稍后重试。';
}
