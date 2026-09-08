import { describe, expect, it } from 'vitest';
import { DesktopStore } from '../state/store';
import { account, deferred, fakeApi, resource, session } from './fixtures';
import type { Resource, Session as SessionType } from '../api/types';

describe('desktop state boundary', () => {
  it('does not overlap session/transfer polls', async () => {
    const pending = deferred<SessionType[]>();
    const api = fakeApi({ sessions: () => pending.promise });
    const store = new DesktopStore(api);
    await store.start();
    const first = store.poll();
    await store.poll();
    expect(api.calls.filter(r => r.op === 'sessions')).toHaveLength(1);
    pending.resolve([session]);
    await first;
    expect(store.snapshot().sessions).toEqual([session]);
  });
  it('ignores a stale poll and refresh after logout', async () => {
    const pending = deferred<SessionType[]>();
    const reads = deferred<Resource[]>();
    let count = 0;
    const api = fakeApi({ sessions: () => pending.promise, resources: () => ++count === 1 ? [resource] : reads.promise });
    const store = new DesktopStore(api);
    await store.start();
    const poll = store.poll();
    const refresh = store.refresh();
    await store.logout();
    pending.resolve([session]); reads.resolve([resource]);
    await Promise.all([poll, refresh]);
    expect(store.snapshot().account).toBeNull();
    expect(store.snapshot().resources).toEqual([]);
    expect(store.snapshot().sessions).toEqual([]);
  });
  it('keeps current account and exposes structured error on logout failure', async () => {
    const store = new DesktopStore(fakeApi({ logout: () => Promise.reject({ code: 'process_error', message: '会话关闭失败' }) }));
    await store.start();
    expect(await store.logout()).toBe(false);
    expect(store.snapshot().account).toEqual(account);
    expect(store.snapshot().error).toContain('会话关闭失败');
  });
  it('never substitutes demonstration resources on API failure', async () => {
    const store = new DesktopStore(fakeApi({ resources: () => Promise.reject(new Error('连接工作空间失败')) }));
    await store.start();
    expect(store.snapshot().resources).toEqual([]);
    expect(store.snapshot().error).toContain('连接工作空间失败');
  });
  it('rejects duplicate mutations and propagates their failure', async () => {
    const pending = deferred<null>();
    const store = new DesktopStore(fakeApi());
    await store.start();
    const first = store.action(() => pending.promise);
    expect(await store.action(() => Promise.resolve(null))).toBe(false);
    pending.reject(new Error('未授权'));
    expect(await first).toBe(false);
    expect(store.snapshot().busy).toBe(false);
    expect(store.snapshot().error).toContain('未授权');
  });
  it('discards polls started before a session mutation', async () => {
    const pending = deferred<SessionType[]>();
    const api = fakeApi({ sessions: () => pending.promise });
    const store = new DesktopStore(api);
    await store.start();
    const poll = store.poll();
    await store.action(() => Promise.resolve(null), false);
    pending.resolve([session]);
    await poll;
    expect(store.snapshot().sessions).toEqual([]);
  });
});
