//! Shared application state.

use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use crate::auth::tokens::AccessTokens;
use crate::config::Config;
use crate::signing::TicketSigner;

/// Everything a handler needs, cloned per request.
#[derive(Clone)]
pub struct AppState {
    /// Connection pool.
    pub db: PgPool,
    /// Immutable configuration.
    pub config: Arc<Config>,
    /// Access token issuer and verifier.
    pub access_tokens: AccessTokens,
    /// Session ticket signer.
    pub signer: Arc<TicketSigner>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("signer", &self.signer)
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Connect, migrate and assemble the state.
    pub async fn bootstrap(config: Config) -> anyhow::Result<Self> {
        let db = connect(&config.database_url).await?;
        migrate(&db).await?;
        let signer = TicketSigner::load_or_create(&db, &config.public_url).await?;
        Ok(Self {
            access_tokens: AccessTokens::new(&config.access_token_secret, config.access_token_ttl),
            db,
            signer: Arc::new(signer),
            config: Arc::new(config),
        })
    }
}

/// Open a pool sized for an interactive control plane.
pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    Ok(PgPoolOptions::new()
        // Argon2 verification holds a connection only briefly, but login
        // bursts are the worst case; a small pool with a short acquire
        // timeout fails fast instead of queueing requests invisibly.
        .max_connections(16)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await?)
}

/// Apply schema migrations.
pub async fn migrate(db: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(db).await?;
    Ok(())
}
