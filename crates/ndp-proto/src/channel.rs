//! Logical channels and their QUIC carriers.

use crate::{ProtoError, Result};

/// A logical NDP channel.
///
/// The discriminant is stable on the wire: it is folded into the AEAD nonce by
/// `ndp-crypto`, so two channels can safely reuse the same sequence numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum Channel {
    /// Capability negotiation, cursor, QoS, lifecycle. Reliable and ordered.
    Control = 0,
    /// Encoded video. One unidirectional QUIC stream per frame.
    Video = 1,
    /// Encoded audio. QUIC datagrams — unreliable by design.
    Audio = 2,
    /// Keyboard/mouse/touch events, client to agent. Reliable and ordered.
    Input = 3,
    /// Clipboard synchronisation, one stream per transfer.
    Clipboard = 4,
    /// File transfer, one stream per transfer.
    File = 5,
}

impl Channel {
    /// Every channel, in wire order.
    pub const ALL: [Channel; 6] = [
        Channel::Control,
        Channel::Video,
        Channel::Audio,
        Channel::Input,
        Channel::Clipboard,
        Channel::File,
    ];

    /// The channel's stable wire discriminant.
    #[must_use]
    pub const fn id(self) -> u32 {
        self as u32
    }

    /// Decode a wire discriminant.
    pub fn from_id(id: u32) -> Result<Self> {
        Ok(match id {
            0 => Channel::Control,
            1 => Channel::Video,
            2 => Channel::Audio,
            3 => Channel::Input,
            4 => Channel::Clipboard,
            5 => Channel::File,
            other => {
                return Err(ProtoError::InvalidValue {
                    field: "channel",
                    value: other.to_string(),
                })
            }
        })
    }

    /// Whether this channel tolerates loss (and therefore rides datagrams).
    #[must_use]
    pub const fn is_unreliable(self) -> bool {
        matches!(self, Channel::Audio)
    }

    /// Whether the channel opens a fresh stream per message rather than
    /// multiplexing everything onto one long-lived stream.
    #[must_use]
    pub const fn is_stream_per_message(self) -> bool {
        matches!(self, Channel::Video | Channel::Clipboard | Channel::File)
    }

    /// Human-readable name, used in logs and metrics labels.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Channel::Control => "control",
            Channel::Video => "video",
            Channel::Audio => "audio",
            Channel::Input => "input",
            Channel::Clipboard => "clipboard",
            Channel::File => "file",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_ids_roundtrip() {
        for ch in Channel::ALL {
            assert_eq!(Channel::from_id(ch.id()).unwrap(), ch);
        }
    }

    #[test]
    fn unknown_channel_is_rejected() {
        assert!(Channel::from_id(99).is_err());
    }

    #[test]
    fn ordered_and_per_message_channels_are_disjoint_and_total() {
        // Every channel must have exactly one carrier: unreliable (datagram),
        // per-message stream, or the long-lived ordered stream.
        for ch in Channel::ALL {
            assert!(
                !(ch.is_unreliable() && ch.is_stream_per_message()),
                "{} claims two carriers",
                ch.name()
            );
        }
        let ordered: Vec<_> = Channel::ALL
            .into_iter()
            .filter(|c| !c.is_unreliable() && !c.is_stream_per_message())
            .collect();
        assert_eq!(ordered, vec![Channel::Control, Channel::Input]);
    }

    #[test]
    fn only_audio_is_unreliable() {
        let unreliable: Vec<_> = Channel::ALL
            .into_iter()
            .filter(|c| c.is_unreliable())
            .collect();
        assert_eq!(unreliable, vec![Channel::Audio]);
    }
}
