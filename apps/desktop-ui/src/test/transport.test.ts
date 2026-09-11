import { beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { desktopApi, errorMessage } from '../api/desktop';
import { resource } from './fixtures';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn(), isTauri: vi.fn() }));
beforeEach(() => { vi.mocked(isTauri).mockReturnValue(true); });
describe('Tauri IPC adapter', () => {
  it('uses the single desktop_request envelope with typed output', async () => {
    vi.mocked(invoke).mockResolvedValue(resource);
    const reply = await desktopApi.request({ op: 'resource', id: 'desktop' });
    expect(reply.policy.file_transfer).toBe(true);
    expect(invoke).toHaveBeenCalledWith('desktop_request', { request: { op: 'resource', id: 'desktop' } });
  });
  it('does not fall back to demo in a plain browser', async () => {
    vi.mocked(isTauri).mockReturnValue(false);
    await expect(desktopApi.request({ op: 'account' })).rejects.toThrow('桌面客户端');
  });
  it('propagates rejected commands and supports structured errors', async () => {
    const error = { code: 'denied', message: '没有权限' };
    vi.mocked(invoke).mockRejectedValue(error);
    await expect(desktopApi.request({ op: 'resources' })).rejects.toEqual(error);
    expect(errorMessage(error)).toBe('操作未完成：没有权限');
  });
  it.each([
    ['network_timeout', '连接服务器超时'],
    ['network_connection', '无法连接服务器'],
    ['tls_certificate', '服务器证书校验失败'],
    ['tls', '无法建立 HTTPS 安全连接'],
    ['network', '与服务器通信失败'],
    ['invalid_credentials', '工作空间标识、邮箱和密码'],
    ['unauthorized', '登录状态已失效'],
  ])('translates %s without exposing raw transport details', (code, expected) => {
    const message = errorMessage({ code, message: 'SECRET internal details' });
    expect(message).toContain(expected);
    expect(message).not.toContain('SECRET');
    expect(message).not.toContain('操作未完成');
  });
});
