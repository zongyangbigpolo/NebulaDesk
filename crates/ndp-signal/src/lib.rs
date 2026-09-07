//! Signalling for NebulaDesk.
//!
//! Everything that is *about* a session rather than *part* of one: the
//! agent's persistent tunnel to a gateway, the client's ticket redemption,
//! and the pairing tokens a relay uses to splice two connections together.
//!
//! None of this touches media. The media path is [`ndp_transport`], and the
//! only thing signalling contributes to it is telling both peers where to
//! meet and proving to the relay that they are allowed to.

#![warn(missing_docs)]

pub mod direct;
pub mod framing;
pub mod messages;
pub mod pairing;

pub use framing::{read_message, request, write_message};
pub use messages::{
    AgentHello, AgentHelloAck, AgentMessage, ClientHello, ClientHelloAck, GatewayMessage,
    RelayHello, RelayHelloAck, SessionRequest, MAX_MESSAGE,
};
pub use pairing::{generate_secret, PairKey, PairToken, Side, TokenError};

/// A signalling failure.
#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    /// An authenticated direct offer had invalid identity or addressing.
    #[error("invalid direct-path offer: {0}")]
    InvalidDirect(&'static str),
    /// The message did not fit the size limit.
    #[error("signalling message of {0} bytes exceeds the limit")]
    TooLarge(usize),

    /// The message was not valid JSON, or not the expected shape.
    #[error("malformed signalling message: {0}")]
    Malformed(#[from] serde_json::Error),

    /// The peer closed the stream partway through a message.
    #[error("signalling stream ended early: {0}")]
    Read(#[from] quinn::ReadExactError),

    /// Writing to the stream failed.
    #[error("signalling write failed: {0}")]
    Write(#[from] quinn::WriteError),

    /// The stream could not be opened.
    #[error("signalling stream could not be opened: {0}")]
    Connection(#[from] quinn::ConnectionError),

    /// The stream was already closed.
    #[error("signalling stream was closed")]
    Closed,

    /// A pairing token was unusable.
    #[error(transparent)]
    Token(#[from] TokenError),
}

impl From<quinn::ClosedStream> for SignalError {
    fn from(_: quinn::ClosedStream) -> Self {
        Self::Closed
    }
}
