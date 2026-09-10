//! Cross-cutting primitives shared by every NebulaDesk component.
//!
//! Deliberately dependency-light: identifiers, a common error type and the
//! observability bootstrap. Anything protocol-specific belongs in `ndp-proto`.

pub mod application;
pub mod ids;
pub mod obs;
pub mod ticket;

pub use application::{ApplicationCapability, ApplicationLaunch, LaunchTarget};
pub use ids::{MachineId, RelayId, ResourceId, SessionId, TenantId, UserId};
pub use ticket::{Jwk, Jwks, SessionPolicy, SessionRole, TicketAuthority, TicketClaims};

/// Errors that are meaningful across component boundaries.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("upstream unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
