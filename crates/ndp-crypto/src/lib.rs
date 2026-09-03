//! End-to-end cryptography for NebulaDesk sessions.
//!
//! Gateways and relays forward bytes they cannot read: everything above QUIC
//! is sealed between the client and the agent.
//!
//! # Handshake
//!
//! `Noise_IK_25519_ChaChaPoly_BLAKE2s`, one round trip:
//!
//! * The **agent** owns a long-lived X25519 static keypair whose public half is
//!   registered with the manager at enrolment.
//! * The **client** learns that public key inside its session ticket, so it can
//!   authenticate the agent before sending anything sensitive — a relay that
//!   substituted a peer would fail the handshake.
//! * The client authenticates itself by carrying the manager-signed session
//!   ticket in the first handshake payload, which the agent verifies offline.
//!
//! This replaces V1's long-lived pre-shared key: every session now gets fresh
//! keys with forward secrecy, and there is no shared secret to leak.
//!
//! # Record layer
//!
//! Noise only performs the handshake. The traffic keys are then split out and
//! driven by an explicit per-channel nonce, because audio rides unreliable
//! datagrams where Noise's implicit in-order counter cannot work:
//!
//! ```text
//! nonce = channel_id (u32 LE) ‖ sequence (u64 LE)      // 12 bytes
//! aad   = the 16-byte cleartext MsgHeader
//! ```
//!
//! Only the low 32 bits of the sequence travel on the wire; each side keeps a
//! roll-over counter locally (as SRTP does), so nonces never repeat.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod keys;
mod record;
mod replay;
mod handshake;

pub use handshake::{HandshakeResult, Initiator, Responder, NOISE_PARAMS};
pub use keys::{PublicKey, StaticKeypair, PUBLIC_KEY_LEN, SECRET_KEY_LEN};
pub use record::{RecordOpener, RecordSealer, SessionKeys, AEAD_TAG_LEN};
pub use replay::ReplayWindow;

/// Errors raised by the crypto layer.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The Noise handshake failed — wrong peer key, tampering, or a malformed
    /// message.
    #[error("noise handshake failed: {0}")]
    Handshake(String),
    /// AEAD authentication failed: wrong key, corruption, or tampering.
    #[error("record authentication failed on channel {channel}")]
    Decrypt {
        /// Channel the record claimed to belong to.
        channel: &'static str,
    },
    /// The record's sequence number was already seen, or is too old.
    #[error("replayed or stale record: channel {channel}, seq {seq}")]
    Replay {
        /// Channel the record claimed to belong to.
        channel: &'static str,
        /// The rejected sequence number.
        seq: u64,
    },
    /// The record was shorter than a header plus an authentication tag.
    #[error("record too short: {0} bytes")]
    ShortRecord(usize),
    /// A key or handshake payload had the wrong length.
    #[error("invalid key material: {0}")]
    InvalidKey(String),
    /// The handshake payload exceeded the permitted size.
    #[error("handshake payload too large: {0} bytes (max {max})", max = handshake::MAX_HANDSHAKE_PAYLOAD)]
    PayloadTooLarge(usize),
    /// The wire format under the AEAD was not valid NDP.
    #[error(transparent)]
    Proto(#[from] ndp_proto::ProtoError),
}

/// Convenience result alias.
pub type Result<T> = std::result::Result<T, CryptoError>;
