//! Capability negotiation types exchanged in `Hello` / `HelloAck`.

use serde::{Deserialize, Serialize};

/// Video codecs NDP can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    /// H.265 / HEVC — default when both ends have hardware support.
    Hevc,
    /// H.264 / AVC — the universal fallback.
    H264,
    /// AV1 — preferred on hardware that has an AV1 encoder.
    Av1,
}

impl VideoCodec {
    /// Preference order used when intersecting capabilities.
    pub const PREFERENCE: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::Hevc, VideoCodec::H264];
}

/// Audio codecs NDP can carry. Opus only — it is the best low-latency choice
/// and is available on every target platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    /// Opus, 48 kHz, 20 ms frames, in-band FEC enabled.
    Opus,
}

/// Geometry of one display or one published-application window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DisplayGeometry {
    /// Logical width in points.
    pub width: u32,
    /// Logical height in points.
    pub height: u32,
    /// Backing-scale factor (2.0 on Retina / 200% DPI displays).
    pub scale: f32,
    /// Target refresh rate in Hz.
    pub refresh_hz: u32,
}

impl DisplayGeometry {
    /// Physical pixel dimensions implied by `scale`.
    #[must_use]
    pub fn pixel_size(&self) -> (u32, u32) {
        (
            (self.width as f32 * self.scale).round() as u32,
            (self.height as f32 * self.scale).round() as u32,
        )
    }

    /// A geometry is usable only if it has non-zero extent and a sane scale.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= 16_384
            && self.height <= 16_384
            && self.scale >= 0.5
            && self.scale <= 4.0
            && self.refresh_hz > 0
            && self.refresh_hz <= 480
    }
}

/// Audio stream parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioParams {
    /// Sample rate in Hz. Opus is always negotiated at 48000.
    pub sample_rate: u32,
    /// Channel count (1 = mono, 2 = stereo).
    pub channels: u8,
    /// Encoder frame duration in milliseconds.
    pub frame_ms: u8,
}

impl Default for AudioParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            frame_ms: 20,
        }
    }
}

/// Colour pipeline capabilities. Reserved for HDR support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorCaps {
    /// Bits per component (8 for NV12, 10 for P010).
    pub bit_depth: u8,
    /// True for full-range (0-255), false for video-range (16-235).
    pub full_range: bool,
    /// True when the sender can produce a PQ/HLG transfer function.
    pub hdr: bool,
}

impl Default for ColorCaps {
    fn default() -> Self {
        Self {
            bit_depth: 8,
            full_range: false,
            hdr: false,
        }
    }
}

/// Optional features a peer supports. Gated further by server-side policy in
/// the session ticket, so a capable client can still be denied clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FeatureFlags(pub u32);

impl FeatureFlags {
    /// No optional features.
    pub const NONE: Self = Self(0);
    /// Bidirectional clipboard synchronisation.
    pub const CLIPBOARD: Self = Self(1 << 0);
    /// Bidirectional file transfer.
    pub const FILE_TRANSFER: Self = Self(1 << 1);
    /// More than one remote display.
    pub const MULTI_MONITOR: Self = Self(1 << 2);
    /// Client microphone forwarded to the agent.
    pub const AUDIO_INPUT: Self = Self(1 << 3);
    /// Agent renders the cursor out-of-band so the client can draw it locally
    /// without waiting for a video frame.
    pub const CURSOR_OVERLAY: Self = Self(1 << 4);
    /// Peer is willing to attempt a direct (non-relayed) path.
    pub const DIRECT_UPGRADE: Self = Self(1 << 5);
    /// Isolated application surfaces, never desktop capture or display input.
    pub const APPLICATION_WINDOWS: Self = Self(1 << 6);

    /// True when every bit in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Bits present in both sets — how features are negotiated.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Union of two sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for FeatureFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// One peer's capabilities. Sent by the client as `Hello` and returned by the
/// agent as `HelloAck` holding the *negotiated* subset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    /// Accepted video codecs, most preferred first.
    pub video_codecs: Vec<VideoCodec>,
    /// Accepted audio codecs, most preferred first.
    pub audio_codecs: Vec<AudioCodec>,
    /// Displays the client wants (the agent mirrors this geometry).
    pub displays: Vec<DisplayGeometry>,
    /// Audio parameters.
    pub audio: AudioParams,
    /// Colour pipeline.
    pub color: ColorCaps,
    /// Upper bound on total session bitrate.
    pub max_bitrate_bps: u32,
    /// Optional features.
    pub features: FeatureFlags,
}

impl Caps {
    /// Intersect two capability sets, producing what the session will use.
    ///
    /// Codec lists are intersected preserving *this* side's preference order,
    /// geometry comes from the client (`self`), and numeric limits take the
    /// minimum. Returns `None` when there is no common video codec, which is
    /// a fatal negotiation failure.
    #[must_use]
    pub fn negotiate(&self, peer: &Caps) -> Option<Caps> {
        let video_codecs: Vec<_> = self
            .video_codecs
            .iter()
            .copied()
            .filter(|c| peer.video_codecs.contains(c))
            .collect();
        if video_codecs.is_empty() {
            return None;
        }
        let audio_codecs: Vec<_> = self
            .audio_codecs
            .iter()
            .copied()
            .filter(|c| peer.audio_codecs.contains(c))
            .collect();

        Some(Caps {
            video_codecs,
            audio_codecs,
            displays: self.displays.clone(),
            audio: AudioParams {
                sample_rate: self.audio.sample_rate.min(peer.audio.sample_rate),
                channels: self.audio.channels.min(peer.audio.channels),
                frame_ms: self.audio.frame_ms.max(peer.audio.frame_ms),
            },
            color: ColorCaps {
                bit_depth: self.color.bit_depth.min(peer.color.bit_depth),
                full_range: self.color.full_range && peer.color.full_range,
                hdr: self.color.hdr && peer.color.hdr,
            },
            max_bitrate_bps: self.max_bitrate_bps.min(peer.max_bitrate_bps),
            features: self.features.intersect(peer.features),
        })
    }

    /// The chosen video codec, i.e. the first surviving preference.
    #[must_use]
    pub fn primary_video_codec(&self) -> Option<VideoCodec> {
        self.video_codecs.first().copied()
    }

    /// Reject capability sets that cannot produce a working session.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.video_codecs.is_empty()
            && !self.displays.is_empty()
            && self.displays.iter().all(DisplayGeometry::is_valid)
            && self.max_bitrate_bps >= 100_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry() -> DisplayGeometry {
        DisplayGeometry {
            width: 1920,
            height: 1080,
            scale: 2.0,
            refresh_hz: 60,
        }
    }

    fn client_caps() -> Caps {
        Caps {
            video_codecs: vec![VideoCodec::Hevc, VideoCodec::H264],
            audio_codecs: vec![AudioCodec::Opus],
            displays: vec![geometry()],
            audio: AudioParams::default(),
            color: ColorCaps::default(),
            max_bitrate_bps: 40_000_000,
            features: FeatureFlags::CLIPBOARD | FeatureFlags::FILE_TRANSFER,
        }
    }

    #[test]
    fn negotiate_keeps_local_preference_order() {
        let client = client_caps();
        let agent = Caps {
            video_codecs: vec![VideoCodec::H264, VideoCodec::Hevc],
            max_bitrate_bps: 20_000_000,
            features: FeatureFlags::CLIPBOARD,
            ..client_caps()
        };
        let out = client.negotiate(&agent).unwrap();
        assert_eq!(out.video_codecs, vec![VideoCodec::Hevc, VideoCodec::H264]);
        assert_eq!(out.primary_video_codec(), Some(VideoCodec::Hevc));
        assert_eq!(out.max_bitrate_bps, 20_000_000);
        assert!(out.features.contains(FeatureFlags::CLIPBOARD));
        assert!(!out.features.contains(FeatureFlags::FILE_TRANSFER));
    }

    #[test]
    fn negotiate_fails_without_a_common_video_codec() {
        let client = Caps {
            video_codecs: vec![VideoCodec::Av1],
            ..client_caps()
        };
        let agent = Caps {
            video_codecs: vec![VideoCodec::H264],
            ..client_caps()
        };
        assert!(client.negotiate(&agent).is_none());
    }

    #[test]
    fn application_support_requires_both_peers_to_opt_in() {
        let old = client_caps();
        let mut new = client_caps();
        new.features = new.features | FeatureFlags::APPLICATION_WINDOWS;
        assert!(!old
            .negotiate(&new)
            .unwrap()
            .features
            .contains(FeatureFlags::APPLICATION_WINDOWS));
        assert!(new
            .negotiate(&new)
            .unwrap()
            .features
            .contains(FeatureFlags::APPLICATION_WINDOWS));
    }

    #[test]
    fn geometry_validation_rejects_nonsense() {
        assert!(geometry().is_valid());
        assert!(!DisplayGeometry {
            width: 0,
            ..geometry()
        }
        .is_valid());
        assert!(!DisplayGeometry {
            scale: 12.0,
            ..geometry()
        }
        .is_valid());
        assert!(!DisplayGeometry {
            refresh_hz: 0,
            ..geometry()
        }
        .is_valid());
    }

    #[test]
    fn pixel_size_applies_scale() {
        assert_eq!(geometry().pixel_size(), (3840, 2160));
    }

    #[test]
    fn caps_survive_json_roundtrip() {
        let caps = client_caps();
        let json = serde_json::to_vec(&caps).unwrap();
        let back: Caps = serde_json::from_slice(&json).unwrap();
        assert_eq!(caps, back);
    }
}
