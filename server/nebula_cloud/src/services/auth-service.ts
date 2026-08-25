import { AppConfig } from '../config';
import { ConflictError, UnauthorizedError } from '../domain/errors';
import { AuthenticatedUser, UserRecord } from '../domain/types';
import { DataStore } from '../repositories/types';
import { hashPassword, randomToken, sha256, verifyPassword } from '../utils/crypto';
import { TokenService } from './token-service';

export class AuthService {
  constructor(
    private readonly store: DataStore,
    private readonly config: Pick<
      AppConfig,
      'REFRESH_TOKEN_TTL_SECONDS' | 'ACCESS_TOKEN_TTL_SECONDS' | 'INITIAL_ADMIN_EMAILS' | 'DEFAULT_TRIAL_CREDIT_SECONDS'
    >,
    private readonly tokenService: TokenService,
  ) {}

  async register(input: { email: string; password: string; displayName: string }): Promise<UserRecord> {
    const email = input.email.trim().toLowerCase();
    const existing = await this.store.findUserByEmail(email);
    if (existing) {
      throw new ConflictError('A user with that email already exists');
    }

    const passwordHash = await hashPassword(input.password);
    // Bootstrap mechanism for the very first admin(s): there's no superuser
    // by default, so an operator lists trusted emails in INITIAL_ADMIN_EMAILS
    // and whoever registers with one of them becomes an admin immediately.
    // Further admins are promoted by an existing admin afterwards.
    const role = this.config.INITIAL_ADMIN_EMAILS.includes(email) ? 'ADMIN' : 'USER';
    return this.store.createUser({
      email,
      passwordHash,
      displayName: input.displayName.trim(),
      role,
      creditSeconds: this.config.DEFAULT_TRIAL_CREDIT_SECONDS,
    });
  }

  async login(input: { email: string; password: string }): Promise<{
    accessToken: string;
    refreshToken: string;
    expiresIn: number;
    user: AuthenticatedUser;
  }> {
    const email = input.email.trim().toLowerCase();
    const user = await this.store.findUserByEmail(email);
    if (!user) {
      throw new UnauthorizedError('Invalid email or password');
    }

    const passwordMatches = await verifyPassword(input.password, user.passwordHash);
    if (!passwordMatches) {
      throw new UnauthorizedError('Invalid email or password');
    }

    return this.issueTokenPair(user);
  }

  async refresh(refreshToken: string): Promise<{
    accessToken: string;
    refreshToken: string;
    expiresIn: number;
    user: AuthenticatedUser;
  }> {
    const now = new Date();
    const tokenHash = sha256(refreshToken);
    const tokenRecord = await this.store.findActiveRefreshToken(tokenHash, now);
    if (!tokenRecord) {
      throw new UnauthorizedError('Invalid or expired refresh token');
    }

    const user = await this.store.findUserById(tokenRecord.userId);
    if (!user) {
      throw new UnauthorizedError('Refresh token user no longer exists');
    }

    await this.store.revokeRefreshToken(tokenHash, now);
    return this.issueTokenPair(user);
  }

  async logout(refreshToken: string): Promise<void> {
    await this.store.revokeRefreshToken(sha256(refreshToken), new Date());
  }

  private async issueTokenPair(user: UserRecord): Promise<{
    accessToken: string;
    refreshToken: string;
    expiresIn: number;
    user: AuthenticatedUser;
  }> {
    const refreshToken = randomToken(32);
    const now = new Date();
    const expiresAt = new Date(now.getTime() + this.config.REFRESH_TOKEN_TTL_SECONDS * 1000);
    await this.store.createRefreshToken({
      userId: user.id,
      tokenHash: sha256(refreshToken),
      expiresAt,
    });

    const authUser: AuthenticatedUser = {
      userId: user.id,
      email: user.email,
      displayName: user.displayName,
      role: user.role,
    };

    return {
      accessToken: this.tokenService.issueAccessToken(authUser),
      refreshToken,
      expiresIn: this.config.ACCESS_TOKEN_TTL_SECONDS,
      user: authUser,
    };
  }
}
