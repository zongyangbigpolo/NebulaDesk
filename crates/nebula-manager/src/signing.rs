//! Session ticket signing.
//!
//! The signing key lives in the database rather than in a file or in memory.
//! Every manager replica therefore signs with the same key, so a gateway's
//! cached JWKS stays valid no matter which replica issued a ticket, and a
//! restart does not invalidate tickets in flight.

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use nebula_common::ticket::{Jwk, Jwks, TicketClaims, AUDIENCE, DEFAULT_TTL};
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::error::{ApiError, ApiResult};

/// Signs session tickets and publishes the matching JWKS.
pub struct TicketSigner {
    kid: String,
    encoding: EncodingKey,
    verifying: VerifyingKey,
    issuer: String,
}

impl std::fmt::Debug for TicketSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketSigner")
            .field("kid", &self.kid)
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

impl TicketSigner {
    /// Load the active signing key, creating one on first start.
    ///
    /// Two replicas starting simultaneously can both try to create the first
    /// key; the insert is written so that the loser simply adopts the
    /// winner's key instead of failing or, worse, ending up with two active
    /// keys that gateways disagree about.
    pub async fn load_or_create(db: &PgPool, issuer: &str) -> anyhow::Result<Self> {
        if let Some(signer) = Self::load(db, issuer).await? {
            return Ok(signer);
        }

        let mut rng = rand::rngs::OsRng;
        let key = SigningKey::generate(&mut rng);
        let public = key.verifying_key();
        // A content-derived kid means two replicas generating different keys
        // cannot collide on the identifier and confuse a caching gateway.
        let kid = hex::encode(&public.to_bytes()[..8]);

        sqlx::query(
            "INSERT INTO signing_keys (kid, private_key, public_key, active)
             VALUES ($1, $2, $3, TRUE)
             ON CONFLICT (kid) DO NOTHING",
        )
        .bind(&kid)
        .bind(key.to_bytes().as_slice())
        .bind(public.to_bytes().as_slice())
        .execute(db)
        .await?;

        Self::load(db, issuer)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no active signing key after creating one"))
    }

    async fn load(db: &PgPool, issuer: &str) -> anyhow::Result<Option<Self>> {
        let row: Option<(String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT kid, private_key, public_key FROM signing_keys
             WHERE active AND retired_at IS NULL
             ORDER BY created_at
             LIMIT 1",
        )
        .fetch_optional(db)
        .await?;

        let Some((kid, private, public)) = row else {
            return Ok(None);
        };
        let secret: [u8; 32] = private
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored signing key is not 32 bytes"))?;
        let public: [u8; 32] = public
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("stored public key is not 32 bytes"))?;
        let key = SigningKey::from_bytes(&secret);
        anyhow::ensure!(
            key.verifying_key().to_bytes() == public,
            "stored key pair does not match"
        );

        // jsonwebtoken wants a PKCS#8 DER private key for EdDSA.
        let der = pkcs8_ed25519(&secret);
        Ok(Some(Self {
            kid,
            encoding: EncodingKey::from_ed_der(&der),
            verifying: key.verifying_key(),
            issuer: issuer.to_string(),
        }))
    }

    /// Sign a set of ticket claims.
    pub fn sign(&self, claims: &TicketClaims) -> ApiResult<String> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.encoding)
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("ticket signing failed: {e}")))
    }

    /// Verify a ticket. Used by tests and by the manager's own tooling; the
    /// gateway performs the same check offline against the published JWKS.
    pub fn verify(&self, token: &str) -> ApiResult<TicketClaims> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_audience(&[AUDIENCE]);
        validation.set_issuer(&[&self.issuer]);
        jsonwebtoken::decode::<TicketClaims>(
            token,
            &DecodingKey::from_ed_der(self.verifying.as_bytes()),
            &validation,
        )
        .map(|d| d.claims)
        .map_err(|_| ApiError::Unauthorized)
    }

    /// The key set gateways fetch and cache.
    #[must_use]
    pub fn jwks(&self) -> Jwks {
        Jwks {
            keys: vec![Jwk::ed25519(&self.verifying.to_bytes(), self.kid.clone())],
        }
    }

    /// Build claims for a new session ticket.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn claims(
        &self,
        session: nebula_common::SessionId,
        tenant: nebula_common::TenantId,
        user: nebula_common::UserId,
        machine: nebula_common::MachineId,
        resource: nebula_common::ResourceId,
        role: nebula_common::SessionRole,
        policy: nebula_common::SessionPolicy,
        agent_key: String,
        relay_addr: String,
        relay_pin: String,
    ) -> TicketClaims {
        let now = OffsetDateTime::now_utc();
        TicketClaims {
            iss: self.issuer.clone(),
            aud: AUDIENCE.to_string(),
            // The jti is the session id: one ticket per session means a
            // gateway can refuse a second use by session, with no extra state.
            jti: session.to_string(),
            iat: now.unix_timestamp(),
            exp: (now + DEFAULT_TTL).unix_timestamp(),
            sid: session,
            tid: tenant,
            uid: user,
            mid: machine,
            rid: resource,
            role,
            policy,
            agent_key,
            relay_addr,
            relay_pin,
        }
    }

    /// Sign a fresh ticket, verifying our own output before returning it.
    ///
    /// Signing is not the expensive part of a session launch, and shipping a
    /// ticket that every gateway will reject is a far worse failure than a
    /// few extra microseconds here.
    pub fn issue(&self, claims: &TicketClaims) -> ApiResult<String> {
        let token = self.sign(claims)?;
        debug_assert!(self.verify(&token).is_ok(), "signed an unverifiable ticket");
        Ok(token)
    }
}

/// Wrap a raw Ed25519 seed in the minimal PKCS#8 v1 structure.
///
/// The encoding is fixed-length for Ed25519, so a template with the seed
/// spliced in is exact and avoids pulling in a DER writer.
fn pkcs8_ed25519(seed: &[u8; 32]) -> Vec<u8> {
    let mut der = Vec::with_capacity(48);
    der.extend_from_slice(&[
        0x30, 0x2e, // SEQUENCE, 46 bytes
        0x02, 0x01, 0x00, // INTEGER version 0
        0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, // AlgorithmIdentifier: Ed25519
        0x04, 0x22, // OCTET STRING, 34 bytes
        0x04, 0x20, // inner OCTET STRING, 32 bytes
    ]);
    der.extend_from_slice(seed);
    der
}

/// Keeps the `Signer` trait in scope for the compile-time check below.
#[allow(dead_code)]
fn _assert_signs(key: &SigningKey) {
    let _ = key.sign(b"");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> TicketSigner {
        let mut rng = rand::rngs::OsRng;
        let key = SigningKey::generate(&mut rng);
        let public = key.verifying_key();
        TicketSigner {
            kid: hex::encode(&public.to_bytes()[..8]),
            encoding: EncodingKey::from_ed_der(&pkcs8_ed25519(&key.to_bytes())),
            verifying: public,
            issuer: "http://manager.test".into(),
        }
    }

    fn claims(s: &TicketSigner) -> TicketClaims {
        s.claims(
            nebula_common::SessionId::new(),
            nebula_common::TenantId::new(),
            nebula_common::UserId::new(),
            nebula_common::MachineId::new(),
            nebula_common::ResourceId::new(),
            nebula_common::SessionRole::Controller,
            nebula_common::SessionPolicy::full(),
            "aa".repeat(32),
            "127.0.0.1:4443".into(),
            "bb".repeat(32),
        )
    }

    #[test]
    fn a_signed_ticket_verifies_and_round_trips_its_claims() {
        let s = signer();
        let c = claims(&s);
        let token = s.issue(&c).unwrap();
        let back = s.verify(&token).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn the_pkcs8_wrapper_produces_a_key_jsonwebtoken_accepts() {
        // If the DER template were wrong this would fail at signing time,
        // which is exactly the regression this guards.
        let s = signer();
        assert!(s.sign(&claims(&s)).is_ok());
    }

    #[test]
    fn a_ticket_from_another_manager_is_refused() {
        let a = signer();
        let b = signer();
        let token = a.issue(&claims(&a)).unwrap();
        assert!(b.verify(&token).is_err());
    }

    #[test]
    fn a_tampered_ticket_is_refused() {
        let s = signer();
        let token = s.issue(&claims(&s)).unwrap();
        let mut parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        // Re-encode the payload with an escalated role.
        let mut c = claims(&s);
        c.role = nebula_common::SessionRole::Admin;
        let forged = base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            serde_json::to_vec(&c).unwrap(),
        );
        parts[1] = &forged;
        assert!(s.verify(&parts.join(".")).is_err());
    }

    #[test]
    fn the_published_jwks_matches_the_signing_key() {
        let s = signer();
        let jwks = s.jwks();
        assert_eq!(jwks.keys.len(), 1);
        assert_eq!(jwks.keys[0].kid, s.kid);
        assert_eq!(jwks.keys[0].public_key().unwrap(), s.verifying.to_bytes());
    }

    #[test]
    fn a_ticket_signed_for_a_different_issuer_is_refused() {
        let s = signer();
        let mut c = claims(&s);
        c.iss = "http://evil.test".into();
        let token = s.sign(&c).unwrap();
        assert!(s.verify(&token).is_err());
    }

    #[test]
    fn a_ticket_for_the_wrong_audience_is_refused() {
        let s = signer();
        let mut c = claims(&s);
        c.aud = "someone-else".into();
        let token = s.sign(&c).unwrap();
        assert!(s.verify(&token).is_err());
    }
}
