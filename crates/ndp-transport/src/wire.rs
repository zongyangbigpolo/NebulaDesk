//! Carrier framing.
//!
//! Two shapes exist and both start with the 4-byte little-endian channel id:
//!
//! * **Per-message carriers** (video frames, file/clipboard transfers,
//!   datagrams) hold exactly one sealed record; the stream FIN or the datagram
//!   boundary delimits it, so no length prefix is written.
//! * **Long-lived carriers** (control, input) interleave many records on one
//!   ordered stream and prefix each with a 4-byte little-endian length.

use ndp_proto::Channel;

use crate::{Result, TransportError};

/// Bytes of channel id at the head of every carrier.
pub(crate) const CHANNEL_PREFIX_LEN: usize = 4;
/// Bytes of length prefix in front of each record on a long-lived carrier.
pub(crate) const LENGTH_PREFIX_LEN: usize = 4;

pub(crate) fn channel_prefix(channel: Channel) -> [u8; CHANNEL_PREFIX_LEN] {
    channel.id().to_le_bytes()
}

pub(crate) fn parse_channel(bytes: &[u8; CHANNEL_PREFIX_LEN]) -> Result<Channel> {
    let id = u32::from_le_bytes(*bytes);
    Channel::from_id(id).map_err(|_| TransportError::UnknownChannel(id))
}

/// Frame one record for a long-lived carrier: `len ‖ record`.
pub(crate) fn frame(record: &[u8], limit: usize) -> Result<Vec<u8>> {
    check_len(record.len(), limit)?;
    let mut out = Vec::with_capacity(LENGTH_PREFIX_LEN + record.len());
    out.extend_from_slice(&(record.len() as u32).to_le_bytes());
    out.extend_from_slice(record);
    Ok(out)
}

/// Validate a length prefix before it is used to size an allocation.
pub(crate) fn check_len(len: usize, limit: usize) -> Result<()> {
    if len > limit {
        return Err(TransportError::RecordTooLarge { len, limit });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_prefix_roundtrips() {
        for ch in Channel::ALL {
            assert_eq!(parse_channel(&channel_prefix(ch)).unwrap(), ch);
        }
    }

    #[test]
    fn unknown_channel_prefix_is_rejected() {
        assert!(matches!(
            parse_channel(&99u32.to_le_bytes()),
            Err(TransportError::UnknownChannel(99))
        ));
    }

    #[test]
    fn frame_prefixes_the_length() {
        let framed = frame(b"abcd", 64).unwrap();
        assert_eq!(&framed[..4], &4u32.to_le_bytes());
        assert_eq!(&framed[4..], b"abcd");
    }

    #[test]
    fn oversized_records_are_refused_before_allocating() {
        assert!(matches!(
            frame(&[0u8; 32], 8),
            Err(TransportError::RecordTooLarge { len: 32, limit: 8 })
        ));
        assert!(check_len(usize::MAX, 1024).is_err());
    }
}
