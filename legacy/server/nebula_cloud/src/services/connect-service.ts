import crypto from 'node:crypto';

import { AppConfig } from '../config';
import { InsufficientCreditError, NotFoundError } from '../domain/errors';
import { AuthenticatedUser } from '../domain/types';
import { DataStore } from '../repositories/types';
import { sha256 } from '../utils/crypto';
import { DeviceService } from './device-service';
import { SessionTokenClaims, TokenService } from './token-service';

export class ConnectService {
  constructor(
    private readonly store: DataStore,
    private readonly config: Pick<AppConfig, 'SESSION_TOKEN_TTL_SECONDS' | 'RELAY_SHARED_SECRET' | 'CONNECT_CREDIT_COST_SECONDS'>,
    private readonly tokenService: TokenService,
    private readonly deviceService: DeviceService,
  ) {}

  async issueSessionTicket(input: { actor: AuthenticatedUser; deviceId: string; sourceIp: string | null }) {
    const device = await this.store.findDeviceById(input.deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }

    // Admins can connect to any device without needing an explicit
    // owner/grant/group relationship — see DeviceService.listAccessibleDevices
    // for the read-side equivalent.
    const role = input.actor.role === 'ADMIN' ? 'ADMIN' : await this.store.getEffectiveRole(input.actor.userId, device.id);
    if (!role) {
      throw new NotFoundError('Device not found');
    }

    // Trial credit (see README.md's "Trial credit" section): a flat cost is
    // deducted from the caller's balance per successful connect, regardless
    // of how long the resulting session actually lasts (metering real
    // session duration would need the relay/VDA to report it back, which
    // doesn't exist today — see ROADMAP.md). Admins are never metered; the
    // spend is atomic at the DB layer so concurrent connects can't both pass
    // a stale balance check (see PrismaDataStore.spendUserCredit).
    if (input.actor.role !== 'ADMIN') {
      const spent = await this.store.spendUserCredit(input.actor.userId, this.config.CONNECT_CREDIT_COST_SECONDS);
      if (!spent) {
        throw new InsufficientCreditError('Not enough connect credit remaining — ask an admin to top up your account');
      }
    }

    const auditId = crypto.randomUUID();
    const sessionToken = this.tokenService.issueSessionToken({
      auditId,
      userId: input.actor.userId,
      relayDeviceId: device.relayDeviceId,
    });
    const expiresAt = new Date(Date.now() + this.config.SESSION_TOKEN_TTL_SECONDS * 1000);

    await this.store.createConnectionAudit({
      id: auditId,
      deviceId: device.id,
      requestedByUserId: input.actor.userId,
      issuedTokenHash: sha256(sessionToken),
      issuedAt: new Date(),
      expiresAt,
      sourceIp: input.sourceIp,
      result: 'ISSUED',
    });

    return {
      ...this.deviceService.getRelayConnectionInfo(device),
      sessionToken,
      // Handed back only to a caller who already passed the role check above
      // (owner/grant/group-member/admin) — the same trust boundary that
      // gates the connection itself, so exposing the PSK here doesn't widen
      // who can decrypt the session beyond who can already open one.
      psk: device.psk,
      expiresAt,
      role,
    };
  }

  async authorizeRelay(input: {
    relaySecret: string | undefined;
    relayDeviceId: string;
    sessionToken: string;
    sourceIp: string | null;
  }): Promise<{ statusCode: number; body: { authorized: boolean; reason?: string } }> {
    if (!input.relaySecret || input.relaySecret !== this.config.RELAY_SHARED_SECRET) {
      return { statusCode: 401, body: { authorized: false, reason: 'relay secret mismatch' } };
    }

    const decoded = this.tokenService.decodeSessionToken(input.sessionToken);
    const decodedJti = decoded?.jti;

    try {
      const claims = this.tokenService.verifySessionToken(input.sessionToken);
      return this.authorizeVerifiedSessionToken(claims, input.relayDeviceId);
    } catch (error) {
      if (decodedJti) {
        const audit = await this.store.findConnectionAuditById(decodedJti);
        if (audit) {
          const result = error instanceof Error && error.name === 'TokenExpiredError' ? 'EXPIRED' : 'DENIED';
          await this.store.updateConnectionAudit(audit.id, {
            result,
            reason: error instanceof Error ? error.message : 'Invalid session token',
          });
        }
      }
      return {
        statusCode: 200,
        body: {
          authorized: false,
          reason: error instanceof Error ? error.message : 'Invalid session token',
        },
      };
    }
  }

  private async authorizeVerifiedSessionToken(
    claims: SessionTokenClaims,
    relayDeviceId: string,
  ): Promise<{ statusCode: number; body: { authorized: boolean; reason?: string } }> {
    if (claims.deviceId !== relayDeviceId) {
      await this.markAuditFailure(claims.jti, 'DENIED', 'Session token device mismatch');
      return { statusCode: 200, body: { authorized: false, reason: 'session token device mismatch' } };
    }

    const device = await this.store.findDeviceByRelayDeviceId(relayDeviceId);
    if (!device) {
      await this.markAuditFailure(claims.jti, 'DENIED', 'Device not found');
      return { statusCode: 200, body: { authorized: false, reason: 'device not found' } };
    }

    const audit = await this.store.findConnectionAuditById(claims.jti);
    if (!audit || audit.deviceId !== device.id || audit.requestedByUserId !== claims.sub) {
      return { statusCode: 200, body: { authorized: false, reason: 'session audit not found' } };
    }

    if (audit.expiresAt <= new Date()) {
      await this.markAuditFailure(audit.id, 'EXPIRED', 'Session ticket expired');
      return { statusCode: 200, body: { authorized: false, reason: 'session ticket expired' } };
    }

    if (audit.usedAt) {
      await this.markAuditFailure(audit.id, 'DENIED', 'Session ticket already used');
      return { statusCode: 200, body: { authorized: false, reason: 'session ticket already used' } };
    }

    const role = await this.getEffectiveRoleLive(claims.sub, device.id);
    if (!role) {
      await this.markAuditFailure(audit.id, 'DENIED', 'User is no longer authorized for this device');
      return { statusCode: 200, body: { authorized: false, reason: 'user is no longer authorized for this device' } };
    }

    await this.store.updateConnectionAudit(audit.id, {
      usedAt: new Date(),
      result: 'AUTHORIZED',
      reason: null,
    });

    return { statusCode: 200, body: { authorized: true } };
  }

  // Re-derives the requester's role from live DB state at authorize time
  // (not from anything cached in the session JWT), so a revoked grant/group
  // membership or an admin demotion takes effect immediately — the same
  // real-time-authorization property §7 of ARCHITECTURE_OVERVIEW.md documents
  // for the rest of this endpoint.
  private async getEffectiveRoleLive(userId: string, deviceId: string) {
    const user = await this.store.findUserById(userId);
    if (user?.role === 'ADMIN') {
      return 'ADMIN' as const;
    }
    return this.store.getEffectiveRole(userId, deviceId);
  }

  private async markAuditFailure(
    auditId: string,
    result: 'DENIED' | 'EXPIRED',
    reason: string,
  ): Promise<void> {
    await this.store.updateConnectionAudit(auditId, { result, reason });
  }
}
