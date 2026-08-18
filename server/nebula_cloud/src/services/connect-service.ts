import crypto from 'node:crypto';

import { AppConfig } from '../config';
import { NotFoundError } from '../domain/errors';
import { AuthenticatedUser } from '../domain/types';
import { DataStore } from '../repositories/types';
import { sha256 } from '../utils/crypto';
import { DeviceService } from './device-service';
import { SessionTokenClaims, TokenService } from './token-service';

export class ConnectService {
  constructor(
    private readonly store: DataStore,
    private readonly config: Pick<AppConfig, 'SESSION_TOKEN_TTL_SECONDS' | 'RELAY_SHARED_SECRET'>,
    private readonly tokenService: TokenService,
    private readonly deviceService: DeviceService,
  ) {}

  async issueSessionTicket(input: { actor: AuthenticatedUser; deviceId: string; sourceIp: string | null }) {
    const device = await this.store.findDeviceById(input.deviceId);
    if (!device) {
      throw new NotFoundError('Device not found');
    }

    const role = await this.store.getEffectiveRole(input.actor.userId, device.id);
    if (!role) {
      throw new NotFoundError('Device not found');
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

    const role = await this.store.getEffectiveRole(claims.sub, device.id);
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

  private async markAuditFailure(
    auditId: string,
    result: 'DENIED' | 'EXPIRED',
    reason: string,
  ): Promise<void> {
    await this.store.updateConnectionAudit(auditId, { result, reason });
  }
}
