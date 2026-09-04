//! Clipboard synchronisation and file transfer (P6).
//!
//! Both features are policy-gated: even when a peer advertises the capability,
//! the agent enforces the `allow_clipboard` / `allow_file_transfer` flags from
//! the session ticket. Announce-then-fetch is used for clipboard so that
//! copying a 40 MB image does not stall the session when nobody pastes it.

use serde::{Deserialize, Serialize};

use crate::{read_u32, read_u64, read_u8, ProtoError, Result};

/// A clipboard content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardFormat {
    /// UTF-8 plain text.
    Text,
    /// HTML fragment.
    Html,
    /// PNG image bytes.
    Png,
    /// A list of file paths, transferred lazily via [`FileOffer`].
    FileList,
}

/// Announcement that the sender's clipboard changed.
///
/// The receiver decides whether to fetch, so large payloads only cross the
/// wire when the user actually pastes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardOffer {
    /// Monotonic id used to request or acknowledge this offer.
    pub offer_id: u64,
    /// Formats available, most faithful first.
    pub formats: Vec<ClipboardFormat>,
    /// Total size of the largest format, for policy limits.
    pub size_hint: u64,
}

impl ClipboardFormat {
    /// Wire discriminant.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Text => 1,
            Self::Html => 2,
            Self::Png => 3,
            Self::FileList => 4,
        }
    }

    /// Decode a wire discriminant.
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::Text,
            2 => Self::Html,
            3 => Self::Png,
            4 => Self::FileList,
            other => {
                return Err(ProtoError::InvalidValue {
                    field: "clipboard format",
                    value: other.to_string(),
                })
            }
        })
    }
}

/// A request for one format from an offer.
///
/// Naming the offer rather than just the format is what keeps a paste
/// truthful: both sides copy things, sometimes at the same moment, and a
/// request that only said "text" would be answered with whatever happened to
/// be on the clipboard by the time it arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardRequest {
    /// The offer being answered.
    pub offer_id: u64,
    /// Which format is wanted.
    pub format: ClipboardFormat,
}

/// Serialized size of the header preceding clipboard contents.
pub const CLIPBOARD_DATA_HEADER_LEN: usize = 12;

/// Binary header prefixing clipboard contents.
///
/// Layout (little-endian, 12 bytes): `offer_id(8) format(1) reserved(3)`.
/// Binary rather than JSON because a pasted screenshot is megabytes and
/// base64 would add a third of that for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardDataHeader {
    /// The offer these contents answer.
    pub offer_id: u64,
    /// What the bytes are.
    pub format: ClipboardFormat,
}

impl ClipboardDataHeader {
    /// Serialize into the fixed layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; CLIPBOARD_DATA_HEADER_LEN] {
        let mut out = [0u8; CLIPBOARD_DATA_HEADER_LEN];
        out[0..8].copy_from_slice(&self.offer_id.to_le_bytes());
        out[8] = self.format.to_u8();
        out
    }

    /// Parse the header and return it with the contents that follow.
    pub fn split(payload: &[u8]) -> Result<(Self, &[u8])> {
        let mut cursor = payload;
        let offer_id = read_u64(&mut cursor)?;
        let format = ClipboardFormat::from_u8(read_u8(&mut cursor)?)?;
        // Three reserved bytes, read so the contents start where they should.
        for _ in 0..3 {
            read_u8(&mut cursor)?;
        }
        Ok((Self { offer_id, format }, cursor))
    }

    /// Build a complete payload from the header and the contents.
    #[must_use]
    pub fn payload(&self, contents: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(CLIPBOARD_DATA_HEADER_LEN + contents.len());
        out.extend_from_slice(&self.to_bytes());
        out.extend_from_slice(contents);
        out
    }
}

/// Metadata announcing a file transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileOffer {
    /// Identifies this transfer across its chunk stream.
    pub transfer_id: u64,
    /// File name with no path component — the receiver chooses the directory.
    pub name: String,
    /// Total size in bytes.
    pub size: u64,
    /// BLAKE3 hash, hex encoded, for end-to-end integrity verification.
    pub blake3: String,
    /// Unix mtime in seconds, preserved when the receiving platform allows.
    pub modified_secs: Option<i64>,
}

impl FileOffer {
    /// Reject names that would let a sender escape the download directory.
    ///
    /// Path separators, `..`, absolute paths, NUL bytes and Windows drive
    /// prefixes are all refused; the receiver is expected to call this before
    /// touching the filesystem.
    #[must_use]
    pub fn has_safe_name(&self) -> bool {
        let n = &self.name;
        !n.is_empty()
            && n.len() <= 255
            && n != "."
            && n != ".."
            && !n.contains('/')
            && !n.contains('\\')
            && !n.contains('\0')
            && !n.starts_with('.')
            && !(n.len() >= 2 && n.as_bytes()[1] == b':')
    }
}

/// Serialized size of [`FileChunkHeader`].
pub const FILE_CHUNK_HEADER_LEN: usize = 16;

/// Binary header prefixing each chunk of file data.
///
/// Layout (little-endian, 16 bytes): `transfer_id(8) offset(8)`. The chunk
/// length is implied by the record length, so no length field is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileChunkHeader {
    /// Which transfer this chunk belongs to.
    pub transfer_id: u64,
    /// Byte offset of this chunk within the file.
    pub offset: u64,
}

impl FileChunkHeader {
    /// Serialize into the fixed layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; FILE_CHUNK_HEADER_LEN] {
        let mut out = [0u8; FILE_CHUNK_HEADER_LEN];
        out[0..8].copy_from_slice(&self.transfer_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out
    }

    /// Parse the header and return it with the chunk data that follows.
    pub fn split(payload: &[u8]) -> Result<(Self, &[u8])> {
        let mut cursor = payload;
        let transfer_id = read_u64(&mut cursor)?;
        let offset = read_u64(&mut cursor)?;
        Ok((
            Self {
                transfer_id,
                offset,
            },
            cursor,
        ))
    }
}

/// Flow-control acknowledgement, letting the sender bound in-flight data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileAck {
    /// Which transfer is being acknowledged.
    pub transfer_id: u64,
    /// Bytes contiguously received so far.
    pub received: u64,
    /// Additional bytes the receiver is willing to accept.
    pub window: u32,
}

impl FileAck {
    /// Serialized size of a [`FileAck`].
    pub const LEN: usize = 20;

    /// Serialize into the fixed layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[0..8].copy_from_slice(&self.transfer_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.received.to_le_bytes());
        out[16..20].copy_from_slice(&self.window.to_le_bytes());
        out
    }

    /// Parse from a payload.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut cursor = payload;
        let transfer_id = read_u64(&mut cursor)?;
        let received = read_u64(&mut cursor)?;
        let window = read_u32(&mut cursor)?;
        if window == 0 {
            return Err(ProtoError::InvalidValue {
                field: "window",
                value: "0".into(),
            });
        }
        Ok(Self {
            transfer_id,
            received,
            window,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(name: &str) -> FileOffer {
        FileOffer {
            transfer_id: 1,
            name: name.into(),
            size: 10,
            blake3: "ab".into(),
            modified_secs: None,
        }
    }

    #[test]
    fn path_traversal_names_are_rejected() {
        assert!(offer("report.pdf").has_safe_name());
        assert!(!offer("../../etc/passwd").has_safe_name());
        assert!(!offer("/etc/passwd").has_safe_name());
        assert!(!offer("C:\\Windows\\System32\\evil.dll").has_safe_name());
        assert!(!offer("sub/dir.txt").has_safe_name());
        assert!(!offer("..").has_safe_name());
        assert!(!offer(".ssh").has_safe_name());
        assert!(!offer("").has_safe_name());
        assert!(!offer("nul\0byte").has_safe_name());
    }

    #[test]
    fn chunk_header_roundtrips() {
        let h = FileChunkHeader {
            transfer_id: 0xdead_beef,
            offset: 4096,
        };
        let mut payload = h.to_bytes().to_vec();
        payload.extend_from_slice(b"data");
        let (parsed, data) = FileChunkHeader::split(&payload).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(data, b"data");
    }

    #[test]
    fn file_ack_roundtrips_and_rejects_zero_window() {
        let ack = FileAck {
            transfer_id: 7,
            received: 1024,
            window: 65536,
        };
        assert_eq!(FileAck::decode(&ack.to_bytes()).unwrap(), ack);

        let zero = FileAck { window: 0, ..ack };
        assert!(FileAck::decode(&zero.to_bytes()).is_err());
    }

    #[test]
    fn clipboard_data_roundtrips_with_its_contents() {
        let header = ClipboardDataHeader {
            offer_id: 9,
            format: ClipboardFormat::Png,
        };
        let payload = header.payload(&[0x89, b'P', b'N', b'G']);
        let (parsed, contents) = ClipboardDataHeader::split(&payload).unwrap();
        assert_eq!(parsed, header);
        assert_eq!(contents, [0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn clearing_the_clipboard_is_not_an_error() {
        // Copying nothing is a real thing to do, and it has to be able to
        // cross: otherwise a cleared clipboard on one side stays stale on
        // the other.
        let header = ClipboardDataHeader {
            offer_id: 0,
            format: ClipboardFormat::Text,
        };
        let payload = header.payload(&[]);
        let (parsed, contents) = ClipboardDataHeader::split(&payload).unwrap();
        assert_eq!(parsed.format, ClipboardFormat::Text);
        assert!(contents.is_empty());
    }

    #[test]
    fn a_truncated_clipboard_header_is_refused_rather_than_guessed() {
        let header = ClipboardDataHeader {
            offer_id: 1,
            format: ClipboardFormat::Text,
        };
        let payload = header.payload(b"hello");
        assert!(ClipboardDataHeader::split(&payload[..7]).is_err());
    }

    #[test]
    fn an_unknown_format_is_an_error_not_a_guess() {
        let mut payload = ClipboardDataHeader {
            offer_id: 1,
            format: ClipboardFormat::Text,
        }
        .payload(b"hi");
        payload[8] = 99;
        assert!(ClipboardDataHeader::split(&payload).is_err());
    }

    #[test]
    fn clipboard_offer_roundtrips_as_json() {
        let o = ClipboardOffer {
            offer_id: 3,
            formats: vec![ClipboardFormat::Png, ClipboardFormat::Text],
            size_hint: 40_000_000,
        };
        let json = serde_json::to_vec(&o).unwrap();
        assert_eq!(serde_json::from_slice::<ClipboardOffer>(&json).unwrap(), o);
    }
}
