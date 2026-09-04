//! Carrier framing.
//!
//! Two shapes exist and both start with the same 4-byte prefix: a
//! little-endian `u16` channel id, then a one-byte urgency, then a reserved
//! byte. The urgency is deliberately in the clear: the relay copies streams
//! it cannot read, and without it every stream would be forwarded at the same
//! standing and the scheduling decided at each end would be undone in the
//! middle.
//!
//! * **Per-message carriers** (video frames, file/clipboard transfers,
//!   datagrams) hold exactly one sealed record; the stream FIN or the datagram
//!   boundary delimits it, so no length prefix is written.
//! * **Long-lived carriers** (control, input) interleave many records on one
//!   ordered stream and prefix each with a 4-byte little-endian length.

use ndp_proto::Channel;

use crate::{Result, TransportError};

/// Bytes of channel id and urgency at the head of every carrier.
pub(crate) const CHANNEL_PREFIX_LEN: usize = 4;
/// Bytes of length prefix in front of each record on a long-lived carrier.
pub(crate) const LENGTH_PREFIX_LEN: usize = 4;

pub(crate) fn channel_prefix(channel: Channel, urgency: u8) -> [u8; CHANNEL_PREFIX_LEN] {
    let id = (channel.id() as u16).to_le_bytes();
    [id[0], id[1], urgency, 0]
}

pub(crate) fn parse_channel(bytes: &[u8; CHANNEL_PREFIX_LEN]) -> Result<Channel> {
    let id = u32::from(u16::from_le_bytes([bytes[0], bytes[1]]));
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
            assert_eq!(parse_channel(&channel_prefix(ch, 7)).unwrap(), ch);
        }
    }

    #[test]
    fn urgency_travels_beside_the_channel_and_is_readable_without_the_key() {
        let prefix = channel_prefix(Channel::Video, 25);
        assert_eq!(ndp_transport_urgency(&prefix), 25);
        assert_eq!(parse_channel(&prefix).unwrap(), Channel::Video);
    }

    /// The relay reads urgency straight out of the prefix; this mirrors it.
    fn ndp_transport_urgency(prefix: &[u8; CHANNEL_PREFIX_LEN]) -> u8 {
        prefix[2]
    }

    #[test]
    fn unknown_channel_prefix_is_rejected() {
        assert!(matches!(
            parse_channel(&[99, 0, 0, 0]),
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
