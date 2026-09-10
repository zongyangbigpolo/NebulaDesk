//! The 16-byte cleartext record header that prefixes every NDP message.

use crate::{read_u16, read_u32, read_u64, read_u8, ProtoError, Result};

/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: u8 = 3;

/// Serialized size of [`MsgHeader`].
pub const HEADER_LEN: usize = 16;

/// What a record carries.
///
/// Discriminants are grouped by plane so that new messages can be added inside
/// a plane without renumbering: `0x00..` control, `0x20..` media, `0x40..`
/// input, `0x60..` clipboard, `0x70..` file transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MsgKind {
    // ---- control plane ----
    /// Client to agent: offered capabilities.
    Hello = 0x01,
    /// Agent to client: negotiated capabilities.
    HelloAck = 0x02,
    /// Orderly shutdown with a reason.
    Bye = 0x03,
    /// Liveness probe carrying a client timestamp.
    Ping = 0x04,
    /// Response echoing the probe timestamp.
    Pong = 0x05,
    /// Mid-session change of resolution, bitrate or codec.
    CapsUpdate = 0x06,
    /// Monitor arrangement changed.
    DisplayLayout = 0x07,
    /// New cursor bitmap.
    CursorShape = 0x08,
    /// Cursor moved (agent-driven, e.g. by the remote app).
    CursorPos = 0x09,
    /// Receiver-side quality report driving adaptive bitrate.
    QosReport = 0x0a,
    /// Peer address candidates for the optional direct-path upgrade.
    PathCandidates = 0x0b,
    /// Negotiated application-surface lifecycle and scoped interaction.
    Application = 0x0c,

    // ---- media plane ----
    /// One encoded video frame (or codec configuration when
    /// [`MsgFlags::CODEC_CONFIG`] is set).
    VideoFrame = 0x20,
    /// One encoded audio packet.
    AudioFrame = 0x21,

    // ---- input plane ----
    /// A single input event.
    InputEvent = 0x40,
    /// Several coalesced input events (typically mouse moves).
    InputBatch = 0x41,

    // ---- clipboard ----
    /// Announcement that new clipboard content is available.
    ClipboardOffer = 0x60,
    /// Requested clipboard content.
    ClipboardData = 0x61,
    /// A request for one format from an offer.
    ClipboardRequest = 0x62,

    // ---- file transfer ----
    /// Metadata announcing a file transfer.
    FileOffer = 0x70,
    /// One chunk of file data.
    FileChunk = 0x71,
    /// Flow-control acknowledgement for received chunks.
    FileAck = 0x72,
}

impl MsgKind {
    /// Decode a wire discriminant.
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0x01 => Self::Hello,
            0x02 => Self::HelloAck,
            0x03 => Self::Bye,
            0x04 => Self::Ping,
            0x05 => Self::Pong,
            0x06 => Self::CapsUpdate,
            0x07 => Self::DisplayLayout,
            0x08 => Self::CursorShape,
            0x09 => Self::CursorPos,
            0x0a => Self::QosReport,
            0x0b => Self::PathCandidates,
            0x0c => Self::Application,
            0x20 => Self::VideoFrame,
            0x21 => Self::AudioFrame,
            0x40 => Self::InputEvent,
            0x41 => Self::InputBatch,
            0x60 => Self::ClipboardOffer,
            0x61 => Self::ClipboardData,
            0x62 => Self::ClipboardRequest,
            0x70 => Self::FileOffer,
            0x71 => Self::FileChunk,
            0x72 => Self::FileAck,
            other => return Err(ProtoError::UnknownKind(other)),
        })
    }

    /// Whether the payload is a JSON-encoded [`crate::ControlMessage`].
    #[must_use]
    pub const fn is_control(self) -> bool {
        (self as u8) < 0x20
    }
}

/// Per-record flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MsgFlags(pub u16);

impl MsgFlags {
    /// No flags set.
    pub const NONE: Self = Self(0);
    /// Video: this frame is an IDR/keyframe and can be decoded standalone.
    pub const KEYFRAME: Self = Self(1 << 0);
    /// The payload is codec configuration (H.264/HEVC parameter sets, or the
    /// Opus identification header) rather than media data.
    pub const CODEC_CONFIG: Self = Self(1 << 1);
    /// The sender permits the transport to drop this record under congestion.
    pub const DISCARDABLE: Self = Self(1 << 2);
    /// This record completes a logical unit split across several records.
    pub const END_OF_UNIT: Self = Self(1 << 3);

    /// True when every bit in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union of two flag sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for MsgFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// The cleartext record header.
///
/// Layout (little-endian, 16 bytes):
///
/// | offset | size | field |
/// |---|---|---|
/// | 0 | 1 | `version` |
/// | 1 | 1 | `kind` |
/// | 2 | 2 | `flags` |
/// | 4 | 4 | `seq` |
/// | 8 | 8 | `timestamp_us` |
///
/// `seq` is per-channel and per-direction. It is the low 32 bits of a 64-bit
/// counter; `ndp-crypto` keeps the high 32 bits as a local roll-over counter,
/// so the nonce never repeats even though the wire field is only 32 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsgHeader {
    /// Protocol version; must equal [`PROTOCOL_VERSION`].
    pub version: u8,
    /// What this record carries.
    pub kind: MsgKind,
    /// Per-record flags.
    pub flags: MsgFlags,
    /// Per-channel, per-direction sequence number (low 32 bits).
    pub seq: u32,
    /// Capture or presentation time in microseconds.
    pub timestamp_us: u64,
}

impl MsgHeader {
    /// Build a header at the current protocol version.
    #[must_use]
    pub const fn new(kind: MsgKind, seq: u32, timestamp_us: u64) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            kind,
            flags: MsgFlags::NONE,
            seq,
            timestamp_us,
        }
    }

    /// Builder-style flag setter.
    #[must_use]
    pub const fn with_flags(mut self, flags: MsgFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Serialize into a fixed 16-byte array.
    #[must_use]
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = self.version;
        out[1] = self.kind as u8;
        out[2..4].copy_from_slice(&self.flags.0.to_le_bytes());
        out[4..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..16].copy_from_slice(&self.timestamp_us.to_le_bytes());
        out
    }

    /// Parse a header from the front of `buf`, advancing it past the header.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        let version = read_u8(buf)?;
        if version != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(version));
        }
        let kind = MsgKind::from_u8(read_u8(buf)?)?;
        let flags = MsgFlags(read_u16(buf)?);
        let seq = read_u32(buf)?;
        let timestamp_us = read_u64(buf)?;
        Ok(Self {
            version,
            kind,
            flags,
            seq,
            timestamp_us,
        })
    }

    /// Split a wire record into its header and the sealed payload that follows.
    pub fn split(record: &[u8]) -> Result<(Self, &[u8])> {
        let mut cursor = record;
        let header = Self::decode(&mut cursor)?;
        Ok((header, cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips() {
        let h = MsgHeader::new(MsgKind::VideoFrame, 42, 1_234_567)
            .with_flags(MsgFlags::KEYFRAME | MsgFlags::DISCARDABLE);
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN);
        let (parsed, rest) = MsgHeader::split(&bytes).unwrap();
        assert_eq!(parsed, h);
        assert!(rest.is_empty());
        assert!(parsed.flags.contains(MsgFlags::KEYFRAME));
        assert!(!parsed.flags.contains(MsgFlags::CODEC_CONFIG));
    }

    #[test]
    fn header_is_little_endian_at_fixed_offsets() {
        let h = MsgHeader::new(MsgKind::AudioFrame, 0x0102_0304, 0x0a0b_0c0d_0e0f_1011);
        let b = h.to_bytes();
        assert_eq!(b[0], PROTOCOL_VERSION);
        assert_eq!(b[1], MsgKind::AudioFrame as u8);
        assert_eq!(&b[4..8], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&b[8..16], &[0x11, 0x10, 0x0f, 0x0e, 0x0d, 0x0c, 0x0b, 0x0a]);
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut bytes = MsgHeader::new(MsgKind::Hello, 0, 0).to_bytes();
        bytes[0] = 99;
        assert_eq!(
            MsgHeader::split(&bytes).unwrap_err(),
            ProtoError::UnsupportedVersion(99)
        );
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let mut bytes = MsgHeader::new(MsgKind::Hello, 0, 0).to_bytes();
        bytes[1] = 0xff;
        assert_eq!(
            MsgHeader::split(&bytes).unwrap_err(),
            ProtoError::UnknownKind(0xff)
        );
    }

    #[test]
    fn truncated_header_is_rejected() {
        let bytes = MsgHeader::new(MsgKind::Hello, 0, 0).to_bytes();
        let err = MsgHeader::split(&bytes[..10]).unwrap_err();
        assert!(matches!(err, ProtoError::Truncated { .. }));
    }

    #[test]
    fn control_kinds_are_classified() {
        assert!(MsgKind::Hello.is_control());
        assert!(MsgKind::QosReport.is_control());
        assert!(!MsgKind::VideoFrame.is_control());
        assert!(!MsgKind::InputEvent.is_control());
    }
}
