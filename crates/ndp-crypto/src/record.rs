//! The AEAD record layer that carries NDP messages once Noise has run.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
///
/// Cheap to clone and usable from `&self`, because the media, audio and input
/// paths all seal concurrently from different tasks. Each channel owns an
/// independent atomic counter, so a large video frame's AEAD pass never blocks
/// an audio packet.
#[derive(Clone)]
pub struct RecordSealer {
    cipher: Arc<ChaCha20Poly1305>,
    counters: Arc<[AtomicU64; CHANNEL_COUNT]>,
}

impl RecordSealer {
    fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Arc::new(ChaCha20Poly1305::new(Key::from_slice(key))),
            counters: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }

    /// The sequence number the next record on `channel` will use.
    #[must_use]
    pub fn next_seq(&self, channel: Channel) -> u64 {
        self.counters[channel.id() as usize].load(Ordering::Relaxed)
    }

    /// Seal `payload` into a complete wire record.
    ///
    /// The returned buffer is `MsgHeader (cleartext) ‖ ciphertext ‖ tag`. The
    /// header is authenticated as associated data, so a record cannot be
    /// replayed under a different kind, flag set or timestamp.
    ///
    /// `header.seq` is overwritten with this channel's next sequence number —
    /// callers must not try to pick it themselves.
    pub fn seal(&self, channel: Channel, mut header: MsgHeader, payload: &[u8]) -> Result<Vec<u8>> {
        let idx = channel.id() as usize;
        // Reserve the sequence up front: two concurrent sealers on the same
        // channel must never be handed the same nonce.
        let seq = self.counters[idx].fetch_add(1, Ordering::Relaxed);
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

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(&aad);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }
}

impl std::fmt::Debug for RecordSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordSealer")
            .field("next_control_seq", &self.next_seq(Channel::Control))
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct ChannelState {
    expander: SeqExpander,
    window: ReplayWindow,
}

/// Opens inbound records, reconstructing sequence numbers and rejecting
/// replays per channel.
///
/// Like [`RecordSealer`] this is clonable and works from `&self`. Replay state
/// is guarded per channel rather than globally: video arrives on many
/// concurrent streams and must not serialise behind audio.
#[derive(Clone)]
pub struct RecordOpener {
    cipher: Arc<ChaCha20Poly1305>,
    channels: Arc<[Mutex<ChannelState>; CHANNEL_COUNT]>,
}

impl RecordOpener {
    fn new(key: &[u8; 32]) -> Self {
        let channels = std::array::from_fn(|i| {
            // Control and Input each ride a single ordered stream, so their
            // sequences must increase strictly. Every other channel is
            // delivered over concurrent streams or datagrams where genuine
            // reordering happens, and needs a sliding window instead.
            let window = match Channel::from_id(i as u32) {
                Ok(Channel::Control) | Ok(Channel::Input) => ReplayWindow::strict(),
                _ => ReplayWindow::new(),
            };
            Mutex::new(ChannelState {
                expander: SeqExpander::new(),
                window,
            })
        });
        Self {
            cipher: Arc::new(ChaCha20Poly1305::new(Key::from_slice(key))),
            channels: Arc::new(channels),
        }
    }

    /// Open a complete wire record, returning its header and plaintext.
    ///
    /// Replay state is only advanced after the AEAD tag verifies, so a forged
    /// record cannot desynchronise the sequence estimator.
    pub fn open(&self, channel: Channel, record: &[u8]) -> Result<(MsgHeader, Vec<u8>)> {
        if record.len() < HEADER_LEN + AEAD_TAG_LEN {
            return Err(CryptoError::ShortRecord(record.len()));
        }
        let (header, ciphertext) = MsgHeader::split(record)?;
        let idx = channel.id() as usize;
        let mut state = self.channels[idx]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let seq = state.expander.expand(header.seq);

        if !state.window.would_accept(seq) {
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
        if !state.window.accept(seq) {
            return Err(CryptoError::Replay {
                channel: channel.name(),
                seq,
            });
        }
        state.expander.commit(seq);

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
        let (sealer, opener) = pair();
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
        let (sealer, opener) = pair();
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
        let (sealer, opener) = pair();
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
        let (sealer, opener) = pair();
        let record = sealer
            .seal(
                Channel::Input,
                MsgHeader::new(MsgKind::InputEvent, 0, 0),
                b"e",
            )
            .unwrap();
        assert!(opener.open(Channel::Input, &record).is_ok());
        assert!(matches!(
            opener.open(Channel::Input, &record),
            Err(CryptoError::Replay { .. })
        ));
    }

    #[test]
    fn a_record_cannot_be_moved_to_another_channel() {
        let (sealer, opener) = pair();
        let record = sealer
            .seal(
                Channel::Video,
                MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                b"v",
            )
            .unwrap();
        // The channel is folded into the nonce, so cross-channel injection fails.
        assert!(matches!(
            opener.open(Channel::Audio, &record),
            Err(CryptoError::Decrypt { .. })
        ));
    }

    #[test]
    fn channels_have_independent_sequence_spaces() {
        let (sealer, opener) = pair();
        for _ in 0..3 {
            let v = sealer
                .seal(
                    Channel::Video,
                    MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                    b"v",
                )
                .unwrap();
            let a = sealer
                .seal(
                    Channel::Audio,
                    MsgHeader::new(MsgKind::AudioFrame, 0, 0),
                    b"a",
                )
                .unwrap();
            assert!(opener.open(Channel::Video, &v).is_ok());
            assert!(opener.open(Channel::Audio, &a).is_ok());
        }
        assert_eq!(sealer.next_seq(Channel::Video), 3);
        assert_eq!(sealer.next_seq(Channel::Audio), 3);
        assert_eq!(sealer.next_seq(Channel::Input), 0);
    }

    #[test]
    fn concurrently_delivered_channels_tolerate_reordering() {
        // Audio rides datagrams and video rides one stream per frame, so both
        // can arrive out of order and must still be accepted exactly once.
        for (channel, kind) in [
            (Channel::Audio, MsgKind::AudioFrame),
            (Channel::Video, MsgKind::VideoFrame),
        ] {
            let (sealer, opener) = pair();
            let r0 = sealer
                .seal(channel, MsgHeader::new(kind, 0, 0), b"0")
                .unwrap();
            let r1 = sealer
                .seal(channel, MsgHeader::new(kind, 0, 0), b"1")
                .unwrap();
            assert!(opener.open(channel, &r1).is_ok());
            assert!(opener.open(channel, &r0).is_ok());
            assert!(opener.open(channel, &r0).is_err(), "replay must still fail");
        }
    }

    #[test]
    fn ordered_channels_reject_reordering() {
        // Control and Input each ride a single ordered stream, so anything
        // arriving out of order is either a bug or an attack.
        for (channel, kind) in [
            (Channel::Control, MsgKind::Ping),
            (Channel::Input, MsgKind::InputEvent),
        ] {
            let (sealer, opener) = pair();
            let r0 = sealer
                .seal(channel, MsgHeader::new(kind, 0, 0), b"0")
                .unwrap();
            let r1 = sealer
                .seal(channel, MsgHeader::new(kind, 0, 0), b"1")
                .unwrap();
            assert!(opener.open(channel, &r1).is_ok());
            assert!(matches!(
                opener.open(channel, &r0),
                Err(CryptoError::Replay { .. })
            ));
        }
    }

    #[test]
    fn concurrent_sealers_never_reuse_a_sequence() {
        let (sealer, opener) = pair();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let sealer = sealer.clone();
            handles.push(std::thread::spawn(move || {
                (0..64)
                    .map(|_| {
                        sealer
                            .seal(
                                Channel::Video,
                                MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                                b"f",
                            )
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            }));
        }
        let mut records: Vec<(u32, Vec<u8>)> = Vec::new();
        for h in handles {
            for record in h.join().unwrap() {
                let (header, _) = MsgHeader::split(&record).unwrap();
                records.push((header.seq, record));
            }
        }

        let mut seqs: Vec<u32> = records.iter().map(|(s, _)| *s).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs.len(),
            8 * 64,
            "concurrent sealers must never reuse a nonce"
        );
        assert_eq!(sealer.next_seq(Channel::Video), 8 * 64);

        // Delivered within the replay window, every record opens exactly once.
        records.sort_by_key(|(s, _)| *s);
        for (_, record) in &records {
            assert!(opener.open(Channel::Video, record).is_ok());
        }
    }

    #[test]
    fn short_records_are_rejected_before_decryption() {
        let (_, opener) = pair();
        assert!(matches!(
            opener.open(Channel::Control, &[0u8; 4]),
            Err(CryptoError::ShortRecord(4))
        ));
    }

    #[test]
    fn different_keys_cannot_read_each_other() {
        let (sealer, _) = pair();
        let (_, foreign) = SessionKeys {
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
