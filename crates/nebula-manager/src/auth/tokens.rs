//! Access tokens, refresh tokens and machine credentials.
//!
//! Three different kinds of secret live here, and they are deliberately
//! different shapes:
//!
//! * **Access tokens** are short-lived HS256 JWTs. They are only ever
//!   verified by the manager, so a symmetric secret is enough and avoids a
//!   database round trip on every request.
//! * **Refresh tokens** are opaque random strings stored hashed, and rotated
//!   on every use. Being opaque means a stolen refresh token can be revoked;
//!   being rotated means a stolen one is detectable when the real client next
//!   tries to use the old value.
//! * **Machine credentials** are opaque random strings issued at enrolment.
//!   They authenticate an agent process, not a person, and never expire —
//!   revocation is deleting the machine.

use base64::Engine;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use nebula_common::{TenantId, UserId};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

use crate::error::{ApiError, ApiResult};

/// Bytes of entropy in an opaque token. 256 bits is far beyond guessable and
/// costs nothing.
const TOKEN_BYTES: usize = 32;

/// Claims carried by an access token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    /// Subject: the user id.
    pub sub: String,
    /// Tenant the user belongs to.
    pub tid: String,
    /// The user's directory role.
    pub role: String,
    /// Issued at, Unix seconds.
    pub iat: i64,
    /// Expiry, Unix seconds.
    pub exp: i64,
}

impl AccessClaims {
    /// The authenticated user's id.
    pub fn user_id(&self) -> ApiResult<UserId> {
        self.sub.parse().map_err(|_| ApiError::Unauthorized)
    }

    /// The authenticated user's tenant.
    pub fn tenant_id(&self) -> ApiResult<TenantId> {
        self.tid.parse().map_err(|_| ApiError::Unauthorized)
    }
}

/// Issues and verifies access tokens.
#[derive(Clone)]
pub struct AccessTokens {
    encoding: EncodingKey,
    decoding: DecodingKey,
    ttl: Duration,
}

impl std::fmt::Debug for AccessTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessTokens")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl AccessTokens {
    /// Build from the configured secret.
    #[must_use]
    pub fn new(secret: &str, ttl: std::time::Duration) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
            ttl: Duration::try_from(ttl).unwrap_or(Duration::minutes(15)),
        }
    }

    /// How long an issued token lasts, in seconds.
    #[must_use]
    pub fn ttl_secs(&self) -> i64 {
        self.ttl.whole_seconds()
    }

    /// Mint a token for a user.
    pub fn issue(&self, user: UserId, tenant: TenantId, role: &str) -> ApiResult<String> {
        let now = OffsetDateTime::now_utc();
        let claims = AccessClaims {
            sub: user.to_string(),
            tid: tenant.to_string(),
            role: role.to_string(),
            iat: now.unix_timestamp(),
            exp: (now + self.ttl).unix_timestamp(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| ApiError::Internal(anyhow::anyhow!("token encoding failed: {e}")))
    }

    /// Verify a token and recover its claims.
    pub fn verify(&self, token: &str) -> ApiResult<AccessClaims> {
        let mut validation = Validation::new(Algorithm::HS256);
        // The default requires an `aud`, which access tokens do not carry.
        validation.validate_aud = false;
        // jsonwebtoken allows 60 seconds of grace by default. Access tokens
        // already last minutes and are refreshed automatically, so extending
        // a revoked one's life is a cost with no matching benefit.
        validation.leeway = 0;
        jsonwebtoken::decode::<AccessClaims>(token, &self.decoding, &validation)
            .map(|d| d.claims)
            .map_err(|_| ApiError::Unauthorized)
    }
}

/// An opaque secret plus the hash to store for it.
#[derive(Debug, Clone)]
pub struct OpaqueToken {
    /// The value handed to the caller. Never persisted.
    pub secret: String,
    /// SHA-256 of the secret, hex encoded. This is what goes in the database.
    pub hash: String,
}

/// Generate a fresh opaque token.
#[must_use]
pub fn generate_opaque() -> OpaqueToken {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_token(&secret);
    OpaqueToken { secret, hash }
}

/// Hash an opaque token for storage or lookup.
///
/// A plain SHA-256 is correct here, unlike for passwords: the input already
/// has 256 bits of entropy, so there is nothing for an attacker to guess and
/// no reason to pay Argon2's cost on every request.
#[must_use]
pub fn hash_token(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// Split a `Machine <id>.<secret>` credential into its parts.
pub fn parse_machine_credential(raw: &str) -> ApiResult<(uuid::Uuid, &str)> {
    let (id, secret) = raw.split_once('.').ok_or(ApiError::Unauthorized)?;
    let id = id.parse().map_err(|_| ApiError::Unauthorized)?;
    if secret.is_empty() {
        return Err(ApiError::Unauthorized);
    }
    Ok((id, secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> AccessTokens {
        AccessTokens::new(
            "a-secret-long-enough-for-tests-0123456789",
            std::time::Duration::from_secs(900),
        )
    }

    #[test]
    fn an_issued_access_token_verifies() {
        let t = tokens();
        let user = UserId::new();
        let tenant = TenantId::new();
        let jwt = t.issue(user, tenant, "ADMIN").unwrap();
        let claims = t.verify(&jwt).unwrap();
        assert_eq!(claims.user_id().unwrap(), user);
        assert_eq!(claims.tenant_id().unwrap(), tenant);
        assert_eq!(claims.role, "ADMIN");
    }

    #[test]
    fn a_token_from_another_secret_is_refused() {
        let jwt = tokens()
            .issue(UserId::new(), TenantId::new(), "USER")
            .unwrap();
        let other = AccessTokens::new(
            "a-different-secret-also-long-enough-98765",
            std::time::Duration::from_secs(900),
        );
        assert!(other.verify(&jwt).is_err());
    }

    #[test]
    fn a_tampered_token_is_refused() {
        let t = tokens();
        let jwt = t.issue(UserId::new(), TenantId::new(), "USER").unwrap();
        let mut bad = jwt.clone();
        // Flip a character in the payload segment.
        let mid = bad.len() / 2;
        let replacement = if bad.as_bytes()[mid] == b'A' {
            'B'
        } else {
            'A'
        };
        bad.replace_range(mid..mid + 1, &replacement.to_string());
        assert!(t.verify(&bad).is_err());
    }

    #[test]
    fn an_expired_token_is_refused() {
        let t = AccessTokens::new(
            "a-secret-long-enough-for-tests-0123456789",
            std::time::Duration::from_secs(900),
        );
        // Encoded directly rather than issued, so the expiry is unambiguously
        // in the past regardless of how fast the test runs.
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let claims = AccessClaims {
            sub: UserId::new().to_string(),
            tid: TenantId::new().to_string(),
            role: "USER".into(),
            iat: now - 7200,
            exp: now - 3600,
        };
        let jwt =
            jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &t.encoding).unwrap();
        assert!(
            t.verify(&jwt).is_err(),
            "a token whose exp has passed must not validate"
        );
    }

    #[test]
    fn opaque_tokens_are_unique_and_their_hash_matches() {
        let a = generate_opaque();
        let b = generate_opaque();
        assert_ne!(a.secret, b.secret);
        assert_eq!(hash_token(&a.secret), a.hash);
        assert_ne!(a.hash, b.hash);
        // The stored form must not reveal the secret.
        assert!(!a.hash.contains(&a.secret));
    }

    #[test]
    fn machine_credentials_parse_and_reject_junk() {
        let id = uuid::Uuid::now_v7();
        let raw = format!("{id}.abc");
        let (parsed, secret) = parse_machine_credential(&raw).unwrap();
        assert_eq!(parsed, id);
        assert_eq!(secret, "abc");

        assert!(parse_machine_credential("no-dot").is_err());
        assert!(parse_machine_credential("not-a-uuid.secret").is_err());
        assert!(parse_machine_credential(&format!("{id}.")).is_err());
    }
}
