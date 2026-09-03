//! Session tickets.
//!
//! A ticket is the manager's signed statement that one user may open one
//! session against one published resource, with one specific policy. It is
//! the only thing a gateway or an agent needs in order to authorise a
//! connection.
//!
//! # Why signed rather than looked up
//!
//! The gateway sits on the hot path of every connection. If it had to call
//! the manager to check each ticket, every session would pay a control-plane
//! round trip, and a manager hiccup would stop all new sessions everywhere.
//! An EdDSA signature verified against a cached JWKS costs microseconds, works
//! while the manager is down, and scales to as many gateways as you like.
//!
//! The cost of that trade is revocation latency, which is why the TTL is
//! [`DEFAULT_TTL`] — a minute is long enough to survive a slow client and
//! short enough that a revoked entitlement cannot be exploited meaningfully.
//! A ticket authorises *starting* a session, never continuing one: cutting
//! off a live session is the gateway's job, driven by the manager.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::ids::{MachineId, ResourceId, SessionId, TenantId, UserId};

/// How long a freshly minted ticket stays usable.
pub const DEFAULT_TTL: Duration = Duration::seconds(60);

/// Tolerance for clock skew between the manager and a verifier.
pub const CLOCK_SKEW: Duration = Duration::seconds(30);

/// The JWT `aud` value: a ticket is meant for gateways, nothing else.
pub const AUDIENCE: &str = "nebula-gateway";

/// What a session may do, resolved from the entitlement at issue time.
///
/// Carried inside the ticket so the agent enforces policy without ever
/// talking to the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPolicy {
    /// Whether the clipboard may be synchronised.
    pub clipboard: bool,
    /// Whether files may be transferred in either direction.
    pub file_transfer: bool,
    /// Whether audio is forwarded.
    pub audio: bool,
    /// Whether the client may inject input, or is limited to viewing.
    pub input: bool,
}

impl SessionPolicy {
    /// The safe default: watch only.
    #[must_use]
    pub const fn view_only() -> Self {
        Self {
            clipboard: false,
            file_transfer: false,
            audio: false,
            input: false,
        }
    }

    /// Full interactive control.
    #[must_use]
    pub const fn full() -> Self {
        Self {
            clipboard: true,
            file_transfer: true,
            audio: true,
            input: true,
        }
    }
}

/// The role the session is granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SessionRole {
    /// May watch, but not touch.
    Viewer,
    /// May watch and control.
    Controller,
    /// Control plus administrative operations on the machine.
    Admin,
}

impl SessionRole {
    /// The strongest policy this role can ever be granted.
    ///
    /// The entitlement narrows it further; this only sets the ceiling, so a
    /// misconfigured entitlement can never hand a viewer a keyboard.
    #[must_use]
    pub const fn max_policy(self) -> SessionPolicy {
        match self {
            SessionRole::Viewer => SessionPolicy::view_only(),
            SessionRole::Controller | SessionRole::Admin => SessionPolicy::full(),
        }
    }

    /// Parse the database's textual representation.
    pub fn from_db(s: &str) -> crate::Result<Self> {
        match s {
            "VIEWER" => Ok(Self::Viewer),
            "CONTROLLER" => Ok(Self::Controller),
            "ADMIN" => Ok(Self::Admin),
            other => Err(crate::Error::Invalid(format!("unknown role {other}"))),
        }
    }

    /// The database's textual representation.
    #[must_use]
    pub const fn as_db(self) -> &'static str {
        match self {
            SessionRole::Viewer => "VIEWER",
            SessionRole::Controller => "CONTROLLER",
            SessionRole::Admin => "ADMIN",
        }
    }
}

/// The claims carried by a session ticket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketClaims {
    /// Issuer, the manager's public base URL.
    pub iss: String,
    /// Audience; always [`AUDIENCE`].
    pub aud: String,
    /// Unique ticket id, so a gateway can refuse a replayed ticket.
    pub jti: String,
    /// Issued at, Unix seconds.
    pub iat: i64,
    /// Expiry, Unix seconds.
    pub exp: i64,

    /// The session this ticket opens.
    pub sid: SessionId,
    /// Tenant the session belongs to.
    pub tid: TenantId,
    /// The authenticated user.
    pub uid: UserId,
    /// The machine that will serve the session.
    pub mid: MachineId,
    /// The published resource being launched.
    pub rid: ResourceId,

    /// Granted role.
    pub role: SessionRole,
    /// Resolved policy, already clamped to the role's ceiling.
    pub policy: SessionPolicy,

    /// The agent's Noise static public key, hex encoded.
    ///
    /// This is what makes the end-to-end handshake meaningful: the client
    /// learns which key to encrypt to from a source it already trusts, so a
    /// malicious relay cannot substitute its own.
    pub agent_key: String,

    /// QUIC address of the relay chosen for this session.
    pub relay_addr: String,
    /// Certificate pin for that relay, hex encoded SHA-256.
    pub relay_pin: String,
}

impl TicketClaims {
    /// Whether the ticket is currently valid, allowing for clock skew.
    #[must_use]
    pub fn is_valid_at(&self, now: OffsetDateTime) -> bool {
        let now = now.unix_timestamp();
        let skew = CLOCK_SKEW.whole_seconds();
        now >= self.iat - skew && now < self.exp + skew
    }

    /// Seconds until expiry, saturating at zero.
    #[must_use]
    pub fn remaining_secs(&self, now: OffsetDateTime) -> i64 {
        (self.exp - now.unix_timestamp()).max(0)
    }
}

/// A JSON Web Key Set, as published by the manager and cached by gateways.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Jwks {
    /// The keys currently trusted for ticket verification.
    pub keys: Vec<Jwk>,
}

/// One Ed25519 public key in JWK form (RFC 8037).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Jwk {
    /// Key type; always `OKP` for Ed25519.
    pub kty: String,
    /// Curve; always `Ed25519`.
    pub crv: String,
    /// Base64url public key, unpadded.
    pub x: String,
    /// Key id, matching the JWT header's `kid`.
    pub kid: String,
    /// Intended use; always `sig`.
    #[serde(rename = "use")]
    pub use_: String,
    /// Algorithm; always `EdDSA`.
    pub alg: String,
    /// Anything a future manager version adds.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Jwk {
    /// Build a JWK from a raw Ed25519 public key.
    #[must_use]
    pub fn ed25519(public_key: &[u8; 32], kid: impl Into<String>) -> Self {
        Self {
            kty: "OKP".into(),
            crv: "Ed25519".into(),
            x: base64url(public_key),
            kid: kid.into(),
            use_: "sig".into(),
            alg: "EdDSA".into(),
            extra: BTreeMap::new(),
        }
    }

    /// Recover the raw public key.
    pub fn public_key(&self) -> crate::Result<[u8; 32]> {
        if self.kty != "OKP" || self.crv != "Ed25519" {
            return Err(crate::Error::Invalid(format!(
                "unsupported key type {}/{}",
                self.kty, self.crv
            )));
        }
        let bytes = base64url_decode(&self.x)?;
        bytes
            .try_into()
            .map_err(|_| crate::Error::Invalid("Ed25519 key must be 32 bytes".into()))
    }
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn base64url_decode(s: &str) -> crate::Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| crate::Error::Invalid(format!("bad base64url: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(now: OffsetDateTime) -> TicketClaims {
        TicketClaims {
            iss: "https://manager.example".into(),
            aud: AUDIENCE.into(),
            jti: "01".into(),
            iat: now.unix_timestamp(),
            exp: (now + DEFAULT_TTL).unix_timestamp(),
            sid: SessionId::new(),
            tid: TenantId::new(),
            uid: UserId::new(),
            mid: MachineId::new(),
            rid: ResourceId::new(),
            role: SessionRole::Controller,
            policy: SessionPolicy::full(),
            agent_key: "aa".repeat(32),
            relay_addr: "203.0.113.7:4443".into(),
            relay_pin: "bb".repeat(32),
        }
    }

    #[test]
    fn a_fresh_ticket_is_valid_and_an_old_one_is_not() {
        let now = OffsetDateTime::now_utc();
        let c = claims(now);
        assert!(c.is_valid_at(now));
        assert!(c.is_valid_at(now + Duration::seconds(59)));
        assert!(!c.is_valid_at(now + DEFAULT_TTL + CLOCK_SKEW + Duration::seconds(1)));
    }

    #[test]
    fn tickets_tolerate_modest_clock_skew_in_both_directions() {
        // Gateways are deployed globally and their clocks drift; refusing a
        // ticket that is one second "early" would be a mysterious outage.
        let now = OffsetDateTime::now_utc();
        let c = claims(now);
        assert!(c.is_valid_at(now - Duration::seconds(20)));
        assert!(!c.is_valid_at(now - Duration::seconds(45)));
    }

    #[test]
    fn viewer_role_cannot_be_granted_input() {
        assert!(!SessionRole::Viewer.max_policy().input);
        assert!(!SessionRole::Viewer.max_policy().clipboard);
        assert!(SessionRole::Controller.max_policy().input);
    }

    #[test]
    fn role_db_representation_roundtrips() {
        for role in [
            SessionRole::Viewer,
            SessionRole::Controller,
            SessionRole::Admin,
        ] {
            assert_eq!(SessionRole::from_db(role.as_db()).unwrap(), role);
        }
        assert!(SessionRole::from_db("ROOT").is_err());
    }

    #[test]
    fn jwk_roundtrips_an_ed25519_key() {
        let key = [7u8; 32];
        let jwk = Jwk::ed25519(&key, "kid-1");
        assert_eq!(jwk.public_key().unwrap(), key);
        // Must survive a JSON round trip: gateways fetch this over HTTP.
        let json = serde_json::to_string(&jwk).unwrap();
        let back: Jwk = serde_json::from_str(&json).unwrap();
        assert_eq!(back.public_key().unwrap(), key);
        assert_eq!(back.kid, "kid-1");
    }

    #[test]
    fn jwk_rejects_a_non_ed25519_key() {
        let mut jwk = Jwk::ed25519(&[0u8; 32], "k");
        jwk.crv = "P-256".into();
        assert!(jwk.public_key().is_err());
    }

    #[test]
    fn claims_survive_a_json_round_trip() {
        let c = claims(OffsetDateTime::now_utc());
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<TicketClaims>(&json).unwrap(), c);
    }

    #[test]
    fn remaining_secs_never_goes_negative() {
        let now = OffsetDateTime::now_utc();
        let c = claims(now);
        assert_eq!(c.remaining_secs(now + Duration::hours(1)), 0);
    }
}
