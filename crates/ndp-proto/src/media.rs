//! Binary descriptors that prefix media payloads.
//!
//! A media record is `MsgHeader ‖ AEAD( FrameInfo ‖ bitstream )`. The info
//! block is tiny and fixed-size so the receiver can route a frame to the right
//! decoder without parsing the codec bitstream, and so mid-session resolution
//! changes are self-describing.

use crate::caps::{AudioCodec, VideoCodec};
use crate::{read_u16, read_u32, read_u8, ProtoError, Result};

/// Serialized size of [`VideoFrameInfo`].
pub const VIDEO_INFO_LEN: usize = 12;

/// Serialized size of [`AudioFrameInfo`].
pub const AUDIO_INFO_LEN: usize = 4;

fn video_codec_id(c: VideoCodec) -> u8 {
    match c {
        VideoCodec::H264 => 1,
        VideoCodec::Hevc => 2,
        VideoCodec::Av1 => 3,
    }
}

fn video_codec_from_id(v: u8) -> Result<VideoCodec> {
    Ok(match v {
        1 => VideoCodec::H264,
        2 => VideoCodec::Hevc,
        3 => VideoCodec::Av1,
        other => {
            return Err(ProtoError::InvalidValue {
                field: "video codec",
                value: other.to_string(),
            })
        }
    })
}

/// Descriptor preceding an encoded video frame.
///
/// Layout (little-endian, 12 bytes): `display(1) codec(1) reserved(2)
/// width(2) height(2) duration_us(4)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFrameInfo {
    /// Which negotiated display this frame belongs to. In explicitly negotiated
    /// application mode, this is a session-unique, never-reused surface ID;
    /// decoder and recovery state must be independent for each ID.
    pub display: u8,
    /// Codec of the bitstream that follows.
    pub codec: VideoCodec,
    /// Coded width in pixels.
    pub width: u16,
    /// Coded height in pixels.
    pub height: u16,
    /// Nominal presentation duration, used by the client's pacing logic.
    pub duration_us: u32,
}

impl VideoFrameInfo {
    /// Serialize into the fixed layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; VIDEO_INFO_LEN] {
        let mut out = [0u8; VIDEO_INFO_LEN];
        out[0] = self.display;
        out[1] = video_codec_id(self.codec);
        out[4..6].copy_from_slice(&self.width.to_le_bytes());
        out[6..8].copy_from_slice(&self.height.to_le_bytes());
        out[8..12].copy_from_slice(&self.duration_us.to_le_bytes());
        out
    }

    /// Parse the descriptor and return it with the bitstream that follows.
    pub fn split(payload: &[u8]) -> Result<(Self, &[u8])> {
        let mut cursor = payload;
        let display = read_u8(&mut cursor)?;
        let codec = video_codec_from_id(read_u8(&mut cursor)?)?;
        let _reserved = read_u16(&mut cursor)?;
        let width = read_u16(&mut cursor)?;
        let height = read_u16(&mut cursor)?;
        let duration_us = read_u32(&mut cursor)?;
        if width == 0 || height == 0 {
            return Err(ProtoError::InvalidValue {
                field: "frame size",
                value: format!("{width}x{height}"),
            });
        }
        Ok((
            Self {
                display,
                codec,
                width,
                height,
                duration_us,
            },
            cursor,
        ))
    }

    /// Build a complete video payload from the descriptor and a bitstream.
    #[must_use]
    pub fn frame_payload(&self, bitstream: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(VIDEO_INFO_LEN + bitstream.len());
        out.extend_from_slice(&self.to_bytes());
        out.extend_from_slice(bitstream);
        out
    }
}

/// Descriptor preceding an encoded audio packet.
///
/// Layout (little-endian, 4 bytes): `codec(1) channels(1) frame_ms(1)
/// reserved(1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFrameInfo {
    /// Codec of the packet that follows. Opus today.
    pub codec: AudioCodec,
    /// Channel count.
    pub channels: u8,
    /// Encoder frame duration in milliseconds.
    pub frame_ms: u8,
}

impl AudioFrameInfo {
    /// Serialize into the fixed layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; AUDIO_INFO_LEN] {
        [1, self.channels, self.frame_ms, 0]
    }

    /// Parse the descriptor and return it with the packet that follows.
    pub fn split(payload: &[u8]) -> Result<(Self, &[u8])> {
        let mut cursor = payload;
        let codec = match read_u8(&mut cursor)? {
            1 => AudioCodec::Opus,
            other => {
                return Err(ProtoError::InvalidValue {
                    field: "audio codec",
                    value: other.to_string(),
                })
            }
        };
        let channels = read_u8(&mut cursor)?;
        let frame_ms = read_u8(&mut cursor)?;
        let _reserved = read_u8(&mut cursor)?;
        if channels == 0 || channels > 8 {
            return Err(ProtoError::InvalidValue {
                field: "channels",
                value: channels.to_string(),
            });
        }
        Ok((
            Self {
                codec,
                channels,
                frame_ms,
            },
            cursor,
        ))
    }

    /// Build a complete audio payload from the descriptor and a packet.
    #[must_use]
    pub fn frame_payload(&self, packet: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(AUDIO_INFO_LEN + packet.len());
        out.extend_from_slice(&self.to_bytes());
        out.extend_from_slice(packet);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_info_roundtrips_with_bitstream() {
        let info = VideoFrameInfo {
            display: 1,
            codec: VideoCodec::Hevc,
            width: 3840,
            height: 2160,
            duration_us: 16_666,
        };
        let payload = info.frame_payload(&[0xde, 0xad, 0xbe, 0xef]);
        let (parsed, bitstream) = VideoFrameInfo::split(&payload).unwrap();
        assert_eq!(parsed, info);
        assert_eq!(bitstream, &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn zero_sized_video_frame_is_rejected() {
        let info = VideoFrameInfo {
            display: 0,
            codec: VideoCodec::H264,
            width: 0,
            height: 1080,
            duration_us: 0,
        };
        assert!(VideoFrameInfo::split(&info.frame_payload(&[])).is_err());
    }

    #[test]
    fn unknown_video_codec_is_rejected() {
        let mut payload = VideoFrameInfo {
            display: 0,
            codec: VideoCodec::H264,
            width: 640,
            height: 480,
            duration_us: 0,
        }
        .frame_payload(&[]);
        payload[1] = 42;
        assert!(VideoFrameInfo::split(&payload).is_err());
    }

    #[test]
    fn audio_info_roundtrips_with_packet() {
        let info = AudioFrameInfo {
            codec: AudioCodec::Opus,
            channels: 2,
            frame_ms: 20,
        };
        let payload = info.frame_payload(&[1, 2, 3]);
        let (parsed, packet) = AudioFrameInfo::split(&payload).unwrap();
        assert_eq!(parsed, info);
        assert_eq!(packet, &[1, 2, 3]);
    }

    #[test]
    fn absurd_channel_count_is_rejected() {
        let mut payload = AudioFrameInfo {
            codec: AudioCodec::Opus,
            channels: 2,
            frame_ms: 20,
        }
        .frame_payload(&[]);
        payload[1] = 99;
        assert!(AudioFrameInfo::split(&payload).is_err());
    }

    #[test]
    fn truncated_info_is_rejected() {
        assert!(VideoFrameInfo::split(&[0, 1, 0]).is_err());
        assert!(AudioFrameInfo::split(&[1]).is_err());
    }
}
