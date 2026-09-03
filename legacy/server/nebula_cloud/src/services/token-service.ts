import crypto from 'node:crypto';

import jwt from 'jsonwebtoken';

import { AppConfig } from '../config';
import { UnauthorizedError } from '../domain/errors';
import { AuthenticatedUser, UserRole } from '../domain/types';

interface AccessTokenClaims {
  sub: string;
  email: string;
  displayName: string;
  role: UserRole;
  type: 'access';
  iat?: number;
  exp?: number;
}

export interface SessionTokenClaims {
  sub: string;
  deviceId: string;
  type: 'session';
  jti: string;
  iat?: number;
  exp?: number;
}

export class TokenService {
  constructor(private readonly config: Pick<AppConfig, 'JWT_ACCESS_SECRET' | 'JWT_SESSION_SECRET' | 'ACCESS_TOKEN_TTL_SECONDS' | 'SESSION_TOKEN_TTL_SECONDS'>) {}

  issueAccessToken(user: AuthenticatedUser): string {
    const payload: AccessTokenClaims = {
      sub: user.userId,
      email: user.email,
      displayName: user.displayName,
      role: user.role,
      type: 'access',
    };
    return jwt.sign(payload, this.config.JWT_ACCESS_SECRET, {
      algorithm: 'HS256',
      expiresIn: this.config.ACCESS_TOKEN_TTL_SECONDS,
      jwtid: crypto.randomUUID(),
    });
  }

  verifyAccessToken(token: string): AuthenticatedUser {
    try {
      const payload = jwt.verify(token, this.config.JWT_ACCESS_SECRET, {
        algorithms: ['HS256'],
      }) as AccessTokenClaims;
      if (payload.type !== 'access') {
        throw new UnauthorizedError('Invalid access token type');
      }
      return {
        userId: payload.sub,
        email: payload.email,
        displayName: payload.displayName,
        // Older tokens issued before roles existed won't carry this claim;
        // treat them as a regular user rather than failing to parse.
        role: payload.role ?? 'USER',
      };
    } catch (error) {
      if (error instanceof UnauthorizedError) {
        throw error;
      }
      throw new UnauthorizedError('Invalid or expired access token');
    }
  }

  issueSessionToken(input: { auditId: string; userId: string; relayDeviceId: string }): string {
    const payload: SessionTokenClaims = {
      sub: input.userId,
      deviceId: input.relayDeviceId,
      type: 'session',
      jti: input.auditId,
    };

    return jwt.sign(payload, this.config.JWT_SESSION_SECRET, {
      algorithm: 'HS256',
      expiresIn: this.config.SESSION_TOKEN_TTL_SECONDS,
    });
  }

  verifySessionToken(token: string): SessionTokenClaims {
    const payload = jwt.verify(token, this.config.JWT_SESSION_SECRET, {
      algorithms: ['HS256'],
    }) as SessionTokenClaims;
    if (payload.type !== 'session') {
      throw new UnauthorizedError('Invalid session token type');
    }
    return payload;
  }

  decodeSessionToken(token: string): Partial<SessionTokenClaims> | null {
    const decoded = jwt.decode(token);
    if (!decoded || typeof decoded === 'string') {
      return null;
    }
    return decoded as Partial<SessionTokenClaims>;
  }
}
