import { AppConfig } from '../config';
import { ForbiddenError, NotFoundError } from '../domain/errors';
import { AuthenticatedUser, DeviceRecord, EffectiveRole } from '../domain/types';
import { DataStore } from '../repositories/types';
import { randomClaimCode, randomRelayDeviceId, randomToken, sha256 } from '../utils/crypto';
import { timingSafeEqual } from 'node:crypto';

export interface DeviceListItem {
  id: string;
  name: string;
  role: EffectiveRole;
  relayDeviceId: string;
  createdAt: string;
  lastSeenAt: string | null;
  online: boolean;
}

export class DeviceService {
  constructor(
    private readonly store: DataStore,
    private readonly config: Pick<AppConfig, 'CLAIM_CODE_TTL_SECONDS' | 'DEVICE_ONLINE_WINDOW_SECONDS' | 'RELAY_PUBLIC_HOST' | 'RELAY_PUBLIC_PORT'>,
  ) {}

  async createDevice(user: AuthenticatedUser, name: string): Promise<{
    device: DeviceRecord;
    relayToken: string;
    claimCode: string;
    claimCodeExpiresAt: Date;
  }> {
    const relayToken = randomToken(32);
    const claimCode = randomClaimCode();
    const claimCodeExpiresAt = new Date(Date.now() + this.config.CLAIM_CODE_TTL_SECONDS * 1000);
    const device = await this.store.createDevice({
      ownerUserId: user.userId,
      name: name.trim(),
      relayDeviceId: randomRelayDeviceId(),
      relayTokenHash: sha256(relayToken),
      claimCodeHash: sha256(claimCode),
      claimCodeExpiresAt,
    });

    return { device, relayToken, claimCode, claimCodeExpiresAt };
  }

  async redeemClaimCode(claimCode: string): Promise<{
    device: DeviceRecord;
    relayToken: string;
  }> {
    const relayToken = randomToken(32);
    const device = await this.store.redeemClaimCode({
      claimCodeHash: sha256(claimCode.trim().toUpperCase()),
      now: new Date(),
      newRelayTokenHash: sha256(relayToken),
    });

    if (!device) {
      throw new NotFoundError('Claim code is invalid or expired');
    }

    return { device, relayToken };
  }

  async listAccessibleDevices(user: AuthenticatedUser): Promise<DeviceListItem[]> {
    const devices = await this.store.listAccessibleDevices(user.userId);
    const now = Date.now();
    const onlineWindowMs = this.config.DEVICE_ONLINE_WINDOW_SECONDS * 1000;
    return devices.map(({ device, role }) => ({
      id: device.id,
      name: device.name,
      role,
      relayDeviceId: device.relayDeviceId,
      createdAt: device.createdAt.toISOString(),
      lastSeenAt: device.lastSeenAt?.toISOString() ?? null,
      online: device.lastSeenAt ? now - device.lastSeenAt.getTime() <= onlineWindowMs : false,
    }));
  }

  async deleteDevice(user: AuthenticatedUser, deviceId: string): Promise<void> {
    const device = await this.store.findDeviceById(deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }
    if (device.ownerUserId !== user.userId) {
      throw new ForbiddenError('Only the owner can delete a device');
    }
    await this.store.deleteDevice(deviceId);
  }

  async heartbeat(deviceId: string, actorUserId?: string): Promise<DeviceRecord> {
    const device = await this.store.findDeviceById(deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }
    if (actorUserId && device.ownerUserId !== actorUserId) {
      throw new ForbiddenError('Only the owner can update heartbeat without relay shared secret');
    }
    const updated = await this.store.touchDeviceHeartbeat(deviceId, new Date());
    if (!updated) {
      throw new NotFoundError('Device not found');
    }
    return updated;
  }

  getRelayConnectionInfo(device: DeviceRecord): { relayHost: string; relayPort: number; relayDeviceId: string } {
    return {
      relayHost: this.config.RELAY_PUBLIC_HOST,
      relayPort: this.config.RELAY_PUBLIC_PORT,
      relayDeviceId: device.relayDeviceId,
    };
  }

  // Verifies a VDA's long-lived relay token (the same credential it uses to
  // register with nebula_relay) — used by the WebRTC signaling WS endpoint
  // to authenticate an inbound "hello" from a VDA. Constant-time compare on
  // the hash to avoid timing side-channels.
  async verifyRelayToken(relayDeviceId: string, token: string): Promise<DeviceRecord | null> {
    const device = await this.store.findDeviceByRelayDeviceId(relayDeviceId);
    if (!device) return null;
    const presented = Buffer.from(sha256(token));
    const expected = Buffer.from(device.relayTokenHash);
    if (presented.length !== expected.length) return null;
    return timingSafeEqual(presented, expected) ? device : null;
  }
}
