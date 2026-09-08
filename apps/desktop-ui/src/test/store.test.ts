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
  it.each([false, true])('ignores stale poll and refresh after logout (reject=%s)', async reject => {
    const pending = deferred<SessionType[]>();
    const reads = deferred<Resource[]>();
    let count = 0;
    const api = fakeApi({ logout: () => reject ? Promise.reject(new Error('IPC interrupted')) : null, sessions: () => pending.promise, resources: () => ++count === 1 ? [resource] : reads.promise });
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
  it('clears account-scoped data when host cleared auth before remote logout rejected', async () => {
    let signedIn = true;
    const api = fakeApi({
      account: () => signedIn ? account : null,
      sessions: () => [session],
      logout: () => { signedIn = false; return Promise.reject({ code: 'timeout', message: '服务器撤销超时' }); },
    });
    const store = new DesktopStore(api);
    await store.start();
    await store.poll();
    expect(store.snapshot().resources).toEqual([resource]);
    expect(store.snapshot().sessions).toEqual([session]);
    expect(await store.logout()).toBe(false);
    expect(await api.request({ op: 'account' })).toBeNull();
    expect(store.snapshot()).toMatchObject({ account: null, resources: [], machines: [], host: null, sessions: [], transfers: [], busy: false });
    expect(store.snapshot().error).toContain('服务器撤销超时');
    expect(store.snapshot().error).toContain('未能确认');
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
