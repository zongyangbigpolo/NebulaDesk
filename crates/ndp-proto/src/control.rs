//! Control-plane messages.
//!
//! Control traffic is low volume and long-lived across versions, so it is
//! JSON-encoded: a newer peer can add fields without breaking an older one.
//! Media and input, which are hot, use packed binary instead.

use serde::{Deserialize, Serialize};

use crate::caps::{Caps, DisplayGeometry, VideoCodec};

/// Why a session is ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByeReason {
    /// The user closed the session.
    UserClosed,
    /// Admission authority or an authenticated protocol binding was invalid.
    /// Ticket expiry and entitlement edits do not revoke an admitted logical session.
    Unauthorized,
    /// Capability negotiation found no workable configuration.
    NegotiationFailed,
    /// The agent is shutting down or draining.
    AgentShutdown,
    /// A newer session for the same resource superseded this one.
    Superseded,
    /// Idle for longer than policy permits.
    IdleTimeout,
    /// An unrecoverable internal error.
    InternalError,
}

/// Receiver-side quality telemetry, sent every ~200 ms to drive the agent's
/// adaptive bitrate controller.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QosReport {
    /// Smoothed round-trip time in microseconds.
    pub rtt_us: u32,
    /// Fraction of records lost since the previous report, `[0.0, 1.0]`.
    pub loss: f32,
    /// Inter-arrival jitter in microseconds.
    pub jitter_us: u32,
    /// Frames waiting in the decoder queue — a proxy for client overload.
    pub decode_queue: u16,
    /// Frames the renderer dropped since the previous report.
    pub dropped_frames: u16,
    /// Goodput the receiver actually observed.
    pub received_bps: u32,
    /// Video frames successfully presented since the previous report.
    pub rendered_frames: u16,
}

/// A cursor bitmap pushed out-of-band so the client can draw the pointer
/// locally at full input rate instead of waiting for the next video frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    /// Bitmap width in pixels.
    pub width: u16,
    /// Bitmap height in pixels.
    pub height: u16,
    /// Hotspot X within the bitmap.
    pub hotspot_x: u16,
    /// Hotspot Y within the bitmap.
    pub hotspot_y: u16,
    /// Premultiplied BGRA pixels, base64 encoded.
    pub bgra_base64: String,
}

/// A transport address candidate offered for the optional direct-path upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCandidate {
    /// `ip:port` in standard textual form.
    pub addr: String,
    /// How the address was learned.
    pub kind: CandidateKind,
    /// Higher wins when several candidates succeed.
    pub priority: u32,
}

/// Authenticated metadata for the session-scoped direct QUIC listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectPathBinding {
    /// Direct negotiation schema version.
    pub version: u8,
    /// Logical session UUID; the receiver must match its authorised session.
    pub session: String,
    /// Listener UUID, also bound into the direct Noise prologue.
    pub listener: String,
    /// SHA-256 fingerprint of the listener's TLS certificate.
    pub certificate_pin: String,
}

/// How a [`PathCandidate`] was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateKind {
    /// A local interface address.
    Host,
    /// The address the relay observed, i.e. the NAT mapping.
    ServerReflexive,
}

/// Everything that can travel on [`crate::Channel::Control`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlMessage {
    /// Only legal after explicit APPLICATION_WINDOWS negotiation.
    Application(crate::application::ApplicationMessage),
    /// Client to agent: offered capabilities.
    Hello {
        /// What the client can do.
        caps: Caps,
        /// Client build identifier, for diagnostics and compatibility shims.
        client_version: String,
        /// Client operating system, for keyboard-layout heuristics.
        client_os: String,
    },
    /// Agent to client: the negotiated capability set.
    HelloAck {
        /// The intersection both ends will use.
        caps: Caps,
        /// Agent build identifier.
        agent_version: String,
        /// Codec the agent's encoder actually started with.
        active_codec: VideoCodec,
    },
    /// Orderly shutdown.
    Bye {
        /// Why the session is ending.
        reason: ByeReason,
        /// Optional human-readable detail for logs.
        detail: Option<String>,
    },
    /// Liveness probe.
    Ping {
        /// Sender's monotonic clock reading, echoed back in `Pong`.
        echo_us: u64,
    },
    /// Response to [`ControlMessage::Ping`].
    Pong {
        /// The probe's `echo_us`, verbatim.
        echo_us: u64,
    },
    /// Request a mid-session change; either side may send it.
    CapsUpdate {
        /// New geometry, when the client window was resized.
        displays: Option<Vec<DisplayGeometry>>,
        /// New bitrate ceiling.
        max_bitrate_bps: Option<u32>,
        /// Ask the encoder to emit a keyframe now (e.g. after packet loss).
        request_keyframe: bool,
    },
    /// The agent's monitor arrangement changed.
    DisplayLayout {
        /// Current displays, in the order used by `InputEvent::display`.
        displays: Vec<DisplayGeometry>,
    },
    /// New pointer bitmap.
    CursorShape(CursorShape),
    /// Pointer moved without client input (the remote app moved it).
    CursorPos {
        /// Display index.
        display: u8,
        /// Normalised X.
        x: f32,
        /// Normalised Y.
        y: f32,
        /// Whether the pointer should be drawn at all.
        visible: bool,
    },
    /// Receiver quality telemetry.
    QosReport(QosReport),
    /// Address candidates for the direct-path upgrade.
    PathCandidates {
        /// Candidates offered by the sender.
        candidates: Vec<PathCandidate>,
        /// Present for mutually authenticated direct QUIC negotiation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        direct_binding: Option<DirectPathBinding>,
    },
}

impl ControlMessage {
    /// The header kind that must accompany this message.
    #[must_use]
    pub const fn kind(&self) -> crate::MsgKind {
        use crate::MsgKind as K;
        match self {
            Self::Application(_) => K::Application,
            Self::Hello { .. } => K::Hello,
            Self::HelloAck { .. } => K::HelloAck,
            Self::Bye { .. } => K::Bye,
            Self::Ping { .. } => K::Ping,
            Self::Pong { .. } => K::Pong,
            Self::CapsUpdate { .. } => K::CapsUpdate,
            Self::DisplayLayout { .. } => K::DisplayLayout,
            Self::CursorShape(_) => K::CursorShape,
            Self::CursorPos { .. } => K::CursorPos,
            Self::QosReport(_) => K::QosReport,
            Self::PathCandidates { .. } => K::PathCandidates,
        }
    }

    /// Encode the JSON payload that follows the record header.
    pub fn encode(&self) -> crate::Result<Vec<u8>> {
        if let Self::Application(message) = self {
            message.validate()?;
        }
        Ok(serde_json::to_vec(self)?)
    }

    /// Decode a JSON control payload.
    pub fn decode(payload: &[u8]) -> crate::Result<Self> {
        let message: Self = serde_json::from_slice(payload)?;
        if let Self::Application(application) = &message {
            application.validate()?;
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::{AudioCodec, AudioParams, ColorCaps, FeatureFlags};
    use crate::MsgKind;

    fn caps() -> Caps {
        Caps {
            video_codecs: vec![VideoCodec::Hevc],
            audio_codecs: vec![AudioCodec::Opus],
            displays: vec![DisplayGeometry {
                width: 2560,
                height: 1440,
                scale: 1.0,
                refresh_hz: 120,
            }],
            audio: AudioParams::default(),
            color: ColorCaps::default(),
            max_bitrate_bps: 30_000_000,
            features: FeatureFlags::CLIPBOARD,
        }
    }

    #[test]
    fn control_messages_roundtrip() {
        let messages = vec![
            ControlMessage::Hello {
                caps: caps(),
                client_version: "0.1.0".into(),
                client_os: "macos".into(),
            },
            ControlMessage::HelloAck {
                caps: caps(),
                agent_version: "0.1.0".into(),
                active_codec: VideoCodec::Hevc,
            },
            ControlMessage::Bye {
                reason: ByeReason::UserClosed,
                detail: None,
            },
            ControlMessage::Ping { echo_us: 99 },
            ControlMessage::QosReport(QosReport {
                rtt_us: 12_000,
                loss: 0.01,
                jitter_us: 300,
                decode_queue: 2,
                dropped_frames: 0,
                received_bps: 8_000_000,
                rendered_frames: 60,
            }),
        ];
        for m in messages {
            let encoded = m.encode().unwrap();
            assert_eq!(ControlMessage::decode(&encoded).unwrap(), m);
        }
    }

    #[test]
    fn kind_matches_variant() {
        assert_eq!(ControlMessage::Ping { echo_us: 0 }.kind(), MsgKind::Ping);
        assert_eq!(
            ControlMessage::Bye {
                reason: ByeReason::IdleTimeout,
                detail: None
            }
            .kind(),
            MsgKind::Bye
        );
    }

    #[test]
    fn unknown_variant_is_rejected() {
        assert!(ControlMessage::decode(br#"{"t":"nonsense"}"#).is_err());
    }
}
