import path from 'node:path';

import Fastify, { FastifyInstance, FastifyReply, FastifyRequest } from 'fastify';
import fastifyStatic from '@fastify/static';
import fastifyWebsocket from '@fastify/websocket';
import { PrismaClient } from '@prisma/client';
import { ZodError, z } from 'zod';

import { AppConfig } from './config';
import { AppError, UnauthorizedError } from './domain/errors';
import { AuthenticatedUser, GrantRole } from './domain/types';
import { PrismaDataStore } from './repositories/prisma-store';
import { DataStore } from './repositories/types';
import { AuthService } from './services/auth-service';
import { ConnectService } from './services/connect-service';
import { DeviceService } from './services/device-service';
import { GrantService } from './services/grant-service';
import { TokenService } from './services/token-service';
import { registerSignaling } from './signaling';

const registerSchema = z.object({
  email: z.email(),
  password: z.string().min(8).max(128),
  displayName: z.string().trim().min(1).max(100),
});

const loginSchema = z.object({
  email: z.email(),
  password: z.string().min(8).max(128),
});

const refreshSchema = z.object({
  refreshToken: z.string().min(16),
});

const createDeviceSchema = z.object({
  name: z.string().trim().min(1).max(120),
});

const redeemClaimCodeSchema = z.object({
  claimCode: z.string().trim().min(5).max(64),
});

const createGrantSchema = z.object({
  granteeEmail: z.email(),
  role: z.enum(['VIEWER', 'CONTROLLER'] satisfies [GrantRole, GrantRole]),
});

const internalAuthorizeSchema = z.object({
  deviceId: z.string().trim().min(1),
  token: z.string().trim().min(1),
});

function parseBearerToken(request: FastifyRequest): string {
  const header = request.headers.authorization;
  if (!header) {
    throw new UnauthorizedError('Missing Authorization header');
  }
  const [scheme, token] = header.split(' ');
  if (scheme !== 'Bearer' || !token) {
    throw new UnauthorizedError('Authorization header must be Bearer <token>');
  }
  return token;
}

function buildVdaCommand(relayHost: string, relayPort: number, relayDeviceId: string, relayToken: string): string {
  return `nebula_vda --port 7000 --relay ${relayHost} --relay-port ${relayPort} --device ${relayDeviceId} --token ${relayToken}`;
}

function buildSessionCommand(relayHost: string, relayPort: number, relayDeviceId: string, sessionToken: string): string {
  return `NEBULA_PSK=<shared-psk> nebula_session --relay ${relayHost} --relay-port ${relayPort} --device ${relayDeviceId} --token ${sessionToken}`;
}

export interface BuildAppOptions {
  config: AppConfig;
  dataStore?: DataStore;
  prismaClient?: PrismaClient;
  logger?: boolean;
}

export async function buildApp(options: BuildAppOptions): Promise<FastifyInstance> {
  const app = Fastify({ logger: options.logger ?? false });
  const projectRoot = path.resolve(__dirname, '..');
  const prisma = options.prismaClient ?? (!options.dataStore ? new PrismaClient() : undefined);
  const store = options.dataStore ?? new PrismaDataStore(prisma!);
  const tokenService = new TokenService(options.config);
  const authService = new AuthService(store, options.config, tokenService);
  const deviceService = new DeviceService(store, options.config);
  const grantService = new GrantService(store);
  const connectService = new ConnectService(store, options.config, tokenService, deviceService);

  if (prisma) {
    app.addHook('onClose', async () => {
      await prisma.$disconnect();
    });
  }

  await app.register(fastifyStatic, {
    root: path.join(projectRoot, 'public'),
    prefix: '/assets/',
    decorateReply: true,
    serve: false,
  });

  await app.register(fastifyWebsocket);
  registerSignaling(app, { config: options.config, deviceService, connectService });

  app.get('/', async (_request, reply) => reply.sendFile('index.html'));
  app.get('/dashboard', async (_request, reply) => reply.sendFile('dashboard.html'));
  app.get('/watch', async (_request, reply) => reply.sendFile('watch.html'));
  app.get('/assets/app.js', async (_request, reply) => reply.sendFile('app.js'));
  app.get('/assets/watch.js', async (_request, reply) => reply.sendFile('watch.js'));
  app.get('/assets/styles.css', async (_request, reply) => reply.sendFile('styles.css'));
  app.get('/assets/watch.css', async (_request, reply) => reply.sendFile('watch.css'));

  async function requireUser(request: FastifyRequest): Promise<AuthenticatedUser> {
    const user = tokenService.verifyAccessToken(parseBearerToken(request));
    request.user = user;
    return user;
  }

  app.setErrorHandler((error, request, reply) => {
    if (error instanceof ZodError) {
      reply.code(400).send({
        error: 'validation_error',
        message: 'Invalid request payload',
        details: z.treeifyError(error),
      });
      return;
    }
    if (error instanceof AppError) {
      reply.code(error.statusCode).send({ error: error.code, message: error.message });
      return;
    }
    console.error(error);
    request.log.error({ err: error }, 'unhandled request error');
    reply.code(500).send({
      error: 'internal_server_error',
      message: error instanceof Error ? error.message : 'Unexpected server error',
    });
  });

  app.get('/health', async () => ({ ok: true }));

  app.post('/auth/register', async (request, reply) => {
    const body = registerSchema.parse(request.body);
    const user = await authService.register(body);
    reply.code(201).send({
      user: {
        id: user.id,
        email: user.email,
        displayName: user.displayName,
        createdAt: user.createdAt.toISOString(),
      },
    });
  });

  app.post('/auth/login', async (request) => {
    const body = loginSchema.parse(request.body);
    const result = await authService.login(body);
    return {
      accessToken: result.accessToken,
      refreshToken: result.refreshToken,
      tokenType: 'Bearer',
      expiresIn: result.expiresIn,
      user: result.user,
    };
  });

  app.post('/auth/refresh', async (request) => {
    const body = refreshSchema.parse(request.body);
    const result = await authService.refresh(body.refreshToken);
    return {
      accessToken: result.accessToken,
      refreshToken: result.refreshToken,
      tokenType: 'Bearer',
      expiresIn: result.expiresIn,
      user: result.user,
    };
  });

  app.post('/auth/logout', async (request) => {
    const body = refreshSchema.parse(request.body);
    await authService.logout(body.refreshToken);
    return { success: true };
  });

  app.get('/auth/me', async (request) => {
    const user = await requireUser(request);
    return { user };
  });

  app.post('/devices', async (request, reply) => {
    const user = await requireUser(request);
    const body = createDeviceSchema.parse(request.body);
    const result = await deviceService.createDevice(user, body.name);
    const connection = deviceService.getRelayConnectionInfo(result.device);
    reply.code(201).send({
      device: {
        id: result.device.id,
        name: result.device.name,
        relayDeviceId: result.device.relayDeviceId,
        createdAt: result.device.createdAt.toISOString(),
        claimCodeExpiresAt: result.claimCodeExpiresAt.toISOString(),
      },
      registration: {
        ...connection,
        relayToken: result.relayToken,
        claimCode: result.claimCode,
        claimCodeExpiresAt: result.claimCodeExpiresAt.toISOString(),
        vdaCommand: buildVdaCommand(connection.relayHost, connection.relayPort, connection.relayDeviceId, result.relayToken),
      },
    });
  });

  app.post('/device-claims/redeem', async (request) => {
    const body = redeemClaimCodeSchema.parse(request.body);
    const result = await deviceService.redeemClaimCode(body.claimCode);
    const connection = deviceService.getRelayConnectionInfo(result.device);
    return {
      device: {
        id: result.device.id,
        name: result.device.name,
        relayDeviceId: result.device.relayDeviceId,
      },
      registration: {
        ...connection,
        relayToken: result.relayToken,
        vdaCommand: buildVdaCommand(connection.relayHost, connection.relayPort, connection.relayDeviceId, result.relayToken),
      },
    };
  });

  app.get('/devices', async (request) => {
    const user = await requireUser(request);
    return { devices: await deviceService.listAccessibleDevices(user) };
  });

  app.delete('/devices/:id', async (request) => {
    const user = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    await deviceService.deleteDevice(user, params.id);
    return { success: true };
  });

  app.post('/devices/:id/heartbeat', async (request) => {
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const relaySecret = request.headers['x-relay-secret'];
    const actorUserId = relaySecret === options.config.RELAY_SHARED_SECRET ? undefined : (await requireUser(request)).userId;
    const device = await deviceService.heartbeat(params.id, actorUserId);
    return {
      deviceId: device.id,
      lastSeenAt: device.lastSeenAt?.toISOString() ?? null,
    };
  });

  app.post('/devices/:id/grants', async (request, reply) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const body = createGrantSchema.parse(request.body);
    const grant = await grantService.createOrUpdateGrant({
      actor,
      deviceId: params.id,
      granteeEmail: body.granteeEmail,
      role: body.role,
    });
    reply.code(201).send({
      grant: {
        id: grant.id,
        deviceId: grant.deviceId,
        role: grant.role,
        createdAt: grant.createdAt.toISOString(),
        revokedAt: grant.revokedAt?.toISOString() ?? null,
        granteeUser: {
          id: grant.granteeUser.id,
          email: grant.granteeUser.email,
          displayName: grant.granteeUser.displayName,
        },
      },
    });
  });

  app.get('/devices/:id/grants', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const grants = await grantService.listGrants(actor, params.id);
    return {
      grants: grants.map((grant) => ({
        id: grant.id,
        deviceId: grant.deviceId,
        role: grant.role,
        createdAt: grant.createdAt.toISOString(),
        revokedAt: grant.revokedAt?.toISOString() ?? null,
        granteeUser: {
          id: grant.granteeUser.id,
          email: grant.granteeUser.email,
          displayName: grant.granteeUser.displayName,
        },
      })),
    };
  });

  app.delete('/grants/:id', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const grant = await grantService.revokeGrant(actor, params.id);
    return {
      success: true,
      grant: grant
        ? {
            id: grant.id,
            revokedAt: grant.revokedAt?.toISOString() ?? null,
          }
        : null,
    };
  });

  app.post('/devices/:id/connect', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const result = await connectService.issueSessionTicket({
      actor,
      deviceId: params.id,
      sourceIp: request.ip,
    });
    return {
      relayHost: result.relayHost,
      relayPort: result.relayPort,
      relayDeviceId: result.relayDeviceId,
      sessionToken: result.sessionToken,
      expiresAt: result.expiresAt.toISOString(),
      role: result.role,
      sessionCommand: buildSessionCommand(result.relayHost, result.relayPort, result.relayDeviceId, result.sessionToken),
    };
  });

  app.post('/internal/authorize', async (request, reply) => {
    const body = internalAuthorizeSchema.parse(request.body);
    const relaySecretHeader = request.headers['x-relay-secret'];
    const relaySecret = Array.isArray(relaySecretHeader) ? relaySecretHeader[0] : relaySecretHeader;
    const result = await connectService.authorizeRelay({
      relaySecret,
      relayDeviceId: body.deviceId,
      sessionToken: body.token,
      sourceIp: request.ip,
    });
    reply.code(result.statusCode).send(result.body);
  });

  return app;
}
