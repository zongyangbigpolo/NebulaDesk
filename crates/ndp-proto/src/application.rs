//! Negotiated per-application native surfaces. Never interpret these as desktop input.
//!
//! The agent sends `Hello` before `HelloAck`, only after the client offered
//! `APPLICATION_WINDOWS`. VideoFrameInfo.display is a session-unique surface ID.
//! This application-only control handshake follows the existing Noise handshake:
//! client ControlMessage::Hello, agent ApplicationMessage::Hello, then agent
//! ControlMessage::HelloAck. It must complete before the client reports Connected.
//! Desktop sessions keep their existing handshake unchanged.
//! Metadata is sent before video but independent streams can arrive in either
//! order. Receivers drop frames for unknown/retired IDs and request a per-surface
//! keyframe on initial upsert or geometry change. Decoder/recovery state must
//! remain per-surface, never shared between interleaved application windows.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{InputEvent, ProtoError, Result};

/// Application protocol version; independently negotiated from desktop NDP.
pub const APPLICATION_PROTOCOL_VERSION: u16 = 1;
/// IDs are bounded and never reused, including after a surface closes.
pub const MAX_APPLICATION_SURFACES: u8 = 32;
/// Bound untrusted native window titles by UTF-8 byte length.
pub const MAX_SURFACE_TITLE_BYTES: usize = 1024;
/// Maximum capture dimension.
pub const MAX_SURFACE_DIMENSION: u32 = 16384;
/// APP-only generation/sequence prefix after VideoFrameInfo and before codec bytes.
pub const SURFACE_FRAME_INFO_LEN: usize = 8;

/// Binds video/config bytes to the exact surface geometry that produced them.
/// Only negotiated APP sessions use this prefix; desktop media is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceFrameInfo {
    /// Must equal the live SurfaceInfo generation, including for codec config.
    pub geometry_generation: u32,
    /// Per-surface video/config sequence, independent of the global logical
    /// channel sequence written by multipath. Increment for every emitted
    /// record on this surface; do not reset merely because geometry changes.
    pub surface_sequence: u32,
}

impl SurfaceFrameInfo {
    /// Encode generation then surface sequence, each four bytes little-endian.
    pub fn to_bytes(&self) -> [u8; SURFACE_FRAME_INFO_LEN] {
        let mut bytes = [0; SURFACE_FRAME_INFO_LEN];
        bytes[..4].copy_from_slice(&self.geometry_generation.to_le_bytes());
        bytes[4..].copy_from_slice(&self.surface_sequence.to_le_bytes());
        bytes
    }

    /// Parse from the bytes remaining after VideoFrameInfo::split.
    pub fn split(payload: &[u8]) -> Result<(Self, &[u8])> {
        let mut cursor = payload;
        let geometry_generation = crate::read_u32(&mut cursor)?;
        let surface_sequence = crate::read_u32(&mut cursor)?;
        if geometry_generation == 0 {
            return Err(invalid());
        }
        Ok((
            Self {
                geometry_generation,
                surface_sequence,
            },
            cursor,
        ))
    }

    /// Prefix encoded codec bytes, then pass this to VideoFrameInfo::frame_payload.
    pub fn frame_payload(&self, bitstream: &[u8]) -> Vec<u8> {
        let mut payload = Vec::with_capacity(SURFACE_FRAME_INFO_LEN + bitstream.len());
        payload.extend_from_slice(&self.to_bytes());
        payload.extend_from_slice(bitstream);
        payload
    }
}

fn invalid() -> ProtoError {
    ProtoError::InvalidValue {
        field: "application message",
        value: "invalid surface state".into(),
    }
}

/// A native application window; no process IDs or native handles cross the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInfo {
    /// Unique ID matching VideoFrameInfo.display.
    pub surface_id: u8,
    /// Monotonically increasing, nonzero generation of this surface geometry.
    pub geometry_generation: u32,
    /// Display-only text, never an ownership or authorization signal.
    pub title: String,
    /// Pixel width of the isolated capture.
    pub width: u32,
    /// Pixel height of the isolated capture.
    pub height: u32,
    /// Pixels per logical native-window point.
    pub scale: f32,
    /// Related parent, if any; must name an existing live surface.
    pub parent_surface_id: Option<u8>,
    /// Whether this surface is a modal dialog.
    pub modal: bool,
    /// Whether its native window is minimized.
    pub minimized: bool,
}

impl SurfaceInfo {
    /// Validate bounded, finite metadata without trusting platform-provided values.
    pub fn validate(&self) -> Result<()> {
        if self.surface_id >= MAX_APPLICATION_SURFACES
            || self.geometry_generation == 0
            || self.title.len() > MAX_SURFACE_TITLE_BYTES
            || self.title.chars().any(char::is_control)
            || !valid_size(self.width, self.height)
            || !self.scale.is_finite()
            || !(0.25..=8.0).contains(&self.scale)
            || self
                .parent_surface_id
                .is_some_and(|p| p >= MAX_APPLICATION_SURFACES || p == self.surface_id)
        {
            return Err(invalid());
        }
        Ok(())
    }
}

fn valid_size(width: u32, height: u32) -> bool {
    (1..=MAX_SURFACE_DIMENSION).contains(&width) && (1..=MAX_SURFACE_DIMENSION).contains(&height)
}

/// Stable public failure categories; never contain native errors, paths or tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationFailureReason {
    /// The configured application could not be launched.
    LaunchFailed,
    /// No operational isolated-window backend is available.
    BackendUnavailable,
    /// A required native permission is unavailable.
    PermissionDenied,
    /// Native ownership or application isolation could not be established.
    IsolationUnavailable,
    /// The logical session exhausted its never-reused surface ID budget.
    SurfaceLimitReached,
    /// One owned surface temporarily cannot be captured.
    SurfaceUnavailable,
    /// The peer did not negotiate the required application protocol.
    ProtocolMismatch,
}

/// Serving OS advisory for explicit application shortcut mapping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationHostOs {
    /// An older peer supplied no advisory; use physical key mapping.
    #[default]
    Unknown,
    /// macOS application host.
    Macos,
    /// Windows application host.
    Windows,
    /// Linux application host.
    Linux,
}

/// Optional shortcut semantics. Physical input remains the safe default.
/// This advisory never overrides a client's explicit user-selected input mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationKeyboardProfile {
    /// Preserve physical USB HID keys and modifiers.
    #[default]
    Physical,
    /// User-opted-in common text editing shortcuts.
    Editing,
    /// User-opted-in terminal shortcuts, preserving terminal control sequences.
    Terminal,
}

/// Reliable ordered surface lifecycle and scoped commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum ApplicationMessage {
    /// Startup failed after explicit client application opt-in. No desktop fallback.
    StartupFailed {
        /// Stable non-sensitive category, without free-form native error text.
        reason: ApplicationFailureReason,
    },
    /// A live surface is temporarily unavailable. This does not retire its ID;
    /// only SurfaceRemove confirms destruction, and SurfaceUpsert can resume it.
    SurfaceUnavailable {
        /// Existing authorized surface whose capture failed.
        surface_id: u8,
        /// Stable non-sensitive category.
        reason: ApplicationFailureReason,
    },
    /// Agent mode announcement BEFORE HelloAck/Connected. Forbidden for old clients.
    Hello {
        /// Must equal APPLICATION_PROTOCOL_VERSION.
        protocol_version: u16,
        /// Session-wide distinct ID budget.
        max_surfaces: u8,
        /// Serving OS advisory. Missing means no semantic translation.
        #[serde(default)]
        host_os: ApplicationHostOs,
        /// Advisory only; missing preserves physical keyboard semantics.
        #[serde(default)]
        keyboard_profile: ApplicationKeyboardProfile,
    },
    /// Create/update a related native surface before delivering its media.
    SurfaceUpsert {
        /// Validated surface metadata.
        surface: SurfaceInfo,
    },
    /// Permanently retire an ID. Children must be removed first.
    SurfaceRemove {
        /// ID that cannot be reused during this logical session.
        surface_id: u8,
    },
    /// Focus one authorized native surface.
    Focus {
        /// Surface to focus.
        surface_id: u8,
        /// Must match the latest metadata.
        geometry_generation: u32,
    },
    /// Resize one authorized native surface.
    Resize {
        /// Surface to resize.
        surface_id: u8,
        /// Must match the latest metadata.
        geometry_generation: u32,
        /// Requested pixel width.
        width: u32,
        /// Requested pixel height.
        height: u32,
    },
    /// Ask the application to close this window normally. This is not a
    /// destruction acknowledgement: retain local state until SurfaceRemove,
    /// including the last window, because closing may create a save dialog.
    Close {
        /// Surface to close.
        surface_id: u8,
        /// Must match the latest metadata.
        geometry_generation: u32,
    },
    /// Minimize one window, not the remote desktop.
    Minimize {
        /// Surface to minimize.
        surface_id: u8,
        /// Must match the latest metadata.
        geometry_generation: u32,
    },
    /// Request an independent codec refresh for one surface.
    RequestKeyframe {
        /// Surface whose decoder needs a refresh.
        surface_id: u8,
        /// Must match the latest metadata.
        geometry_generation: u32,
    },
    /// Scoped existing 32-byte input event, carried on reliable control.
    Input {
        /// Surface to receive input; must equal the event display field.
        surface_id: u8,
        /// Stale generations must be rejected, never remapped.
        geometry_generation: u32,
        /// Existing InputEvent binary representation; no desktop interpretation.
        event: [u8; 32],
    },
}

impl ApplicationMessage {
    /// Structural validation. A session must additionally check negotiation,
    /// message direction, policy, ownership and current registry generation.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::StartupFailed { .. } => {}
            Self::Hello {
                protocol_version,
                max_surfaces,
                ..
            } => {
                if *protocol_version != APPLICATION_PROTOCOL_VERSION
                    || !(1..=MAX_APPLICATION_SURFACES).contains(max_surfaces)
                {
                    return Err(invalid());
                }
            }
            Self::SurfaceUpsert { surface } => surface.validate()?,
            Self::SurfaceRemove { surface_id } | Self::SurfaceUnavailable { surface_id, .. } => {
                if *surface_id >= MAX_APPLICATION_SURFACES {
                    return Err(invalid());
                }
            }
            _ => {
                let (id, generation) = self.command_target().ok_or_else(invalid)?;
                if id >= MAX_APPLICATION_SURFACES || generation == 0 {
                    return Err(invalid());
                }
                if let Self::Resize { width, height, .. } = self {
                    if !valid_size(*width, *height) {
                        return Err(invalid());
                    }
                }
                if let Self::Input { event, .. } = self {
                    let input = InputEvent::decode(&mut event.as_slice())?;
                    if input.display != id
                        || !input.x.is_finite()
                        || !input.y.is_finite()
                        || !input.scroll_x.is_finite()
                        || !input.scroll_y.is_finite()
                        || (input.kind.is_pointer()
                            && (!(0.0..=1.0).contains(&input.x) || !(0.0..=1.0).contains(&input.y)))
                    {
                        return Err(invalid());
                    }
                }
            }
        }
        Ok(())
    }

    /// Target of a client command; lifecycle/hello messages have no command target.
    pub fn command_target(&self) -> Option<(u8, u32)> {
        match *self {
            Self::Focus {
                surface_id,
                geometry_generation,
            }
            | Self::Resize {
                surface_id,
                geometry_generation,
                ..
            }
            | Self::Close {
                surface_id,
                geometry_generation,
            }
            | Self::Minimize {
                surface_id,
                geometry_generation,
            }
            | Self::RequestKeyframe {
                surface_id,
                geometry_generation,
            }
            | Self::Input {
                surface_id,
                geometry_generation,
                ..
            } => Some((surface_id, geometry_generation)),
            _ => None,
        }
    }
}

/// Bounded lifecycle state shared by native clients and agent command dispatch.
#[derive(Debug)]
pub struct SurfaceRegistry {
    max_surfaces: u8,
    used: [bool; MAX_APPLICATION_SURFACES as usize],
    live: BTreeMap<u8, SurfaceInfo>,
}

impl SurfaceRegistry {
    /// Create only after receiving/negotiating a valid application Hello.
    pub fn new(max_surfaces: u8) -> Result<Self> {
        ApplicationMessage::Hello {
            protocol_version: APPLICATION_PROTOCOL_VERSION,
            max_surfaces,
            host_os: ApplicationHostOs::Unknown,
            keyboard_profile: ApplicationKeyboardProfile::Physical,
        }
        .validate()?;
        Ok(Self {
            max_surfaces,
            used: [false; MAX_APPLICATION_SURFACES as usize],
            live: BTreeMap::new(),
        })
    }

    /// Apply metadata atomically; reject ID reuse, cycles and stale geometry.
    pub fn upsert(&mut self, surface: SurfaceInfo) -> Result<()> {
        surface.validate()?;
        let id = surface.surface_id;
        if id >= self.max_surfaces || (self.used[id as usize] && !self.live.contains_key(&id)) {
            return Err(invalid());
        }
        if let Some(previous) = self.live.get(&id) {
            let changed_geometry = previous.width != surface.width
                || previous.height != surface.height
                || previous.scale != surface.scale;
            if surface.geometry_generation < previous.geometry_generation
                || (changed_geometry && surface.geometry_generation == previous.geometry_generation)
            {
                return Err(invalid());
            }
        }
        let mut parent = surface.parent_surface_id;
        for _ in 0..MAX_APPLICATION_SURFACES {
            let Some(parent_id) = parent else {
                break;
            };
            if parent_id == id {
                return Err(invalid());
            }
            parent = self
                .live
                .get(&parent_id)
                .ok_or_else(invalid)?
                .parent_surface_id;
        }
        if parent.is_some() {
            return Err(invalid());
        }
        self.used[id as usize] = true;
        self.live.insert(id, surface);
        Ok(())
    }

    /// Remove a leaf surface permanently. Drop all queued media for this ID.
    pub fn remove(&mut self, surface_id: u8) -> Result<()> {
        if self
            .live
            .values()
            .any(|s| s.parent_surface_id == Some(surface_id))
            || self.live.remove(&surface_id).is_none()
        {
            return Err(invalid());
        }
        Ok(())
    }

    /// Metadata for a live surface, or None for unknown/retired media.
    pub fn get(&self, surface_id: u8) -> Option<&SurfaceInfo> {
        self.live.get(&surface_id)
    }

    /// Require a live surface and exact generation before queuing decoder work.
    /// Repeat this check when presenting asynchronously decoded frames.
    pub fn validate_frame(&self, surface_id: u8, frame: &SurfaceFrameInfo) -> Result<&SurfaceInfo> {
        let surface = self.live.get(&surface_id).ok_or_else(invalid)?;
        if frame.geometry_generation == 0
            || frame.geometry_generation != surface.geometry_generation
        {
            return Err(invalid());
        }
        Ok(surface)
    }

    /// Require a live surface and exact geometry for a client command.
    /// Backend dispatch must still enforce ticket input policy and native ownership.
    pub fn validate_command(&self, message: &ApplicationMessage) -> Result<&SurfaceInfo> {
        message.validate()?;
        let (id, generation) = message.command_target().ok_or_else(invalid)?;
        let surface = self.live.get(&id).ok_or_else(invalid)?;
        if generation != surface.geometry_generation {
            return Err(invalid());
        }
        Ok(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ControlMessage;

    #[test]
    fn interleaved_surface_sequences_live_inside_authenticated_payloads() {
        for (surface_id, sequence) in [(0, 0), (1, 0), (0, 1), (1, 1), (0, u32::MAX)] {
            let video = crate::VideoFrameInfo {
                display: surface_id,
                codec: crate::VideoCodec::H264,
                width: 640,
                height: 480,
                duration_us: 16667,
            };
            let surface = SurfaceFrameInfo {
                geometry_generation: 7,
                surface_sequence: sequence,
            };
            let payload = video.frame_payload(&surface.frame_payload(b"encoded"));
            let (decoded_video, remaining) = crate::VideoFrameInfo::split(&payload).unwrap();
            let (decoded_surface, encoded) = SurfaceFrameInfo::split(remaining).unwrap();
            assert_eq!(decoded_video.display, surface_id);
            assert_eq!(decoded_surface.surface_sequence, sequence);
            assert_eq!(decoded_surface.geometry_generation, 7);
            assert_eq!(encoded, b"encoded");
        }
    }

    #[test]
    fn old_application_hello_defaults_to_physical_keyboard_without_os_guessing() {
        let control = ControlMessage::decode(
            br#"{"t":"application","op":"Hello","protocol_version":1,"max_surfaces":32}"#,
        )
        .unwrap();
        assert_eq!(
            control,
            ControlMessage::Application(ApplicationMessage::Hello {
                protocol_version: 1,
                max_surfaces: 32,
                host_os: ApplicationHostOs::Unknown,
                keyboard_profile: ApplicationKeyboardProfile::Physical,
            })
        );
    }

    #[test]
    fn application_failures_are_stable_bounded_and_round_trip() {
        for message in [
            ApplicationMessage::StartupFailed {
                reason: ApplicationFailureReason::LaunchFailed,
            },
            ApplicationMessage::SurfaceUnavailable {
                surface_id: 0,
                reason: ApplicationFailureReason::PermissionDenied,
            },
        ] {
            let control = ControlMessage::Application(message);
            assert_eq!(
                ControlMessage::decode(&control.encode().unwrap()).unwrap(),
                control
            );
        }
        assert!(ApplicationMessage::SurfaceUnavailable {
            surface_id: 32,
            reason: ApplicationFailureReason::SurfaceUnavailable,
        }
        .validate()
        .is_err());
        assert!(ControlMessage::decode(
            br#"{"t":"application","op":"StartupFailed","reason":"private executable path"}"#
        )
        .is_err());
    }

    #[test]
    fn generation_prefix_drops_stale_same_size_frames_and_retired_surfaces() {
        let frame = SurfaceFrameInfo {
            geometry_generation: 1,
            surface_sequence: 42,
        };
        let video = crate::VideoFrameInfo {
            display: 0,
            codec: crate::VideoCodec::H264,
            width: 640,
            height: 480,
            duration_us: 16667,
        };
        let payload = video.frame_payload(&frame.frame_payload(b"codec-data"));
        let (video_back, remaining) = crate::VideoFrameInfo::split(&payload).unwrap();
        let (frame_back, bitstream) = SurfaceFrameInfo::split(remaining).unwrap();
        assert_eq!(video_back, video);
        assert_eq!(frame_back, frame);
        assert_eq!(bitstream, b"codec-data");
        let mut registry = SurfaceRegistry::new(32).unwrap();
        assert!(registry.validate_frame(0, &frame).is_err());
        registry.upsert(surface(0)).unwrap();
        assert!(registry.validate_frame(0, &frame).is_ok());
        let mut next = surface(0);
        next.geometry_generation = 2;
        registry.upsert(next).unwrap();
        assert!(
            registry.validate_frame(0, &frame).is_err(),
            "same-size stale frame"
        );
        registry.remove(0).unwrap();
        assert!(registry
            .validate_frame(
                0,
                &SurfaceFrameInfo {
                    geometry_generation: 2,
                    surface_sequence: 43,
                }
            )
            .is_err());
        assert!(SurfaceFrameInfo::split(&[0; 8]).is_err());
        assert!(SurfaceFrameInfo::split(&[1; 7]).is_err());
    }

    #[test]
    fn input_requires_live_surface_and_exact_generation() {
        let mut registry = SurfaceRegistry::new(32).unwrap();
        registry.upsert(surface(2)).unwrap();
        let mut event = InputEvent::mouse_move(0.5, 0.5, crate::Modifiers::default());
        let mut message = ApplicationMessage::Input {
            surface_id: 2,
            geometry_generation: 1,
            event: event.to_bytes(),
        };
        assert!(
            message.validate().is_err(),
            "desktop/default display cannot target another surface"
        );
        event.display = 2;
        message = ApplicationMessage::Input {
            surface_id: 2,
            geometry_generation: 1,
            event: event.to_bytes(),
        };
        assert!(registry.validate_command(&message).is_ok());
        registry.remove(2).unwrap();
        assert!(registry.validate_command(&message).is_err());
        event.x = f32::NAN;
        assert!(ApplicationMessage::Input {
            surface_id: 2,
            geometry_generation: 1,
            event: event.to_bytes()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn early_late_media_and_pending_close_never_create_or_retire_surfaces() {
        let mut registry = SurfaceRegistry::new(32).unwrap();
        assert!(
            registry.get(0).is_none(),
            "early frames cannot create a surface"
        );
        registry.upsert(surface(0)).unwrap();
        let close = ApplicationMessage::Close {
            surface_id: 0,
            geometry_generation: 1,
        };
        registry.validate_command(&close).unwrap();
        assert!(registry.get(0).is_some(), "close is not remote destruction");
        let mut save_dialog = surface(1);
        save_dialog.parent_surface_id = Some(0);
        save_dialog.modal = true;
        registry.upsert(save_dialog).unwrap();
        assert!(
            registry.remove(0).is_err(),
            "save dialog retains its parent"
        );
        registry.remove(1).unwrap();
        registry.remove(0).unwrap();
        assert!(
            registry.get(0).is_none(),
            "late frames cannot revive a surface"
        );
        assert!(
            registry.upsert(surface(0)).is_err(),
            "retired IDs cannot be reused"
        );
    }
    fn surface(id: u8) -> SurfaceInfo {
        SurfaceInfo {
            surface_id: id,
            geometry_generation: 1,
            title: "Application".into(),
            width: 640,
            height: 480,
            scale: 1.0,
            parent_surface_id: None,
            modal: false,
            minimized: false,
        }
    }

    #[test]
    fn lifecycle_never_reuses_ids_and_rejects_stale_geometry() {
        let mut registry = SurfaceRegistry::new(2).unwrap();
        registry.upsert(surface(0)).unwrap();
        let mut resized = surface(0);
        resized.width = 800;
        assert!(registry.upsert(resized.clone()).is_err());
        resized.geometry_generation = 2;
        registry.upsert(resized).unwrap();
        assert!(registry
            .validate_command(&ApplicationMessage::Focus {
                surface_id: 0,
                geometry_generation: 1
            })
            .is_err());
        registry.remove(0).unwrap();
        assert!(registry.upsert(surface(0)).is_err());
        assert!(registry.upsert(surface(2)).is_err());
    }

    #[test]
    fn parent_graph_and_removal_are_bounded() {
        let mut registry = SurfaceRegistry::new(32).unwrap();
        registry.upsert(surface(0)).unwrap();
        let mut child = surface(1);
        child.parent_surface_id = Some(0);
        registry.upsert(child).unwrap();
        let mut cycle = surface(0);
        cycle.parent_surface_id = Some(1);
        assert!(registry.upsert(cycle).is_err());
        assert!(registry.remove(0).is_err());
        registry.remove(1).unwrap();
        registry.remove(0).unwrap();
    }

    #[test]
    fn control_roundtrip_and_bad_geometry() {
        let message = ControlMessage::Application(ApplicationMessage::SurfaceUpsert {
            surface: surface(0),
        });
        assert_eq!(
            ControlMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
        let mut bad = surface(0);
        bad.scale = f32::NAN;
        assert!(bad.validate().is_err());
        bad = surface(0);
        bad.title = "x".repeat(MAX_SURFACE_TITLE_BYTES + 1);
        assert!(bad.validate().is_err());
        assert!(ApplicationMessage::Hello {
            protocol_version: 2,
            max_surfaces: 32,
            host_os: ApplicationHostOs::Unknown,
            keyboard_profile: ApplicationKeyboardProfile::Physical,
        }
        .validate()
        .is_err());
    }
}
