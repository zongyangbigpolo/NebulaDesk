//! Input events (client to agent).
//!
//! This is a hot path — a dragging mouse produces hundreds of events per
//! second — so events use a fixed 32-byte binary layout rather than JSON.
//!
//! # Cross-platform key codes
//!
//! V1 forwarded raw macOS virtual key codes, which only worked because both
//! ends were Macs. V2 normalises to **USB HID Usage IDs (Keyboard/Keypad page
//! `0x07`)**: every platform can map to and from them, so a Windows client can
//! drive a Linux agent.
//!
//! # Coordinates
//!
//! Pointer positions are normalised to `[0.0, 1.0]` over the target display,
//! making them resolution independent — the agent multiplies by its own
//! capture geometry.

use crate::{read_f32, read_u16, read_u32, read_u8, take, ProtoError, Result};

/// Serialized size of one [`InputEvent`].
pub const INPUT_EVENT_LEN: usize = 32;

/// What kind of input occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum InputKind {
    /// Pointer moved with no button held.
    MouseMove = 1,
    /// Pointer button pressed.
    MouseDown = 2,
    /// Pointer button released.
    MouseUp = 3,
    /// Pointer moved with a button held.
    MouseDrag = 4,
    /// Scroll wheel or trackpad scroll.
    Wheel = 5,
    /// Key pressed (or auto-repeated).
    KeyDown = 6,
    /// Key released.
    KeyUp = 7,
    /// Pointer left the client's video surface — the agent should drop any
    /// hover state.
    PointerLeave = 8,
}

impl InputKind {
    /// Decode a wire discriminant.
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            1 => Self::MouseMove,
            2 => Self::MouseDown,
            3 => Self::MouseUp,
            4 => Self::MouseDrag,
            5 => Self::Wheel,
            6 => Self::KeyDown,
            7 => Self::KeyUp,
            8 => Self::PointerLeave,
            other => {
                return Err(ProtoError::InvalidValue {
                    field: "input kind",
                    value: other.to_string(),
                })
            }
        })
    }

    /// Whether this event carries a meaningful pointer position.
    #[must_use]
    pub const fn is_pointer(self) -> bool {
        matches!(
            self,
            Self::MouseMove | Self::MouseDown | Self::MouseUp | Self::MouseDrag | Self::Wheel
        )
    }

    /// Whether this event is safe to coalesce with an identical successor
    /// (only positional moves are — dropping a click would be a bug).
    #[must_use]
    pub const fn is_coalescible(self) -> bool {
        matches!(self, Self::MouseMove | Self::MouseDrag)
    }
}

/// Pointer buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MouseButton {
    /// No button (movement, or a wheel event).
    None = 0,
    /// Primary button.
    Left = 1,
    /// Secondary button.
    Right = 2,
    /// Wheel button.
    Middle = 3,
    /// Back / X1.
    Back = 4,
    /// Forward / X2.
    Forward = 5,
}

impl MouseButton {
    /// Decode a wire discriminant.
    pub fn from_u8(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Self::None,
            1 => Self::Left,
            2 => Self::Right,
            3 => Self::Middle,
            4 => Self::Back,
            5 => Self::Forward,
            other => {
                return Err(ProtoError::InvalidValue {
                    field: "mouse button",
                    value: other.to_string(),
                })
            }
        })
    }
}

/// Keyboard modifier state, as a bitset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Modifiers(pub u16);

impl Modifiers {
    /// No modifiers held.
    pub const NONE: Self = Self(0);
    /// Either Shift key.
    pub const SHIFT: Self = Self(1 << 0);
    /// Either Control key.
    pub const CONTROL: Self = Self(1 << 1);
    /// Either Alt / Option key.
    pub const ALT: Self = Self(1 << 2);
    /// Either Command / Windows / Super key.
    pub const META: Self = Self(1 << 3);
    /// Caps Lock is engaged.
    pub const CAPS_LOCK: Self = Self(1 << 4);
    /// Num Lock is engaged.
    pub const NUM_LOCK: Self = Self(1 << 5);
    /// The macOS Fn key is held.
    pub const FN: Self = Self(1 << 6);
    /// The event is an OS-generated auto-repeat.
    pub const REPEAT: Self = Self(1 << 7);

    /// True when every bit in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union of two modifier sets.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

/// A USB HID Usage ID from the Keyboard/Keypad page (`0x07`).
///
/// Platform-neutral by construction; each platform backend owns the mapping to
/// and from its native representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyCode(pub u32);

impl KeyCode {
    /// No key.
    pub const NONE: Self = Self(0);
    /// `a` / `A`.
    pub const A: Self = Self(0x04);
    /// `z` / `Z`.
    pub const Z: Self = Self(0x1d);
    /// `1` / `!`.
    pub const DIGIT1: Self = Self(0x1e);
    /// Return / Enter.
    pub const ENTER: Self = Self(0x28);
    /// Escape.
    pub const ESCAPE: Self = Self(0x29);
    /// Backspace.
    pub const BACKSPACE: Self = Self(0x2a);
    /// Tab.
    pub const TAB: Self = Self(0x2b);
    /// Space bar.
    pub const SPACE: Self = Self(0x2c);
    /// F1.
    pub const F1: Self = Self(0x3a);
    /// Right arrow.
    pub const ARROW_RIGHT: Self = Self(0x4f);
    /// Left arrow.
    pub const ARROW_LEFT: Self = Self(0x50);
    /// Down arrow.
    pub const ARROW_DOWN: Self = Self(0x51);
    /// Up arrow.
    pub const ARROW_UP: Self = Self(0x52);
    /// Left Control.
    pub const CONTROL_LEFT: Self = Self(0xe0);
    /// Left Shift.
    pub const SHIFT_LEFT: Self = Self(0xe1);
    /// Left Alt / Option.
    pub const ALT_LEFT: Self = Self(0xe2);
    /// Left Meta / Command / Windows.
    pub const META_LEFT: Self = Self(0xe3);
    /// Right Meta / Command / Windows.
    pub const META_RIGHT: Self = Self(0xe7);

    /// Whether the usage ID falls inside the defined Keyboard/Keypad page.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.0 <= 0xe7
    }
}

/// One input event.
///
/// Wire layout (little-endian, 32 bytes):
///
/// | offset | size | field |
/// |---|---|---|
/// | 0 | 1 | `kind` |
/// | 1 | 1 | `button` |
/// | 2 | 2 | `modifiers` |
/// | 4 | 1 | `display` |
/// | 5 | 1 | reserved |
/// | 6 | 2 | `pointer_id` |
/// | 8 | 4 | `x` (normalised f32) |
/// | 12 | 4 | `y` (normalised f32) |
/// | 16 | 4 | `scroll_x` |
/// | 20 | 4 | `scroll_y` |
/// | 24 | 4 | `key` (HID usage) |
/// | 28 | 4 | `unicode` scalar, 0 when unused |
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEvent {
    /// What happened.
    pub kind: InputKind,
    /// Which pointer button, if any.
    pub button: MouseButton,
    /// Modifier keys held at the time of the event.
    pub modifiers: Modifiers,
    /// Index into the negotiated display list.
    pub display: u8,
    /// Distinguishes simultaneous pointers (multi-touch); 0 for the mouse.
    pub pointer_id: u16,
    /// Horizontal position normalised to `[0, 1]` over the target display.
    pub x: f32,
    /// Vertical position normalised to `[0, 1]` over the target display.
    pub y: f32,
    /// Horizontal scroll delta in lines (positive = right).
    pub scroll_x: f32,
    /// Vertical scroll delta in lines (positive = up).
    pub scroll_y: f32,
    /// HID usage of the key involved, or [`KeyCode::NONE`].
    pub key: KeyCode,
    /// Resolved Unicode scalar for text input, or 0.
    ///
    /// Sent alongside the HID usage so an agent can reproduce characters that
    /// depend on the *client's* keyboard layout (e.g. a German client driving
    /// a US-layout agent) instead of re-deriving them from the scan code.
    pub unicode: u32,
}

impl InputEvent {
    /// A pointer movement to a normalised position.
    #[must_use]
    pub fn mouse_move(x: f32, y: f32, modifiers: Modifiers) -> Self {
        Self {
            kind: InputKind::MouseMove,
            button: MouseButton::None,
            modifiers,
            display: 0,
            pointer_id: 0,
            x,
            y,
            scroll_x: 0.0,
            scroll_y: 0.0,
            key: KeyCode::NONE,
            unicode: 0,
        }
    }

    /// A key press or release.
    #[must_use]
    pub fn key(kind: InputKind, key: KeyCode, modifiers: Modifiers) -> Self {
        Self {
            kind,
            button: MouseButton::None,
            modifiers,
            display: 0,
            pointer_id: 0,
            x: 0.0,
            y: 0.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            key,
            unicode: 0,
        }
    }

    /// Serialize into the fixed 32-byte layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; INPUT_EVENT_LEN] {
        let mut out = [0u8; INPUT_EVENT_LEN];
        out[0] = self.kind as u8;
        out[1] = self.button as u8;
        out[2..4].copy_from_slice(&self.modifiers.0.to_le_bytes());
        out[4] = self.display;
        out[6..8].copy_from_slice(&self.pointer_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.x.to_le_bytes());
        out[12..16].copy_from_slice(&self.y.to_le_bytes());
        out[16..20].copy_from_slice(&self.scroll_x.to_le_bytes());
        out[20..24].copy_from_slice(&self.scroll_y.to_le_bytes());
        out[24..28].copy_from_slice(&self.key.0.to_le_bytes());
        out[28..32].copy_from_slice(&self.unicode.to_le_bytes());
        out
    }

    /// Parse one event from the front of `buf`, advancing it.
    ///
    /// Coordinates are clamped to `[0, 1]` and non-finite values rejected, so
    /// a malicious client cannot steer the agent's cursor off-screen or feed
    /// `NaN` into platform injection APIs.
    pub fn decode(buf: &mut &[u8]) -> Result<Self> {
        let kind = InputKind::from_u8(read_u8(buf)?)?;
        let button = MouseButton::from_u8(read_u8(buf)?)?;
        let modifiers = Modifiers(read_u16(buf)?);
        let display = read_u8(buf)?;
        let _reserved = read_u8(buf)?;
        let pointer_id = read_u16(buf)?;
        let x = read_coord(buf, "x")?;
        let y = read_coord(buf, "y")?;
        let scroll_x = read_finite(buf, "scroll_x")?;
        let scroll_y = read_finite(buf, "scroll_y")?;
        let key = KeyCode(read_u32(buf)?);
        let unicode = read_u32(buf)?;

        if !key.is_valid() {
            return Err(ProtoError::InvalidValue {
                field: "key",
                value: key.0.to_string(),
            });
        }

        Ok(Self {
            kind,
            button,
            modifiers,
            display,
            pointer_id,
            x,
            y,
            scroll_x,
            scroll_y,
            key,
            unicode,
        })
    }

    /// Decode a densely packed batch of events (an `InputBatch` payload).
    pub fn decode_batch(mut buf: &[u8]) -> Result<Vec<Self>> {
        if buf.len() % INPUT_EVENT_LEN != 0 {
            return Err(ProtoError::Truncated {
                need: buf.len().next_multiple_of(INPUT_EVENT_LEN),
                have: buf.len(),
            });
        }
        let mut out = Vec::with_capacity(buf.len() / INPUT_EVENT_LEN);
        while !buf.is_empty() {
            out.push(Self::decode(&mut buf)?);
        }
        Ok(out)
    }

    /// Encode a batch of events into one contiguous payload.
    #[must_use]
    pub fn encode_batch(events: &[Self]) -> Vec<u8> {
        let mut out = Vec::with_capacity(events.len() * INPUT_EVENT_LEN);
        for e in events {
            out.extend_from_slice(&e.to_bytes());
        }
        out
    }

    /// Map the normalised position onto a concrete pixel grid.
    #[must_use]
    pub fn to_pixels(&self, width: u32, height: u32) -> (f64, f64) {
        (
            f64::from(self.x) * f64::from(width),
            f64::from(self.y) * f64::from(height),
        )
    }
}

fn read_coord(buf: &mut &[u8], field: &'static str) -> Result<f32> {
    let v = read_finite(buf, field)?;
    Ok(v.clamp(0.0, 1.0))
}

fn read_finite(buf: &mut &[u8], field: &'static str) -> Result<f32> {
    let v = read_f32(buf)?;
    if !v.is_finite() {
        return Err(ProtoError::InvalidValue {
            field,
            value: format!("{v}"),
        });
    }
    Ok(v)
}

/// Coalesce consecutive positional moves, keeping only the latest.
///
/// Under congestion an unthrottled client can queue hundreds of moves; only
/// the final position matters, but every click and key must survive.
#[must_use]
pub fn coalesce(events: &[InputEvent]) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for &e in events {
        match out.last_mut() {
            Some(prev)
                if prev.kind == e.kind
                    && e.kind.is_coalescible()
                    && prev.button == e.button
                    && prev.pointer_id == e.pointer_id
                    && prev.display == e.display =>
            {
                *prev = e;
            }
            _ => out.push(e),
        }
    }
    out
}

/// Read a raw batch payload without allocating, for zero-copy injection.
pub fn iter_batch(buf: &[u8]) -> impl Iterator<Item = Result<InputEvent>> + '_ {
    let mut rest = buf;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        if rest.len() < INPUT_EVENT_LEN {
            let have = rest.len();
            rest = &[];
            return Some(Err(ProtoError::Truncated {
                need: INPUT_EVENT_LEN,
                have,
            }));
        }
        let mut chunk = take(&mut rest, INPUT_EVENT_LEN).expect("length checked");
        Some(InputEvent::decode(&mut chunk))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_roundtrips() {
        let e = InputEvent {
            kind: InputKind::MouseDown,
            button: MouseButton::Right,
            modifiers: Modifiers::SHIFT | Modifiers::META,
            display: 1,
            pointer_id: 7,
            x: 0.25,
            y: 0.75,
            scroll_x: -1.5,
            scroll_y: 3.0,
            key: KeyCode::A,
            unicode: 'a' as u32,
        };
        let bytes = e.to_bytes();
        assert_eq!(bytes.len(), INPUT_EVENT_LEN);
        let mut cursor = &bytes[..];
        assert_eq!(InputEvent::decode(&mut cursor).unwrap(), e);
        assert!(cursor.is_empty());
    }

    #[test]
    fn coordinates_are_clamped() {
        let mut e = InputEvent::mouse_move(5.0, -3.0, Modifiers::NONE);
        let bytes = e.to_bytes();
        let mut cursor = &bytes[..];
        e = InputEvent::decode(&mut cursor).unwrap();
        assert_eq!(e.x, 1.0);
        assert_eq!(e.y, 0.0);
    }

    #[test]
    fn non_finite_coordinates_are_rejected() {
        let e = InputEvent::mouse_move(f32::NAN, 0.5, Modifiers::NONE);
        let bytes = e.to_bytes();
        let mut cursor = &bytes[..];
        assert!(matches!(
            InputEvent::decode(&mut cursor),
            Err(ProtoError::InvalidValue { field: "x", .. })
        ));
    }

    #[test]
    fn out_of_page_keycodes_are_rejected() {
        let mut e = InputEvent::key(InputKind::KeyDown, KeyCode::A, Modifiers::NONE);
        e.key = KeyCode(0xffff);
        let bytes = e.to_bytes();
        let mut cursor = &bytes[..];
        assert!(matches!(
            InputEvent::decode(&mut cursor),
            Err(ProtoError::InvalidValue { field: "key", .. })
        ));
    }

    #[test]
    fn batch_roundtrips() {
        let events = vec![
            InputEvent::mouse_move(0.1, 0.1, Modifiers::NONE),
            InputEvent::key(InputKind::KeyDown, KeyCode::ENTER, Modifiers::NONE),
        ];
        let encoded = InputEvent::encode_batch(&events);
        assert_eq!(InputEvent::decode_batch(&encoded).unwrap(), events);
        let via_iter: Vec<_> = iter_batch(&encoded).map(Result::unwrap).collect();
        assert_eq!(via_iter, events);
    }

    #[test]
    fn ragged_batch_is_rejected() {
        let mut encoded =
            InputEvent::encode_batch(&[InputEvent::mouse_move(0.0, 0.0, Modifiers::NONE)]);
        encoded.truncate(INPUT_EVENT_LEN - 1);
        assert!(InputEvent::decode_batch(&encoded).is_err());
        assert!(iter_batch(&encoded).any(|r| r.is_err()));
    }

    #[test]
    fn coalesce_keeps_clicks_and_drops_stale_moves() {
        let events = vec![
            InputEvent::mouse_move(0.1, 0.1, Modifiers::NONE),
            InputEvent::mouse_move(0.2, 0.2, Modifiers::NONE),
            InputEvent::mouse_move(0.3, 0.3, Modifiers::NONE),
            InputEvent::key(InputKind::KeyDown, KeyCode::SPACE, Modifiers::NONE),
            InputEvent::mouse_move(0.4, 0.4, Modifiers::NONE),
        ];
        let out = coalesce(&events);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].x, 0.3);
        assert_eq!(out[1].kind, InputKind::KeyDown);
        assert_eq!(out[2].x, 0.4);
    }

    #[test]
    fn normalised_coordinates_map_to_pixels() {
        let e = InputEvent::mouse_move(0.5, 0.25, Modifiers::NONE);
        assert_eq!(e.to_pixels(1920, 1080), (960.0, 270.0));
    }
}
