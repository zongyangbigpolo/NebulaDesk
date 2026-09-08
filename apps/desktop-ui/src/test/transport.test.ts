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
});
