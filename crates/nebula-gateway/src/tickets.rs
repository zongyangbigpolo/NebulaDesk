//! Offline ticket verification.
//!
//! The gateway is on the hot path of every connection, so it must never need
//! the manager to admit a client. It caches the manager's published key set
//! and checks signatures locally; the manager can be down for the length of a
//! deployment without stopping sessions from starting.
//!
//! What that costs is revocation latency, which is why a ticket lives about a
//! minute and authorises *starting* one session, never continuing one.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use nebula_common::ticket::{TicketClaims, AUDIENCE};
use tokio::sync::{Mutex, RwLock};

use crate::manager::ManagerClient;

/// Why a ticket was refused.
///
/// Callers turn every one of these into the same opaque rejection: telling a
/// client whether its ticket was expired, forged or replayed is telling an
/// attacker which knob to turn.
#[derive(Debug, thiserror::Error)]
pub enum TicketError {
    /// The token was not a well-formed JWT, or its signature did not verify.
    #[error("the ticket is not valid")]
    Invalid,

    /// The signing key is not one the manager publishes.
    #[error("the ticket was signed by an unknown key")]
    UnknownKey,

    /// This ticket has already been used.
    #[error("the ticket has already been redeemed")]
    Replayed,
}

/// How long to wait before hitting the manager for the key set again after a
/// miss. Without this, a flood of tickets bearing made-up key ids would turn
/// every one of them into a manager request.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Refresh the cached key set this often even when nothing has missed, so a
/// rotated key is picked up before any ticket signed with it arrives.
const BACKGROUND_REFRESH: Duration = Duration::from_secs(300);

struct Keys {
    by_kid: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

/// Verifies session tickets against the manager's key set.
pub struct TicketVerifier {
    manager: ManagerClient,
    issuer: String,
    keys: RwLock<Keys>,
    refreshing: Mutex<()>,
    seen: Mutex<HashMap<String, i64>>,
}

impl TicketVerifier {
    /// Build a verifier that fetches keys from `manager` and requires tickets
    /// to name `issuer`.
    #[must_use]
    pub fn new(manager: ManagerClient, issuer: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            manager,
            issuer: issuer.into(),
            keys: RwLock::new(Keys {
                by_kid: HashMap::new(),
                fetched_at: None,
            }),
            refreshing: Mutex::new(()),
            seen: Mutex::new(HashMap::new()),
        })
    }

    /// Populate the cache, failing if the manager cannot be reached.
    ///
    /// Called at startup so a misconfigured gateway fails loudly rather than
    /// refusing every client once traffic arrives.
    pub async fn prime(&self) -> anyhow::Result<()> {
        self.refresh().await
    }

    /// Keep the cache warm for as long as the gateway runs.
    pub async fn refresh_forever(self: Arc<Self>) {
        loop {
            tokio::time::sleep(BACKGROUND_REFRESH).await;
            if let Err(error) = self.refresh().await {
                tracing::warn!(%error, "could not refresh the manager key set");
            }
        }
    }

    async fn refresh(&self) -> anyhow::Result<()> {
        let jwks = self.manager.jwks().await?;
        let mut by_kid = HashMap::new();
        for jwk in jwks.keys {
            if jwk.kty != "OKP" || jwk.crv != "Ed25519" {
                continue;
            }
            let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&jwk.x) else {
                continue;
            };
            by_kid.insert(jwk.kid.clone(), DecodingKey::from_ed_der(&raw));
        }
        if by_kid.is_empty() {
            anyhow::bail!("the manager published no usable Ed25519 keys");
        }
        let mut keys = self.keys.write().await;
        keys.by_kid = by_kid;
        keys.fetched_at = Some(Instant::now());
        Ok(())
    }

    /// Refresh at most once per [`MIN_REFRESH_INTERVAL`], collapsing
    /// concurrent misses onto a single request.
    async fn refresh_if_stale(&self) {
        let _guard = self.refreshing.lock().await;
        let recent = self
            .keys
            .read()
            .await
            .fetched_at
            .is_some_and(|at| at.elapsed() < MIN_REFRESH_INTERVAL);
        if recent {
            return;
        }
        if let Err(error) = self.refresh().await {
            tracing::warn!(%error, "could not refresh the manager key set");
        }
    }

    /// Verify a ticket and consume it.
    ///
    /// A ticket is single-use: the gateway remembers every `jti` until it
    /// expires, so a ticket captured in flight buys an attacker nothing once
    /// the legitimate client has redeemed it.
    pub async fn redeem(&self, token: &str) -> Result<TicketClaims, TicketError> {
        let claims = self.verify(token).await?;
        self.consume(&claims).await?;
        Ok(claims)
    }

    /// Verify before APP capability checks, without spending an unusable ticket.
    /// The caller must consume the verified claims before forwarding a session.
    pub(crate) async fn verify(&self, token: &str) -> Result<TicketClaims, TicketError> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| TicketError::Invalid)?;
        let kid = header.kid.ok_or(TicketError::Invalid)?;

        let claims = match self.decode(token, &kid).await {
            Ok(claims) => claims,
            Err(TicketError::UnknownKey) => {
                // A key the manager rotated to since the last fetch looks
                // exactly like a forged one, so one refresh and one retry is
                // the difference between a rotation being seamless and it
                // being an outage.
                self.refresh_if_stale().await;
                self.decode(token, &kid).await?
            }
            Err(other) => return Err(other),
        };

        claims
            .launch_target
            .validate(claims.rid, claims.policy)
            .map_err(|_| TicketError::Invalid)?;
        Ok(claims)
    }

    async fn decode(&self, token: &str, kid: &str) -> Result<TicketClaims, TicketError> {
        let keys = self.keys.read().await;
        let key = keys.by_kid.get(kid).ok_or(TicketError::UnknownKey)?;

        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[AUDIENCE]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        // The manager and the gateway are separate machines; a ticket that
        // lives sixty seconds cannot also demand perfectly aligned clocks.
        validation.leeway = nebula_common::ticket::CLOCK_SKEW
            .whole_seconds()
            .unsigned_abs();

        jsonwebtoken::decode::<TicketClaims>(token, key, &validation)
            .map(|data| data.claims)
            .map_err(|_| TicketError::Invalid)
    }

    /// Record a ticket as spent, rejecting it if it already was.
    pub(crate) async fn consume(&self, claims: &TicketClaims) -> Result<(), TicketError> {
        let now = time::OffsetDateTime::now_utc();
        if !claims.is_valid_at(now) {
            return Err(TicketError::Invalid);
        }
        let now = now.unix_timestamp();
        let mut seen = self.seen.lock().await;
        // Expired entries can never cause a false rejection, so dropping them
        // here keeps the table proportional to the ticket rate rather than to
        // uptime, without needing a sweeper task.
        seen.retain(|_, exp| *exp > now);
        let acceptance_end = claims
            .exp
            .saturating_add(nebula_common::ticket::CLOCK_SKEW.whole_seconds());
        if seen.insert(claims.jti.clone(), acceptance_end).is_some() {
            return Err(TicketError::Replayed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: i64) -> TicketClaims {
        serde_json::from_value(serde_json::json!({
            "iss":"issuer","aud":"nebula-gateway","jti":"single-use",
            "iat":exp-60,"exp":exp,
            "sid":nebula_common::SessionId::new(),"tid":nebula_common::TenantId::new(),
            "uid":nebula_common::UserId::new(),"mid":nebula_common::MachineId::new(),
            "rid":nebula_common::ResourceId::new(),"role":"VIEWER",
            "policy":nebula_common::SessionPolicy::view_only(),
            "agent_key":"agent","relay_addr":"relay","relay_pin":"pin"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn consumption_is_atomic_and_replay_cache_covers_clock_skew() {
        let verifier = TicketVerifier::new(
            ManagerClient::new("http://unused.test", None).unwrap(),
            "issuer",
        );
        let claims = claims(time::OffsetDateTime::now_utc().unix_timestamp() - 1);
        let (first, second) = tokio::join!(verifier.consume(&claims), verifier.consume(&claims));
        assert!(matches!(
            (first, second),
            (Ok(()), Err(TicketError::Replayed)) | (Err(TicketError::Replayed), Ok(()))
        ));
    }

    #[tokio::test]
    async fn claims_expired_while_waiting_for_admission_are_not_consumed() {
        let verifier = TicketVerifier::new(
            ManagerClient::new("http://unused.test", None).unwrap(),
            "issuer",
        );
        let claims = claims(time::OffsetDateTime::now_utc().unix_timestamp() - 31);
        assert!(matches!(
            verifier.consume(&claims).await,
            Err(TicketError::Invalid)
        ));
        assert!(verifier.seen.lock().await.is_empty());
    }
}
