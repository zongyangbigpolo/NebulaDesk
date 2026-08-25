import { z } from 'zod';

const envSchema = z.object({
  PORT: z.coerce.number().int().positive().default(4000),
  HOST: z.string().default('0.0.0.0'),
  DATABASE_URL: z.string().min(1),
  JWT_ACCESS_SECRET: z.string().min(16),
  JWT_REFRESH_SECRET: z.string().min(16),
  JWT_SESSION_SECRET: z.string().min(16),
  ACCESS_TOKEN_TTL_SECONDS: z.coerce.number().int().positive().default(900),
  REFRESH_TOKEN_TTL_SECONDS: z.coerce.number().int().positive().default(60 * 60 * 24 * 30),
  SESSION_TOKEN_TTL_SECONDS: z.coerce.number().int().positive().default(60),
  CLAIM_CODE_TTL_SECONDS: z.coerce.number().int().positive().default(3600),
  DEVICE_ONLINE_WINDOW_SECONDS: z.coerce.number().int().positive().default(120),
  RELAY_PUBLIC_HOST: z.string().min(1),
  RELAY_PUBLIC_PORT: z.coerce.number().int().positive().default(7100),
  RELAY_SHARED_SECRET: z.string().min(16),
  // Comma-separated emails that get role=ADMIN automatically on registration —
  // the bootstrap mechanism for the very first admin(s), since there is no
  // superuser account by default and no other user can promote one (see
  // AuthService.register). Promoting further admins after bootstrap is done
  // by an existing admin via PATCH /admin/users/:id/role.
  INITIAL_ADMIN_EMAILS: z
    .string()
    .default('')
    .transform((value) =>
      value
        .split(',')
        .map((email) => email.trim().toLowerCase())
        .filter((email) => email.length > 0),
    ),
  // Self-service VDA enrollment (see "Self-registration & trial credit" in
  // README.md): a single shared secret, distributed out-of-band by whoever
  // runs this nebula_cloud instance, that any LOGGED-IN user can present to
  // POST /devices/self-register to register a device under their own
  // account — no admin has to pre-create the device row first. Empty (the
  // default) disables the endpoint entirely: self-registration is opt-in.
  DEVICE_ENROLLMENT_TOKEN: z.string().default(''),
  // New accounts start with this many seconds of "connect" credit (see
  // User.creditSeconds); each successful POST /devices/:id/connect deducts
  // CONNECT_CREDIT_COST_SECONDS from it (flat cost per connect, not metered
  // by actual session duration — see README.md's "Trial credit" section for
  // why). Admins are never metered.
  DEFAULT_TRIAL_CREDIT_SECONDS: z.coerce.number().int().nonnegative().default(600),
  CONNECT_CREDIT_COST_SECONDS: z.coerce.number().int().positive().default(600),
});

export type AppConfig = z.infer<typeof envSchema>;

export function loadConfig(env: NodeJS.ProcessEnv = process.env): AppConfig {
  return envSchema.parse(env);
}
