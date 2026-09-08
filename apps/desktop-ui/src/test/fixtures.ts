import type { Account, Commands, DesktopApi, LocalHost, Machine, Request, Resource, Session } from '../api/types';

export const account: Account = { id: 'owner', display_name: '测试用户', email: 'owner@example.test', tenant: '测试空间', manager_url: 'https://manager.example.test', role: 'USER' };
export const resource: Resource = { id: 'desktop', name: '测试电脑', kind: 'DESKTOP', description: '设备描述', machine_status: 'ONLINE', role: 'CONTROLLER', policy: { input: true, audio: false, clipboard: true, file_transfer: true }, owner_name: '测试用户', owned: true, machine_id: 'machine', os: 'macOS', os_version: null, last_seen_at: null, launch_supported: true };
export const machine: Machine = { id: 'machine', name: resource.name, os: 'macOS', os_version: '', arch: 'arm64', status: 'ONLINE', owner_user_id: account.id, last_seen_at: null, capabilities: {} };
export const host: LocalHost = { enrolled: true, machine_id: 'machine', name: resource.name, manager_url: account.manager_url, running: true, permissions: { screen: 'unknown', input: 'denied', audio: 'unknown' }, error: null };
export const session: Session = { session_id: 'session', resource_id: resource.id, name: resource.name, state: 'connected', path: 'direct', started_at: '2026-09-08T01:00:00Z', rtt_ms: 4, error: null };
export type Handlers = { [K in keyof Commands]?: (request: Request<K>) => Commands[K]['result'] | Promise<Commands[K]['result']> };
export function fakeApi(overrides: Handlers = {}): DesktopApi & { calls: Request[] } {
  const handlers: Handlers = { account: () => account, resources: () => [resource], machines: () => [machine], local_host: () => host, sessions: () => [], transfers: () => [], resource: () => resource, machine_resources: () => [], grants: () => [], logout: () => null, ...overrides };
  const calls: Request[] = [];
  return {
    calls,
    async request<K extends keyof Commands>(request: Request<K>) {
      calls.push(request as Request);
      const handler = handlers[request.op];
      if (!handler) throw new Error(`Unconfigured command: ${request.op}`);
      return await handler(request) as Commands[K]['result'];
    },
  };
}
export function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
