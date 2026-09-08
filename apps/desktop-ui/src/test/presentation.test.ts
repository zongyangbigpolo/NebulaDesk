import { describe, expect, it } from 'vitest';
import { machineStatusLabel, shortcutLabel, unavailable } from '../ui/common';
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
});
