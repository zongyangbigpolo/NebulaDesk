//! The AEAD record layer that carries NDP messages once Noise has run.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use ndp_proto::{Channel, MsgHeader, HEADER_LEN};
use zeroize::Zeroize;

use crate::replay::{ReplayWindow, SeqExpander};
use crate::{CryptoError, Result};

/// Size of the ChaCha20-Poly1305 authentication tag.
pub const AEAD_TAG_LEN: usize = 16;

const CHANNEL_COUNT: usize = 6;

/// The two directional traffic keys produced by the handshake.
///
/// `send` and `recv` are swapped between the two peers, so each direction has
/// an independent key and nonce space.
pub struct SessionKeys {
    /// Key used to seal records this peer sends.
    pub send: [u8; 32],
    /// Key used to open records this peer receives.
    pub recv: [u8; 32],
}

impl SessionKeys {
    /// Split into a sealer and an opener, consuming the keys.
    #[must_use]
    pub fn split(self) -> (RecordSealer, RecordOpener) {
        let sealer = RecordSealer::new(&self.send);
        let opener = RecordOpener::new(&self.recv);
        (sealer, opener)
    }
}

impl Drop for SessionKeys {
    fn drop(&mut self) {
        self.send.zeroize();
        self.recv.zeroize();
    }
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKeys(redacted)")
    }
}

/// Build the 12-byte nonce: `channel_id (u32 LE) ‖ sequence (u64 LE)`.
///
/// Folding the channel into the nonce lets every channel keep its own
/// sequence space without any risk of nonce reuse across channels.
fn nonce_for(channel: Channel, seq: u64) -> Nonce {
    let mut raw = [0u8; 12];
    raw[0..4].copy_from_slice(&channel.id().to_le_bytes());
    raw[4..12].copy_from_slice(&seq.to_le_bytes());
    *Nonce::from_slice(&raw)
}

/// Seals outbound records, tracking one sequence counter per channel.
pub struct RecordSealer {
    cipher: ChaCha20Poly1305,
    counters: [u64; CHANNEL_COUNT],
}

impl RecordSealer {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            counters: [0; CHANNEL_COUNT],
        }
    }

    /// The sequence number the next record on `channel` will use.
    #[must_use]
    pub fn next_seq(&self, channel: Channel) -> u64 {
        self.counters[channel.id() as usize]
    }

    /// Seal `payload` into a complete wire record.
    ///
    /// The returned buffer is `MsgHeader (cleartext) ‖ ciphertext ‖ tag`. The
    /// header is authenticated as associated data, so a record cannot be
    /// replayed under a different kind, flag set or timestamp.
    ///
    /// `header.seq` is overwritten with this channel's next sequence number —
    /// callers must not try to pick it themselves.
    pub fn seal(
        &mut self,
        channel: Channel,
        mut header: MsgHeader,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        let idx = channel.id() as usize;
        let seq = self.counters[idx];
        header.seq = seq as u32;
        let aad = header.to_bytes();

        let ciphertext = self
            .cipher
            .encrypt(
                &nonce_for(channel, seq),
                Payload {
                    msg: payload,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt {
                channel: channel.name(),
            })?;

        self.counters[idx] = seq.wrapping_add(1);

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(&aad);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }
}

impl std::fmt::Debug for RecordSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordSealer")
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

/// Opens inbound records, reconstructing sequence numbers and rejecting
/// replays per channel.
pub struct RecordOpener {
    cipher: ChaCha20Poly1305,
    expanders: [SeqExpander; CHANNEL_COUNT],
    windows: [ReplayWindow; CHANNEL_COUNT],
}

impl RecordOpener {
    fn new(key: &[u8; 32]) -> Self {
        let windows = std::array::from_fn(|i| {
            // Reliable, ordered channels can demand strictly increasing
            // sequences; only the datagram channel needs a sliding window.
            match Channel::from_id(i as u32) {
                Ok(ch) if ch.is_unreliable() => ReplayWindow::new(),
                _ => ReplayWindow::strict(),
            }
        });
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            expanders: std::array::from_fn(|_| SeqExpander::new()),
            windows,
        }
    }

    /// Open a complete wire record, returning its header and plaintext.
    ///
    /// Replay state is only advanced after the AEAD tag verifies, so a forged
    /// record cannot desynchronise the sequence estimator.
    pub fn open(&mut self, channel: Channel, record: &[u8]) -> Result<(MsgHeader, Vec<u8>)> {
        if record.len() < HEADER_LEN + AEAD_TAG_LEN {
            return Err(CryptoError::ShortRecord(record.len()));
        }
        let (header, ciphertext) = MsgHeader::split(record)?;
        let idx = channel.id() as usize;
        let seq = self.expanders[idx].expand(header.seq);

        if !self.windows[idx].would_accept(seq) {
            return Err(CryptoError::Replay {
                channel: channel.name(),
                seq,
            });
        }

        let aad = header.to_bytes();
        let plaintext = self
            .cipher
            .decrypt(
                &nonce_for(channel, seq),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| CryptoError::Decrypt {
                channel: channel.name(),
            })?;

        // Authenticated: it is now safe to commit sequence state.
        if !self.windows[idx].accept(seq) {
            return Err(CryptoError::Replay {
                channel: channel.name(),
                seq,
            });
        }
        self.expanders[idx].commit(seq);

        Ok((header, plaintext))
    }
}

impl std::fmt::Debug for RecordOpener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordOpener(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndp_proto::{MsgFlags, MsgKind};

    fn pair() -> (RecordSealer, RecordOpener) {
        SessionKeys {
            send: [7u8; 32],
            recv: [7u8; 32],
        }
        .split()
    }

    #[test]
    fn seal_and_open_roundtrip() {
        let (mut sealer, mut opener) = pair();
        let header = MsgHeader::new(MsgKind::VideoFrame, 0, 42).with_flags(MsgFlags::KEYFRAME);
        let record = sealer.seal(Channel::Video, header, b"frame data").unwrap();

        let (out_header, plaintext) = opener.open(Channel::Video, &record).unwrap();
        assert_eq!(plaintext, b"frame data");
        assert_eq!(out_header.kind, MsgKind::VideoFrame);
        assert!(out_header.flags.contains(MsgFlags::KEYFRAME));
        assert_eq!(out_header.timestamp_us, 42);
    }

    #[test]
    fn header_is_cleartext_but_authenticated() {
        let (mut sealer, mut opener) = pair();
        let header = MsgHeader::new(MsgKind::VideoFrame, 0, 42);
        let mut record = sealer.seal(Channel::Video, header, b"payload").unwrap();

        // A relay can read the header...
        let (visible, _) = MsgHeader::split(&record).unwrap();
        assert_eq!(visible.kind, MsgKind::VideoFrame);

        // ...but cannot alter it.
        record[2] = 0xff;
        assert!(matches!(
            opener.open(Channel::Video, &record),
            Err(CryptoError::Decrypt { .. })
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (mut sealer, mut opener) = pair();
        let mut record = sealer
            .seal(Channel::Control, MsgHeader::new(MsgKind::Ping, 0, 0), b"x")
            .unwrap();
        let last = record.len() - 1;
        record[last] ^= 0x01;
        assert!(matches!(
            opener.open(Channel::Control, &record),
            Err(CryptoError::Decrypt { .. })
        ));
    }

    #[test]
    fn replaying_a_record_is_rejected() {
        let (mut sealer, mut opener) = pair();
        let record = sealer
            .seal(Channel::Input, MsgHeader::new(MsgKind::InputEvent, 0, 0), b"e")
            .unwrap();
        assert!(opener.open(Channel::Input, &record).is_ok());
        assert!(matches!(
            opener.open(Channel::Input, &record),
            Err(CryptoError::Replay { .. })
        ));
    }

    #[test]
    fn a_record_cannot_be_moved_to_another_channel() {
        let (mut sealer, mut opener) = pair();
        let record = sealer
            .seal(Channel::Video, MsgHeader::new(MsgKind::VideoFrame, 0, 0), b"v")
            .unwrap();
        // The channel is folded into the nonce, so cross-channel injection fails.
        assert!(matches!(
            opener.open(Channel::Audio, &record),
            Err(CryptoError::Decrypt { .. })
        ));
    }

    #[test]
    fn channels_have_independent_sequence_spaces() {
        let (mut sealer, mut opener) = pair();
        for _ in 0..3 {
            let v = sealer
                .seal(Channel::Video, MsgHeader::new(MsgKind::VideoFrame, 0, 0), b"v")
                .unwrap();
            let a = sealer
                .seal(Channel::Audio, MsgHeader::new(MsgKind::AudioFrame, 0, 0), b"a")
                .unwrap();
            assert!(opener.open(Channel::Video, &v).is_ok());
            assert!(opener.open(Channel::Audio, &a).is_ok());
        }
        assert_eq!(sealer.next_seq(Channel::Video), 3);
        assert_eq!(sealer.next_seq(Channel::Audio), 3);
        assert_eq!(sealer.next_seq(Channel::Input), 0);
    }

    #[test]
    fn audio_tolerates_reordering_but_video_does_not() {
        let (mut sealer, mut opener) = pair();
        let a0 = sealer
            .seal(Channel::Audio, MsgHeader::new(MsgKind::AudioFrame, 0, 0), b"0")
            .unwrap();
        let a1 = sealer
            .seal(Channel::Audio, MsgHeader::new(MsgKind::AudioFrame, 0, 0), b"1")
            .unwrap();
        // Deliver out of order: the datagram channel accepts both.
        assert!(opener.open(Channel::Audio, &a1).is_ok());
        assert!(opener.open(Channel::Audio, &a0).is_ok());
    }

    #[test]
    fn short_records_are_rejected_before_decryption() {
        let (_, mut opener) = pair();
        assert!(matches!(
            opener.open(Channel::Control, &[0u8; 4]),
            Err(CryptoError::ShortRecord(4))
        ));
    }

    #[test]
    fn different_keys_cannot_read_each_other() {
        let (mut sealer, _) = pair();
        let (_, mut foreign) = SessionKeys {
            send: [9u8; 32],
            recv: [9u8; 32],
        }
        .split();
        let record = sealer
            .seal(Channel::Control, MsgHeader::new(MsgKind::Ping, 0, 0), b"p")
            .unwrap();
        assert!(foreign.open(Channel::Control, &record).is_err());
    }
}
