//! **NDP v3** — the NebulaDesk wire protocol.
//!
//! This crate is pure data: encoding and decoding only, no IO and no async.
//! That makes the entire wire format exhaustively unit-testable and keeps it
//! usable from the agent, the client, and from test harnesses alike.
//!
//! # Layering
//!
//! ```text
//! QUIC connection (ALPN "ndp/3")
//!   └── logical Channel (Control | Video | Audio | Input | Clipboard | File)
//!         └── sealed record  = MsgHeader (16B cleartext) ‖ AEAD(payload)
//!               └── payload  = control JSON | encoded media | packed input
//! ```
//!
//! The 16-byte [`MsgHeader`] always travels in the clear and is used as AEAD
//! associated data by `ndp-crypto`, so a record cannot be replayed under a
//! different header.
//!
//! # Channel/stream mapping
//!
//! | Channel | QUIC carrier | Reliability |
//! |---|---|---|
//! | [`Channel::Control`] | one bidirectional stream | reliable, ordered |
//! | [`Channel::Video`] | **one unidirectional stream per frame** | reliable, resettable |
//! | [`Channel::Audio`] | datagrams | unreliable |
//! | [`Channel::Input`] | one bidirectional stream | reliable, ordered |
//! | [`Channel::Clipboard`] | bidirectional stream on demand | reliable |
//! | [`Channel::File`] | one bidirectional stream per transfer | reliable |
//!
//! One stream per video frame is the key latency decision: frames never
//! head-of-line block each other, the receiver gets framing for free from the
//! stream boundary, and a congested sender can `reset_stream` a stale frame to
//! hand its bandwidth to a fresher one.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod caps;
pub mod channel;
pub mod control;
pub mod header;
pub mod input;
pub mod media;
pub mod transfer;

pub use caps::{
    AudioCodec, AudioParams, Caps, ColorCaps, DisplayGeometry, FeatureFlags, VideoCodec,
};
pub use channel::Channel;
pub use control::{ByeReason, ControlMessage, CursorShape, QosReport};
pub use header::{MsgFlags, MsgHeader, MsgKind, HEADER_LEN, PROTOCOL_VERSION};
pub use input::{InputEvent, InputKind, KeyCode, Modifiers, MouseButton};
pub use media::{AudioFrameInfo, VideoFrameInfo};
pub use transfer::{ClipboardFormat, ClipboardOffer, FileChunkHeader, FileOffer};

/// Errors produced while decoding an NDP message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtoError {
    /// Buffer ended before a complete structure could be read.
    #[error("truncated: need {need} bytes, have {have}")]
    Truncated {
        /// Bytes required to decode the structure.
        need: usize,
        /// Bytes actually available.
        have: usize,
    },
    /// Peer speaks a protocol version we do not implement.
    #[error("unsupported protocol version {0} (this build speaks {PROTOCOL_VERSION})")]
    UnsupportedVersion(u8),
    /// The `kind` byte does not name a message we know.
    #[error("unknown message kind {0:#04x}")]
    UnknownKind(u8),
    /// A discriminant inside a payload was out of range.
    #[error("invalid {field}: {value}")]
    InvalidValue {
        /// Name of the offending field.
        field: &'static str,
        /// The rejected value, rendered for diagnostics.
        value: String,
    },
    /// A JSON control payload failed to parse.
    #[error("malformed control payload: {0}")]
    Json(String),
}

impl From<serde_json::Error> for ProtoError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

/// Convenience result alias for this crate.
pub type Result<T> = std::result::Result<T, ProtoError>;

/// Read exactly `N` bytes from the front of `buf`, advancing it.
pub(crate) fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if buf.len() < n {
        return Err(ProtoError::Truncated {
            need: n,
            have: buf.len(),
        });
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

macro_rules! read_int {
    ($name:ident, $ty:ty, $n:literal) => {
        pub(crate) fn $name(buf: &mut &[u8]) -> Result<$ty> {
            let bytes = take(buf, $n)?;
            Ok(<$ty>::from_le_bytes(
                bytes.try_into().expect("length checked"),
            ))
        }
    };
}

read_int!(read_u8, u8, 1);
read_int!(read_u16, u16, 2);
read_int!(read_u32, u32, 4);
read_int!(read_u64, u64, 8);
read_int!(read_f32, f32, 4);
