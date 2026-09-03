//! Relay pairing tokens.
//!
//! A relay must splice two connections that belong to the same session, and
//! it must do so without a database, without a callback to the gateway, and
//! without trusting either peer. The gateway therefore mints two short-lived
//! tokens per session — one for the client, one for the agent — each a MAC
//! over the session id and the side it authorises. The relay verifies the MAC
//! with a shared secret and pairs the two halves.
//!
//! # Why a shared secret rather than a signature
//!
//! A relay verifies a token on every connection attempt, including hostile
//! ones. HMAC-SHA256 is roughly two hash compressions; an asymmetric
//! verification is thousands of times more expensive and would turn the
//! pairing check into the cheapest denial-of-service target in the system.
//! The secret only ever authorises byte forwarding for a session the gateway
//! already authorised, so its compromise cannot grant access to a machine:
//! the Noise handshake between client and agent still has to succeed, and the
//! relay never holds a key that can complete it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use nebula_common::SessionId;
use sha2::Sha256;

/// How long a pairing token remains usable.
///
/// Long enough for a client on a slow link to finish connecting, short enough
/// that a token captured in transit is worthless by the time it is replayed.
pub const TOKEN_TTL: Duration = Duration::from_secs(30);

/// The smallest acceptable pairing secret.
pub const MIN_SECRET_LEN: usize = 32;

/// Which end of the session a token authorises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// The user's client application.
    Client,
    /// The agent on the server machine.
    Agent,
}

impl Side {
    /// The other end of the same session.
    #[must_use]
    pub const fn peer(self) -> Self {
        match self {
            Side::Client => Side::Agent,
            Side::Agent => Side::Client,
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Side::Client => 1,
            Side::Agent => 2,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Side::Client),
            2 => Some(Side::Agent),
            _ => None,
        }
    }
}

/// Why a pairing token was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// The token was not the expected length, or not valid base64.
    #[error("malformed pairing token")]
    Malformed,
    /// The MAC did not verify.
    #[error("pairing token failed authentication")]
    BadSignature,
    /// The token's validity window has passed.
    #[error("pairing token has expired")]
    Expired,
}

/// The wire length of a decoded token: 16 id + 1 side + 8 expiry + 32 MAC.
const TOKEN_LEN: usize = 16 + 1 + 8 + 32;

/// A verified pairing token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairToken {
    /// The session the bearer may join.
    pub session: SessionId,
    /// Which half of the session it authorises.
    pub side: Side,
    /// Expiry, Unix seconds.
    pub expires_at: u64,
}

/// Mints and verifies pairing tokens with a deployment-wide secret.
#[derive(Clone)]
pub struct PairKey {
    key: Vec<u8>,
}

impl std::fmt::Debug for PairKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairKey(<redacted>)")
    }
}

impl PairKey {
    /// Build from a shared secret.
    ///
    /// # Errors
    ///
    /// Fails if the secret is shorter than [`MIN_SECRET_LEN`]. A short secret
    /// is brute-forceable offline from a single observed token, which would
    /// let anyone mint pairings for arbitrary sessions.
    pub fn new(secret: &[u8]) -> Result<Self, TokenError> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(TokenError::Malformed);
        }
        Ok(Self {
            key: secret.to_vec(),
        })
    }

    /// Mint a token valid for [`TOKEN_TTL`] from now.
    #[must_use]
    pub fn mint(&self, session: SessionId, side: Side) -> String {
        self.mint_at(session, side, now() + TOKEN_TTL.as_secs())
    }

    fn mint_at(&self, session: SessionId, side: Side, expires_at: u64) -> String {
        let mut body = Vec::with_capacity(TOKEN_LEN);
        body.extend_from_slice(session.as_uuid().as_bytes());
        body.push(side.tag());
        body.extend_from_slice(&expires_at.to_be_bytes());
        body.extend_from_slice(&self.mac(&body));
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(body)
    }

    /// Verify a token and recover what it authorises.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError`] if the token is malformed, unauthentic or
    /// expired. The MAC is checked before the expiry so that an attacker
    /// cannot use response timing to learn whether a forged token happened to
    /// carry a plausible timestamp.
    pub fn verify(&self, token: &str) -> Result<PairToken, TokenError> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| TokenError::Malformed)?;
        if raw.len() != TOKEN_LEN {
            return Err(TokenError::Malformed);
        }
        let (body, mac) = raw.split_at(TOKEN_LEN - 32);

        let mut h = <Hmac<Sha256> as Mac>::new_from_slice(&self.key)
            .expect("HMAC accepts a key of any length");
        h.update(body);
        h.verify_slice(mac).map_err(|_| TokenError::BadSignature)?;

        let mut id = [0u8; 16];
        id.copy_from_slice(&body[..16]);
        let side = Side::from_tag(body[16]).ok_or(TokenError::Malformed)?;
        let expires_at = u64::from_be_bytes(body[17..25].try_into().expect("eight bytes"));

        if now() >= expires_at {
            return Err(TokenError::Expired);
        }
        Ok(PairToken {
            session: SessionId::from_uuid(uuid::Uuid::from_bytes(id)),
            side,
            expires_at,
        })
    }

    fn mac(&self, body: &[u8]) -> [u8; 32] {
        let mut h = <Hmac<Sha256> as Mac>::new_from_slice(&self.key)
            .expect("HMAC accepts a key of any length");
        h.update(body);
        h.finalize().into_bytes().into()
    }
}

/// Generate a fresh pairing secret, hex encoded, for an operator to configure
/// on every gateway and relay in a deployment.
#[must_use]
pub fn generate_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> PairKey {
        PairKey::new(b"a-pairing-secret-of-sufficient-length").unwrap()
    }

    #[test]
    fn a_minted_token_verifies_and_round_trips() {
        let session = SessionId::new();
        let k = key();
        let token = k.mint(session, Side::Client);
        let parsed = k.verify(&token).unwrap();
        assert_eq!(parsed.session, session);
        assert_eq!(parsed.side, Side::Client);
        assert!(parsed.expires_at > now());
    }

    #[test]
    fn the_two_sides_of_a_session_get_different_tokens() {
        let session = SessionId::new();
        let k = key();
        // Otherwise one peer could present its own token twice and be spliced
        // to itself, or a captured client token would admit an agent.
        assert_ne!(k.mint(session, Side::Client), k.mint(session, Side::Agent));
    }

    #[test]
    fn a_token_from_another_deployment_is_refused() {
        let token = key().mint(SessionId::new(), Side::Agent);
        let other = PairKey::new(b"a-different-secret-of-sufficient-len").unwrap();
        assert_eq!(other.verify(&token), Err(TokenError::BadSignature));
    }

    #[test]
    fn tampering_with_the_side_is_detected() {
        let k = key();
        let token = k.mint(SessionId::new(), Side::Client);
        let mut raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .unwrap();
        raw[16] = Side::Agent.tag();
        let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(k.verify(&forged), Err(TokenError::BadSignature));
    }

    #[test]
    fn extending_the_expiry_is_detected() {
        let k = key();
        let token = k.mint(SessionId::new(), Side::Client);
        let mut raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&token)
            .unwrap();
        raw[17..25].copy_from_slice(&u64::MAX.to_be_bytes());
        let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(k.verify(&forged), Err(TokenError::BadSignature));
    }

    #[test]
    fn an_expired_token_is_refused() {
        let k = key();
        let token = k.mint_at(SessionId::new(), Side::Client, now() - 1);
        assert_eq!(k.verify(&token), Err(TokenError::Expired));
    }

    #[test]
    fn junk_is_rejected_without_panicking() {
        let k = key();
        assert_eq!(k.verify(""), Err(TokenError::Malformed));
        assert_eq!(k.verify("!!!not base64!!!"), Err(TokenError::Malformed));
        assert_eq!(k.verify("c2hvcnQ"), Err(TokenError::Malformed));
    }

    #[test]
    fn short_secrets_are_refused() {
        // A 16-byte secret is recoverable offline from one observed token.
        assert!(PairKey::new(b"too-short").is_err());
        assert!(PairKey::new(&[0u8; MIN_SECRET_LEN]).is_ok());
    }

    #[test]
    fn generated_secrets_are_long_and_distinct() {
        let a = generate_secret();
        let b = generate_secret();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(PairKey::new(a.as_bytes()).is_ok());
    }

    #[test]
    fn sides_are_complementary() {
        assert_eq!(Side::Client.peer(), Side::Agent);
        assert_eq!(Side::Agent.peer(), Side::Client);
    }
}
