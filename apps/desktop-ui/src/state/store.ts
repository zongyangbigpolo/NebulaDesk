import type { Account, DesktopApi, LocalHost, Machine, Resource, Session, Transfer } from '../api/types';
import { errorMessage } from '../api/desktop';

export type Snapshot = {
  account: Account | null; ready: boolean; busy: boolean; loading: boolean; error: string | null;
  resources: Resource[]; machines: Machine[]; host: LocalHost | null; sessions: Session[]; transfers: Transfer[];
};
const empty = (): Snapshot => ({
  account: null, ready: false, busy: false, loading: false, error: null,
  resources: [], machines: [], host: null, sessions: [], transfers: [],
});

// Epochs isolate authentication lifetimes; sequence numbers prevent older reads replacing mutations.
export class DesktopStore {
  private state = empty();
  private listeners = new Set<() => void>();
  private epoch = 0;
  private reads = 0;
  private pollPending = false;
  private mutationPending = false;
  constructor(readonly api: DesktopApi) {}
  snapshot = () => this.state;
  subscribe = (listener: () => void) => { this.listeners.add(listener); return () => { this.listeners.delete(listener); }; };
  private set(patch: Partial<Snapshot>) { this.state = { ...this.state, ...patch }; this.listeners.forEach(fn => fn()); }
  clearError = () => this.set({ error: null });
  async start() {
    const epoch = ++this.epoch;
    try {
      const account = await this.api.request({ op: 'account' });
      if (epoch !== this.epoch) return;
      this.set({ account, ready: true });
      if (account) await this.refresh();
    } catch (error) {
      if (epoch === this.epoch) this.set({ ready: true, error: errorMessage(error) });
    }
  }
  async login(input: { manager_url: string; tenant: string; email: string; password: string; allow_insecure_http?: boolean }) {
    if (this.mutationPending) return false;
    this.mutationPending = true;
    const epoch = ++this.epoch;
    this.set({ busy: true, error: null });
    try {
      const account = await this.api.request({ op: 'login', ...input });
      if (epoch !== this.epoch) return false;
      this.set({ account, ready: true });
      await this.refresh();
      return true;
    } catch (error) {
      if (epoch === this.epoch) this.set({ error: errorMessage(error) });
      return false;
    } finally {
      this.mutationPending = false;
      if (epoch === this.epoch) this.set({ busy: false });
    }
  }
  async logout() {
    if (this.mutationPending) return false;
    this.mutationPending = true;
    const epoch = ++this.epoch;
    this.set({ busy: true, error: null });
    try {
      await this.api.request({ op: 'logout' });
      if (epoch === this.epoch) { this.state = { ...empty(), ready: true }; this.set({}); }
      return true;
    } catch (error) {
      if (epoch === this.epoch) this.set({ error: errorMessage(error) });
      return false;
    } finally {
      this.mutationPending = false;
      if (epoch === this.epoch) this.set({ busy: false, loading: false });
    }
  }
  async refresh() {
    if (!this.state.account) return;
    const epoch = this.epoch;
    const read = ++this.reads;
    this.set({ loading: true, error: null });
    const results = await Promise.allSettled([
      this.api.request({ op: 'resources' }), this.api.request({ op: 'machines' }),
      this.api.request({ op: 'local_host' }),
    ]);
    if (epoch !== this.epoch || read !== this.reads) return;
    const [resources, machines, host] = results;
    this.set({
      loading: false,
      resources: resources.status === 'fulfilled' ? resources.value : [],
      machines: machines.status === 'fulfilled' ? machines.value : [],
      host: host.status === 'fulfilled' ? host.value : null,
      error: results.filter(r => r.status === 'rejected').map(r => errorMessage(r.reason)).join('；') || null,
    });
  }
  async poll() {
    if (!this.state.account || this.pollPending || this.mutationPending) return;
    this.pollPending = true;
    const epoch = this.epoch;
    const read = this.reads;
    try {
      const [sessions, transfers] = await Promise.all([
        this.api.request({ op: 'sessions' }), this.api.request({ op: 'transfers' }),
      ]);
      if (epoch === this.epoch && read === this.reads) this.set({ sessions, transfers });
    } catch (error) {
      if (epoch === this.epoch && read === this.reads) this.set({ error: errorMessage(error) });
    } finally { this.pollPending = false; }
  }
  async action(work: () => Promise<unknown>, refresh = true) {
    if (this.mutationPending || !this.state.account) return false;
    this.mutationPending = true;
    const epoch = this.epoch;
    ++this.reads;
    this.set({ busy: true, loading: false, error: null });
    try {
      await work();
      if (epoch !== this.epoch) return false;
      if (refresh) await this.refresh();
      return true;
    } catch (error) {
      if (epoch === this.epoch) this.set({ error: errorMessage(error) });
      return false;
    } finally {
      this.mutationPending = false;
      if (epoch === this.epoch) { this.set({ busy: false }); void this.poll(); }
    }
  }
}
