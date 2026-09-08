export type Account = { id: string; email: string; display_name: string; role: string; tenant: string; manager_url: string };
export type Policy = { input: boolean; audio: boolean; clipboard: boolean; file_transfer: boolean };
export type Resource = {
  id: string; name: string; kind: 'DESKTOP' | 'APP'; description: string;
  machine_status: string; role: string | null; policy: Policy; owner_name: string | null;
  owned: boolean; machine_id: string | null; os: string | null; os_version: string | null;
  last_seen_at: string | null; launch_supported: boolean;
};
export type Machine = {
  id: string; name: string; os: string; os_version: string; arch: string; status: string;
  owner_user_id: string | null; last_seen_at: string | null; capabilities: Record<string, unknown>;
};
export type PublishedResource = {
  id: string; machine_id: string; kind: 'DESKTOP' | 'APP'; name: string;
  description: string; enabled: boolean; launch_path: string | null;
};
export type Grant = {
  id: string; resource_id: string; user_id: string | null; group_id: string | null;
  role: string; allow_clipboard: boolean; allow_file_transfer: boolean; allow_audio: boolean;
};
export type Session = {
  session_id: string; resource_id: string; name: string;
  state: 'connecting' | 'connected' | 'disconnected' | 'failed';
  path: 'direct' | 'relay' | null; started_at: string; error: string | null; rtt_ms: number | null;
};
export type Permission = 'granted' | 'denied' | 'unknown';
export type LocalHost = {
  enrolled: boolean; machine_id: string | null; manager_url: string | null; name: string | null;
  running: boolean; permissions: { screen: Permission; input: Permission; audio: Permission }; error: string | null;
};
export type Transfer = {
  id: string; session_id: string; name: string; direction: 'send' | 'receive';
  transferred: number; total: number; state: 'offered' | 'transferring' | 'complete' | 'failed'; error: string | null;
};
export type Publication = { kind: 'DESKTOP' | 'APP'; name: string; description: string; launch_path?: string; launch_args?: string[] };
export type Changes = { name?: string; description?: string; enabled?: boolean };

export type Commands = {
  login: { args: { manager_url: string; tenant: string; email: string; password: string; allow_insecure_http?: boolean }; result: Account };
  logout: { args: object; result: null };
  account: { args: object; result: Account | null };
  resources: { args: object; result: Resource[] };
  machines: { args: object; result: Machine[] };
  resource: { args: { id: string }; result: Resource };
  connect: { args: { resource_id: string }; result: Session };
  sessions: { args: object; result: Session[] };
  focus_session: { args: { session_id: string }; result: null };
  disconnect_session: { args: { session_id: string }; result: null };
  local_host: { args: object; result: LocalHost };
  create_enrollment: { args: { name: string }; result: { token: string; expires_at: string } };
  enroll_local: { args: { manager_url: string; token: string; name: string; allow_insecure_http?: boolean }; result: LocalHost };
  set_host_enabled: { args: { enabled: boolean }; result: LocalHost };
  rename_machine: { args: { machine_id: string; name: string }; result: null };
  remove_machine: { args: { machine_id: string }; result: null };
  machine_resources: { args: { machine_id: string }; result: PublishedResource[] };
  publish_resource: { args: { machine_id: string; resource: Publication }; result: PublishedResource };
  update_resource: { args: { resource_id: string; changes: Changes }; result: null };
  grants: { args: { resource_id: string }; result: Grant[] };
  grant_access: { args: { resource_id: string; email: string; role: string; allow_clipboard: boolean; allow_file_transfer: boolean; allow_audio: boolean }; result: Grant };
  revoke_access: { args: { entitlement_id: string }; result: null };
  transfers: { args: object; result: Transfer[] };
  send_files: { args: { session_id: string }; result: null };
  open_permission_settings: { args: { permission: 'screen' | 'input' | 'audio' }; result: null };
};
export type Request<K extends keyof Commands = keyof Commands> = {
  [P in K]: { op: P } & Commands[P]['args']
}[K];
export interface DesktopApi {
  request<K extends keyof Commands>(request: Request<K>): Promise<Commands[K]['result']>;
}
