//! QUIC transport for NebulaDesk.
//!
//! # Why QUIC only
//!
//! NebulaDesk carries interactive video: a single dropped packet must never
//! stall audio or input. TCP gives one ordered byte stream and therefore one
//! head-of-line queue for everything, which is exactly the wrong shape. QUIC
//! gives independent streams plus unreliable datagrams over one congestion
//! controller and one handshake, so this crate never offers a TCP fallback.
//!
//! # Carrier mapping
//!
//! Every [`Channel`] is mapped onto the QUIC primitive that matches its
//! delivery requirements:
//!
//! | Channel | Carrier | Rationale |
//! |---|---|---|
//! | `Control` | one long-lived bidirectional stream | low rate, must be ordered |
//! | `Input` | one long-lived bidirectional stream | ordering is correctness: a click must not overtake the move that positioned it |
//! | `Video` | one unidirectional stream **per frame** | frames are independent; a lost frame's retransmits cannot block the next one, and a stale frame can be cancelled outright with `reset_stream` |
//! | `Audio` | datagrams | 20 ms of audio is worthless once retransmitted; Opus in-band FEC repairs loss instead |
//! | `Clipboard`, `File` | one unidirectional stream per transfer | a 40 MB paste must not stall anything else |
//!
//! Each carrier is prefixed with its 4-byte little-endian channel id, so the
//! receiver can classify a stream or datagram before decrypting. On the
//! long-lived streams records are additionally length-prefixed; on
//! per-message streams the stream's FIN delimits the record, which removes a
//! framing layer from the video hot path.
//!
//! # Layering
//!
//! QUIC's own TLS protects the hop. It is *not* the session's security
//! boundary: a relay terminates QUIC and can see every byte it forwards. The
//! payload records are therefore separately sealed end-to-end by
//! [`ndp_crypto`], and this crate only ever moves already-sealed bytes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod endpoint;
mod session;
mod tls;
mod wire;

pub use config::{TransportConfig, ALPN_AGENT_GATEWAY, ALPN_RELAY, ALPN_SESSION};
pub use endpoint::{client_endpoint, connect, server_endpoint, ServerCredentials};
pub use session::{FrameOutcome, Incoming, Session, SessionReceiver};
pub use tls::{client_config, dev_credentials, CertificateFingerprint};

/// Errors surfaced by the transport layer.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The QUIC connection failed or was closed by the peer.
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),

    /// A stream write failed.
    #[error("write: {0}")]
    Write(#[from] quinn::WriteError),

    /// A stream read failed.
    #[error("read: {0}")]
    Read(#[from] quinn::ReadError),

    /// The responder refused the session during the handshake.
    #[error("session refused: {reason}")]
    Rejected {
        /// Why the responder said no.
        reason: String,
    },

    /// Reading a whole stream failed.
    #[error("read to end: {0}")]
    ReadToEnd(#[from] quinn::ReadToEndError),

    /// A datagram could not be sent.
    #[error("datagram: {0}")]
    Datagram(#[from] quinn::SendDatagramError),

    /// The peer closed a carrier before sending a complete record.
    #[error("{carrier} truncated after {got} bytes")]
    Truncated {
        /// Which carrier was cut short.
        carrier: &'static str,
        /// How many bytes did arrive.
        got: usize,
    },

    /// A record exceeded the negotiated limit; refused before allocating.
    #[error("record of {len} bytes exceeds the {limit} byte limit")]
    RecordTooLarge {
        /// The advertised length.
        len: usize,
        /// The configured ceiling.
        limit: usize,
    },

    /// The peer used a channel id that is not part of this protocol version.
    #[error("unknown channel id {0}")]
    UnknownChannel(u32),

    /// A channel was used over a carrier it is not mapped to.
    #[error("channel {channel} is not carried by {carrier}")]
    WrongCarrier {
        /// The offending channel.
        channel: &'static str,
        /// The carrier it arrived on.
        carrier: &'static str,
    },

    /// End-to-end record sealing or opening failed.
    #[error(transparent)]
    Crypto(#[from] ndp_crypto::CryptoError),

    /// A record failed to decode once decrypted.
    #[error(transparent)]
    Proto(#[from] ndp_proto::ProtoError),

    /// Endpoint or certificate setup failed.
    #[error("configuration: {0}")]
    Config(String),

    /// Local socket I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The session's background reader has stopped.
    #[error("session closed")]
    Closed,
}

/// Convenience alias for transport results.
pub type Result<T> = std::result::Result<T, TransportError>;
