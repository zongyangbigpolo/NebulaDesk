//! Manager configuration.

use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Runtime configuration, assembled from environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Address the HTTP API listens on.
    pub listen: SocketAddr,
    /// Postgres connection string.
    pub database_url: String,
    /// Public base URL, used as the ticket issuer.
    pub public_url: String,
    /// Secret used to sign access tokens (HS256).
    ///
    /// Access tokens are only ever verified by the manager itself, so a shared
    /// symmetric secret is both sufficient and cheaper than asymmetric
    /// signatures. Session tickets are different: they are verified by
    /// gateways and agents, and use Ed25519.
    pub access_token_secret: String,
    /// Lifetime of an access token.
    pub access_token_ttl: Duration,
    /// Lifetime of a refresh token.
    pub refresh_token_ttl: Duration,
    /// Bearer token that authorises tenant creation and node registration.
    ///
    /// These are operator actions with no tenant to authenticate against, so
    /// they are gated by a deployment-wide secret rather than a user session.
    pub bootstrap_token: String,
    /// Maximum accepted request body.
    pub max_body_bytes: usize,
}

/// The environment variable that must be set before the manager will start.
const REQUIRED: [&str; 3] = [
    "NEBULA_DATABASE_URL",
    "NEBULA_ACCESS_TOKEN_SECRET",
    "NEBULA_BOOTSTRAP_TOKEN",
];

impl Config {
    /// Read configuration from the environment.
    ///
    /// Deliberately fails rather than inventing defaults for secrets: a
    /// manager that silently starts with a well-known signing key is worse
    /// than one that refuses to start.
    pub fn from_env() -> anyhow::Result<Self> {
        for key in REQUIRED {
            if std::env::var(key).unwrap_or_default().is_empty() {
                anyhow::bail!("{key} must be set");
            }
        }
        let secret = std::env::var("NEBULA_ACCESS_TOKEN_SECRET")?;
        anyhow::ensure!(
            secret.len() >= 32,
            "NEBULA_ACCESS_TOKEN_SECRET must be at least 32 characters"
        );
        Ok(Self {
            listen: std::env::var("NEBULA_LISTEN")
                .unwrap_or_else(|_| "0.0.0.0:8080".into())
                .parse()?,
            database_url: std::env::var("NEBULA_DATABASE_URL")?,
            public_url: std::env::var("NEBULA_PUBLIC_URL")
                .unwrap_or_else(|_| "http://localhost:8080".into()),
            access_token_secret: secret,
            access_token_ttl: Duration::from_secs(env_secs("NEBULA_ACCESS_TOKEN_TTL", 900)?),
            refresh_token_ttl: Duration::from_secs(env_secs(
                "NEBULA_REFRESH_TOKEN_TTL",
                30 * 24 * 3600,
            )?),
            bootstrap_token: std::env::var("NEBULA_BOOTSTRAP_TOKEN")?,
            max_body_bytes: 2 * 1024 * 1024,
        })
    }

    /// A configuration suitable for tests, pointed at `database_url`.
    #[must_use]
    pub fn for_test(database_url: String) -> Self {
        Self {
            listen: "127.0.0.1:0".parse().expect("literal address"),
            database_url,
            public_url: "http://manager.test".into(),
            access_token_secret: "test-secret-that-is-long-enough-0123456789".into(),
            access_token_ttl: Duration::from_secs(900),
            refresh_token_ttl: Duration::from_secs(3600),
            bootstrap_token: "test-bootstrap-token".into(),
            max_body_bytes: 2 * 1024 * 1024,
        }
    }
}

fn env_secs(key: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v.parse()?),
        _ => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_has_a_long_enough_secret() {
        // Mirrors the production check, so the fixture cannot drift into
        // being something production would reject.
        assert!(
            Config::for_test("postgres://x".into())
                .access_token_secret
                .len()
                >= 32
        );
    }

    #[test]
    fn defaults_are_sane() {
        let c = Config::for_test("postgres://x".into());
        assert!(c.access_token_ttl < c.refresh_token_ttl);
        assert!(c.max_body_bytes > 0);
    }
}
