import path from 'node:path';

import Fastify, { FastifyInstance, FastifyReply, FastifyRequest } from 'fastify';
import fastifyStatic from '@fastify/static';
import fastifyWebsocket from '@fastify/websocket';
import { PrismaClient } from '@prisma/client';
import { ZodError, z } from 'zod';

import { AppConfig } from './config';
import { AppError, UnauthorizedError } from './domain/errors';
import { AuthenticatedUser, GrantRole, UserRole } from './domain/types';
import { PrismaDataStore } from './repositories/prisma-store';
import { DataStore } from './repositories/types';
import { AuthService } from './services/auth-service';
import { ConnectService } from './services/connect-service';
import { DeviceService } from './services/device-service';
import { GrantService } from './services/grant-service';
import { GroupService } from './services/group-service';
import { TokenService } from './services/token-service';
import { UserService } from './services/user-service';
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

const selfRegisterDeviceSchema = z.object({
  name: z.string().trim().min(1).max(120),
  enrollmentToken: z.string().min(1),
});

const redeemClaimCodeSchema = z.object({
  claimCode: z.string().trim().min(5).max(64),
});

const createGrantSchema = z.object({
  granteeEmail: z.email(),
  role: z.enum(['VIEWER', 'CONTROLLER'] satisfies [GrantRole, GrantRole]),
});

const createGroupSchema = z.object({
  name: z.string().trim().min(1).max(120),
});

const addGroupMemberSchema = z.object({
  email: z.email(),
  role: z.enum(['VIEWER', 'CONTROLLER'] satisfies [GrantRole, GrantRole]),
});

const assignDeviceGroupSchema = z.object({
  groupId: z.uuid().nullable(),
});

const setUserRoleSchema = z.object({
  role: z.enum(['USER', 'ADMIN'] satisfies [UserRole, UserRole]),
});

const addUserCreditSchema = z.object({
  addSeconds: z.number().int(),
});

const internalAuthorizeSchema = z.object({
  deviceId: z.string().trim().min(1),
  token: z.string().trim().min(1),
});

const internalHeartbeatSchema = z.object({
  deviceId: z.string().trim().min(1),
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

function buildVdaCommand(relayHost: string, relayPort: number, relayDeviceId: string, relayToken: string, psk: string): string {
  return `nebula_vda --port 7000 --psk ${psk} --relay ${relayHost} --relay-port ${relayPort} --device ${relayDeviceId} --token ${relayToken}`;
}

function buildSessionCommand(relayHost: string, relayPort: number, relayDeviceId: string, sessionToken: string, psk: string): string {
  return `NEBULA_PSK=${psk} nebula_session --relay ${relayHost} --relay-port ${relayPort} --device ${relayDeviceId} --token ${sessionToken}`;
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
  const groupService = new GroupService(store);
  const userService = new UserService(store);
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
        role: user.role,
        creditSeconds: user.creditSeconds,
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
    // `user` here is whatever's baked into the access token (role included,
    // see TokenService) — creditSeconds changes far more often (every
    // connect), so it's looked up fresh instead of trusting the token.
    const fresh = await store.findUserById(user.userId);
    return { user: { ...user, creditSeconds: fresh?.creditSeconds ?? null } };
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
        psk: result.psk,
        claimCode: result.claimCode,
        claimCodeExpiresAt: result.claimCodeExpiresAt.toISOString(),
        vdaCommand: buildVdaCommand(connection.relayHost, connection.relayPort, connection.relayDeviceId, result.relayToken, result.psk),
      },
    });
  });

  // Self-service registration (see README.md's "Self-registration & trial
  // credit"): any logged-in user presenting DEVICE_ENROLLMENT_TOKEN gets a
  // device created under their own account immediately — no admin/claim-code
  // step needed first. This is what the Flutter manager's "Host this Mac"
  // tab calls.
  app.post('/devices/self-register', async (request, reply) => {
    const user = await requireUser(request);
    const body = selfRegisterDeviceSchema.parse(request.body);
    const result = await deviceService.selfRegisterDevice(user, body.name, body.enrollmentToken);
    const connection = deviceService.getRelayConnectionInfo(result.device);
    reply.code(201).send({
      device: {
        id: result.device.id,
        name: result.device.name,
        relayDeviceId: result.device.relayDeviceId,
        createdAt: result.device.createdAt.toISOString(),
      },
      registration: {
        ...connection,
        relayToken: result.relayToken,
        psk: result.psk,
        vdaCommand: buildVdaCommand(connection.relayHost, connection.relayPort, connection.relayDeviceId, result.relayToken, result.psk),
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
        psk: result.device.psk,
        vdaCommand: buildVdaCommand(connection.relayHost, connection.relayPort, connection.relayDeviceId, result.relayToken, result.device.psk),
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
    // Fetch fresh balance to report back (creditSeconds was just decremented
    // by issueSessionTicket for non-admins; for admins it's whatever they
    // currently have, unaffected).
    const freshActor = await store.findUserById(actor.userId);
    return {
      relayHost: result.relayHost,
      relayPort: result.relayPort,
      relayDeviceId: result.relayDeviceId,
      sessionToken: result.sessionToken,
      psk: result.psk,
      expiresAt: result.expiresAt.toISOString(),
      role: result.role,
      creditSecondsRemaining: freshActor?.creditSeconds ?? null,
      sessionCommand: buildSessionCommand(result.relayHost, result.relayPort, result.relayDeviceId, result.sessionToken, result.psk),
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

  // Called by nebula_relay (not a browser/CLI client) to report that a VDA is
  // registered and alive, so `online`/`lastSeenAt` in GET /devices reflect
  // the QUIC-path relay session too (previously only the WebRTC signaling
  // path kept this fresh). See server/nebula_relay/DEPLOY.md for how to wire
  // `--saas-heartbeat-url`/`--saas-heartbeat-interval-secs` on the relay.
  app.post('/internal/heartbeat', async (request, reply) => {
    const body = internalHeartbeatSchema.parse(request.body);
    const relaySecretHeader = request.headers['x-relay-secret'];
    const relaySecret = Array.isArray(relaySecretHeader) ? relaySecretHeader[0] : relaySecretHeader;
    if (!relaySecret || relaySecret !== options.config.RELAY_SHARED_SECRET) {
      reply.code(401).send({ ok: false, reason: 'relay secret mismatch' });
      return;
    }
    const device = await deviceService.heartbeatByRelayDeviceId(body.deviceId);
    if (!device) {
      reply.code(404).send({ ok: false, reason: 'device not found' });
      return;
    }
    reply.code(200).send({ ok: true, lastSeenAt: device.lastSeenAt?.toISOString() ?? null });
  });

  // --- Admin: users, groups, and device-group assignment -------------------
  // See "Groups & admin role" in README.md. All of these require the caller's
  // access token to carry role=ADMIN (see UserService/GroupService).

  app.get('/admin/users', async (request) => {
    const actor = await requireUser(request);
    const users = await userService.listUsers(actor);
    return {
      users: users.map((user) => ({
        id: user.id,
        email: user.email,
        displayName: user.displayName,
        role: user.role,
        creditSeconds: user.creditSeconds,
        createdAt: user.createdAt.toISOString(),
      })),
    };
  });

  app.patch('/admin/users/:id/role', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const body = setUserRoleSchema.parse(request.body);
    const updated = await userService.setUserRole(actor, params.id, body.role);
    return {
      user: {
        id: updated.id,
        email: updated.email,
        displayName: updated.displayName,
        role: updated.role,
        createdAt: updated.createdAt.toISOString(),
      },
    };
  });

  // Admin top-up for a user's trial "connect" credit (see README.md's "Trial
  // credit" section). `addSeconds` may be negative to claw back credit.
  app.patch('/admin/users/:id/credit', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const body = addUserCreditSchema.parse(request.body);
    const updated = await userService.addCredit(actor, params.id, body.addSeconds);
    return {
      user: {
        id: updated.id,
        email: updated.email,
        creditSeconds: updated.creditSeconds,
      },
    };
  });

  app.post('/admin/groups', async (request, reply) => {
    const actor = await requireUser(request);
    const body = createGroupSchema.parse(request.body);
    const group = await groupService.createGroup(actor, body.name);
    reply.code(201).send({
      group: { id: group.id, name: group.name, createdAt: group.createdAt.toISOString() },
    });
  });

  app.get('/admin/groups', async (request) => {
    const actor = await requireUser(request);
    const groups = await groupService.listGroups(actor);
    return {
      groups: groups.map((group) => ({
        id: group.id,
        name: group.name,
        createdAt: group.createdAt.toISOString(),
        memberCount: group.memberCount,
        deviceCount: group.deviceCount,
      })),
    };
  });

  app.get('/admin/groups/:id', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const { group, members, devices } = await groupService.getGroup(actor, params.id);
    return {
      group: { id: group.id, name: group.name, createdAt: group.createdAt.toISOString() },
      members: members.map((membership) => ({
        userId: membership.userId,
        email: membership.user.email,
        displayName: membership.user.displayName,
        role: membership.role,
      })),
      devices: devices.map((device) => ({
        id: device.id,
        name: device.name,
        relayDeviceId: device.relayDeviceId,
      })),
    };
  });

  app.delete('/admin/groups/:id', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    await groupService.deleteGroup(actor, params.id);
    return { success: true };
  });

  app.post('/admin/groups/:id/members', async (request, reply) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const body = addGroupMemberSchema.parse(request.body);
    const membership = await groupService.addMember(actor, params.id, body.email, body.role);
    reply.code(201).send({
      member: { userId: membership.userId, email: membership.user.email, role: membership.role },
    });
  });

  app.delete('/admin/groups/:id/members/:userId', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid(), userId: z.uuid() }).parse(request.params);
    await groupService.removeMember(actor, params.id, params.userId);
    return { success: true };
  });

  app.post('/admin/devices/:id/group', async (request) => {
    const actor = await requireUser(request);
    const params = z.object({ id: z.uuid() }).parse(request.params);
    const body = assignDeviceGroupSchema.parse(request.body);
    const device = await groupService.assignDeviceToGroup(actor, params.id, body.groupId);
    return { deviceId: device.id, groupId: device.groupId };
  });

  return app;
}
