import 'fastify';

import { AuthenticatedUser } from './domain/types';

declare module 'fastify' {
  interface FastifyRequest {
    user?: AuthenticatedUser;
  }
}
