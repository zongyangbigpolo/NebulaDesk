import { AppConfig } from '../config';
import { ForbiddenError, NotFoundError, UnauthorizedError } from '../domain/errors';
import { AuthenticatedUser, DeviceRecord, EffectiveRole } from '../domain/types';
import { DataStore } from '../repositories/types';
import { randomClaimCode, randomRelayDeviceId, randomToken, sha256 } from '../utils/crypto';
import { timingSafeEqual } from 'node:crypto';

export interface DeviceListItem {
  id: string;
  name: string;
  role: EffectiveRole;
  relayDeviceId: string;
  groupId: string | null;
  createdAt: string;
  lastSeenAt: string | null;
  online: boolean;
}

export class DeviceService {
  constructor(
    private readonly store: DataStore,
    private readonly config: Pick<
      AppConfig,
      'CLAIM_CODE_TTL_SECONDS' | 'DEVICE_ONLINE_WINDOW_SECONDS' | 'RELAY_PUBLIC_HOST' | 'RELAY_PUBLIC_PORT' | 'DEVICE_ENROLLMENT_TOKEN'
    >,
  ) {}

  async createDevice(user: AuthenticatedUser, name: string): Promise<{
    device: DeviceRecord;
    relayToken: string;
    psk: string;
    claimCode: string;
    claimCodeExpiresAt: Date;
  }> {
    const relayToken = randomToken(32);
    const psk = randomToken(32);
    const claimCode = randomClaimCode();
    const claimCodeExpiresAt = new Date(Date.now() + this.config.CLAIM_CODE_TTL_SECONDS * 1000);
    const device = await this.store.createDevice({
      ownerUserId: user.userId,
      name: name.trim(),
      relayDeviceId: randomRelayDeviceId(),
      relayTokenHash: sha256(relayToken),
      psk,
      claimCodeHash: sha256(claimCode),
      claimCodeExpiresAt,
    });

    return { device, relayToken, psk, claimCode, claimCodeExpiresAt };
  }

  // Self-service registration (see README.md's "Self-registration & trial
  // credit"): any LOGGED-IN user who presents the shared DEVICE_ENROLLMENT_TOKEN
  // gets a device created under their OWN account directly — no admin has to
  // create it first and no separate claim-code round trip is needed, since
  // the caller is already authenticated. This is exactly createDevice() with
  // one extra check up front; it's what the Flutter manager's "Host this Mac"
  // tab calls.
  async selfRegisterDevice(user: AuthenticatedUser, name: string, presentedToken: string): ReturnType<DeviceService['createDevice']> {
    const expected = this.config.DEVICE_ENROLLMENT_TOKEN;
    if (!expected) {
      throw new ForbiddenError('Self-registration is not enabled on this server');
    }
    const presented = Buffer.from(presentedToken);
    const expectedBuf = Buffer.from(expected);
    const valid = presented.length === expectedBuf.length && timingSafeEqual(presented, expectedBuf);
    if (!valid) {
      throw new UnauthorizedError('Invalid enrollment token');
    }
    return this.createDevice(user, name);
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

    // The PSK is NOT rotated here: it's already returned once at creation
    // time (device.psk was set then) and claim-redemption only needs to hand
    // back the same value again for the install script to plug into
    // `nebula_vda --psk`, not generate a new end-to-end encryption secret.
    return { device, relayToken };
  }

  async listAccessibleDevices(user: AuthenticatedUser): Promise<DeviceListItem[]> {
    // Admins see every device in the system (role reported as 'ADMIN' so the
    // UI can distinguish "I can see this because I'm an admin" from actually
    // owning/being granted the device) rather than only their own/granted ones.
    const devices =
      user.role === 'ADMIN'
        ? (await this.store.listAllDevices()).map((device) => ({ device, role: 'ADMIN' as const }))
        : await this.store.listAccessibleDevices(user.userId);
    return this.toListItems(devices);
  }

  private toListItems(devices: { device: DeviceRecord; role: EffectiveRole }[]): DeviceListItem[] {
    const now = Date.now();
    const onlineWindowMs = this.config.DEVICE_ONLINE_WINDOW_SECONDS * 1000;
    return devices.map(({ device, role }) => ({
      id: device.id,
      name: device.name,
      role,
      relayDeviceId: device.relayDeviceId,
      groupId: device.groupId,
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
    if (device.ownerUserId !== user.userId && user.role !== 'ADMIN') {
      throw new ForbiddenError('Only the owner or an admin can delete a device');
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

  // Called by nebula_relay itself (X-Relay-Secret, no user session) — see
  // POST /internal/heartbeat. Keyed by relayDeviceId because that's the only
  // identifier the relay ever has (it never sees our internal device UUID).
  // Returns null rather than throwing so the caller can log-and-continue for
  // an unknown device instead of treating it as a hard failure.
  async heartbeatByRelayDeviceId(relayDeviceId: string): Promise<DeviceRecord | null> {
    const device = await this.store.findDeviceByRelayDeviceId(relayDeviceId);
    if (!device) return null;
    return this.store.touchDeviceHeartbeat(device.id, new Date());
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
