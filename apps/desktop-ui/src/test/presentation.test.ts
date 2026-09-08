import { describe, expect, it } from 'vitest';
import { grantStatus, machineStatusLabel, shortcutLabel, unavailable } from '../ui/common';
import type { Grant } from '../api/types';
import { resource } from './fixtures';

describe('truthful resource metadata', () => {
  it('distinguishes unknown, offline, and online devices', () => {
    expect(machineStatusLabel(resource)).toBe('在线');
    expect(machineStatusLabel({ ...resource, machine_status: 'OFFLINE' })).toBe('离线');
    expect(machineStatusLabel({ ...resource, machine_status: 'UNRECOGNIZED' })).toBe('状态未知');
    expect(unavailable({ ...resource, machine_status: 'UNRECOGNIZED' })).toBe('设备状态未知');
  });
  it('shows the platform-specific search shortcut without changing behavior', () => {
    expect(shortcutLabel('MacIntel')).toBe('⌘ K');
    expect(shortcutLabel('Win32')).toBe('Ctrl K');
    expect(shortcutLabel('Linux x86_64')).toBe('Ctrl K');
  });
  it('classifies expiry boundaries and prioritizes revoked state', () => {
    const grant: Grant = { id: 'grant', resource_id: resource.id, user_id: 'recipient', group_id: null, role: 'VIEWER', allow_audio: false, allow_clipboard: false, allow_file_transfer: false };
    const now = Date.parse('2026-09-08T00:00:00Z');
    expect(grantStatus(grant, now)).toBe('active');
    expect(grantStatus({ ...grant, expires_at: '2026-09-08T00:00:00Z' }, now)).toBe('expired');
    expect(grantStatus({ ...grant, expires_at: '2026-09-08T00:00:01Z' }, now)).toBe('active');
    expect(grantStatus({ ...grant, expires_at: 'invalid' }, now)).toBe('unknown');
    expect(grantStatus({ ...grant, revoked_at: '2026-09-01T00:00:00Z', expires_at: '2026-09-08T00:00:00Z' }, now)).toBe('revoked');
  });
});
