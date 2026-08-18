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
});

export type AppConfig = z.infer<typeof envSchema>;

export function loadConfig(env: NodeJS.ProcessEnv = process.env): AppConfig {
  return envSchema.parse(env);
}
