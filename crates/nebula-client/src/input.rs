//! Turning local input into protocol events.
//!
//! Keys travel as USB HID usages, not as the platform's own scan codes. That
//! is what lets a Mac drive a Windows machine: both ends already know how to
//! translate HID, and neither has to carry a table for the other's
//! conventions. Winit's `KeyCode` is itself a physical-position code, so this
//! mapping is positional throughout and never depends on the layout either
//! side happens to have selected.
//!
//! Text is carried separately, as a Unicode scalar, because the character a
//! key produces depends on the *client's* layout and only the client knows
//! it.

use ndp_proto::{InputEvent, InputKind, KeyCode as Hid, Modifiers, MouseButton};
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};

/// Where the picture sits inside the window.
///
/// The video keeps its own aspect ratio, so unless the window happens to
/// match there are bars at the sides or the top. Positions must be normalised
/// against the picture, not the window, or the remote pointer drifts further
/// from the local one the further it gets from the centre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Left edge of the picture within the window, in physical pixels.
    pub x: f64,
    /// Top edge of the picture within the window, in physical pixels.
    pub y: f64,
    /// Picture width in physical pixels.
    pub width: f64,
    /// Picture height in physical pixels.
    pub height: f64,
}

impl Viewport {
    /// Fit a picture of the given size inside a window, centred.
    #[must_use]
    pub fn fit(window: (f64, f64), picture: (u32, u32)) -> Self {
        let (ww, wh) = window;
        if picture.0 == 0 || picture.1 == 0 || ww <= 0.0 || wh <= 0.0 {
            return Self {
                x: 0.0,
                y: 0.0,
                width: ww.max(1.0),
                height: wh.max(1.0),
            };
        }
        let scale = (ww / f64::from(picture.0)).min(wh / f64::from(picture.1));
        let width = f64::from(picture.0) * scale;
        let height = f64::from(picture.1) * scale;
        Self {
            x: (ww - width) / 2.0,
            y: (wh - height) / 2.0,
            width,
            height,
        }
    }

    /// Normalise a window position to `[0, 1]` over the picture.
    ///
    /// Returns `None` for a position in the bars around the picture: there is
    /// no remote pixel under the cursor there, and clamping would pin the
    /// remote pointer to an edge it was never moved to.
    #[must_use]
    pub fn normalise(&self, x: f64, y: f64) -> Option<(f32, f32)> {
        let u = (x - self.x) / self.width;
        let v = (y - self.y) / self.height;
        (0.0..=1.0).contains(&u).then_some(())?;
        (0.0..=1.0).contains(&v).then_some(())?;
        Some((u as f32, v as f32))
    }
}

/// Translate the modifier keys currently held.
#[must_use]
pub fn modifiers(state: ModifiersState) -> Modifiers {
    let mut out = Modifiers::NONE;
    if state.shift_key() {
        out = out.union(Modifiers::SHIFT);
    }
    if state.control_key() {
        out = out.union(Modifiers::CONTROL);
    }
    if state.alt_key() {
        out = out.union(Modifiers::ALT);
    }
    if state.super_key() {
        out = out.union(Modifiers::META);
    }
    out
}

/// Translate a mouse button.
#[must_use]
pub fn button(button: WinitButton) -> Option<MouseButton> {
    Some(match button {
        WinitButton::Left => MouseButton::Left,
        WinitButton::Right => MouseButton::Right,
        WinitButton::Middle => MouseButton::Middle,
        WinitButton::Back => MouseButton::Back,
        WinitButton::Forward => MouseButton::Forward,
        // Extra buttons have no agreed meaning across platforms, so there is
        // nothing sensible to send.
        WinitButton::Other(_) => return None,
    })
}

/// Build a scroll event.
///
/// The protocol carries scroll in lines because that is the unit every
/// platform's injection API accepts. Pixel deltas — trackpads, precise mice —
/// are converted with the same divisor the platforms use for the reverse
/// conversion, so a trackpad flick moves about as far remotely as locally.
#[must_use]
pub fn scroll(delta: MouseScrollDelta, at: (f32, f32), modifiers: Modifiers) -> InputEvent {
    const PIXELS_PER_LINE: f64 = 16.0;
    let (x, y) = match delta {
        MouseScrollDelta::LineDelta(x, y) => (x, y),
        MouseScrollDelta::PixelDelta(position) => (
            (position.x / PIXELS_PER_LINE) as f32,
            (position.y / PIXELS_PER_LINE) as f32,
        ),
    };
    InputEvent {
        kind: InputKind::Wheel,
        button: MouseButton::None,
        modifiers,
        display: 0,
        pointer_id: 0,
        x: at.0,
        y: at.1,
        scroll_x: x,
        scroll_y: y,
        key: Hid::NONE,
        unicode: 0,
    }
}

/// Build a key event, carrying the character it produced when there is one.
#[must_use]
pub fn key(
    physical: PhysicalKey,
    state: ElementState,
    text: Option<&str>,
    held: Modifiers,
) -> Option<InputEvent> {
    let PhysicalKey::Code(code) = physical else {
        // A key this build of winit has no name for. Sending a made-up usage
        // would press something arbitrary on the other machine.
        return None;
    };
    let usage = hid(code)?;
    let kind = match state {
        ElementState::Pressed => InputKind::KeyDown,
        ElementState::Released => InputKind::KeyUp,
    };
    let mut event = InputEvent::key(kind, Hid(usage), held);
    // Control characters are the platform's rendering of the key, not text
    // the user typed; forwarding them would double up with the key itself.
    event.unicode = text
        .and_then(|t| t.chars().next())
        .filter(|c| !c.is_control())
        .map_or(0, u32::from);
    Some(event)
}

/// Map a physical key to its USB HID usage on the Keyboard/Keypad page.
///
/// The table is written out rather than computed because the HID page is not
/// ordered the way any keyboard is, and an arithmetic shortcut that is right
/// for the letters is wrong everywhere else.
#[must_use]
pub fn hid(code: KeyCode) -> Option<u32> {
    use KeyCode as K;
    Some(match code {
        K::KeyA => 0x04,
        K::KeyB => 0x05,
        K::KeyC => 0x06,
        K::KeyD => 0x07,
        K::KeyE => 0x08,
        K::KeyF => 0x09,
        K::KeyG => 0x0a,
        K::KeyH => 0x0b,
        K::KeyI => 0x0c,
        K::KeyJ => 0x0d,
        K::KeyK => 0x0e,
        K::KeyL => 0x0f,
        K::KeyM => 0x10,
        K::KeyN => 0x11,
        K::KeyO => 0x12,
        K::KeyP => 0x13,
        K::KeyQ => 0x14,
        K::KeyR => 0x15,
        K::KeyS => 0x16,
        K::KeyT => 0x17,
        K::KeyU => 0x18,
        K::KeyV => 0x19,
        K::KeyW => 0x1a,
        K::KeyX => 0x1b,
        K::KeyY => 0x1c,
        K::KeyZ => 0x1d,

        // The digit row starts at 1 and wraps 0 around to the end, which is
        // the one place the HID page's ordering surprises people.
        K::Digit1 => 0x1e,
        K::Digit2 => 0x1f,
        K::Digit3 => 0x20,
        K::Digit4 => 0x21,
        K::Digit5 => 0x22,
        K::Digit6 => 0x23,
        K::Digit7 => 0x24,
        K::Digit8 => 0x25,
        K::Digit9 => 0x26,
        K::Digit0 => 0x27,

        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2a,
        K::Tab => 0x2b,
        K::Space => 0x2c,
        K::Minus => 0x2d,
        K::Equal => 0x2e,
        K::BracketLeft => 0x2f,
        K::BracketRight => 0x30,
        K::Backslash => 0x31,
        K::Semicolon => 0x33,
        K::Quote => 0x34,
        K::Backquote => 0x35,
        K::Comma => 0x36,
        K::Period => 0x37,
        K::Slash => 0x38,
        K::CapsLock => 0x39,

        K::F1 => 0x3a,
        K::F2 => 0x3b,
        K::F3 => 0x3c,
        K::F4 => 0x3d,
        K::F5 => 0x3e,
        K::F6 => 0x3f,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,
        K::PrintScreen => 0x46,
        K::ScrollLock => 0x47,
        K::Pause => 0x48,
        K::Insert => 0x49,
        K::Home => 0x4a,
        K::PageUp => 0x4b,
        K::Delete => 0x4c,
        K::End => 0x4d,
        K::PageDown => 0x4e,
        K::ArrowRight => 0x4f,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,

        K::NumLock => 0x53,
        K::NumpadDivide => 0x54,
        K::NumpadMultiply => 0x55,
        K::NumpadSubtract => 0x56,
        K::NumpadAdd => 0x57,
        K::NumpadEnter => 0x58,
        K::Numpad1 => 0x59,
        K::Numpad2 => 0x5a,
        K::Numpad3 => 0x5b,
        K::Numpad4 => 0x5c,
        K::Numpad5 => 0x5d,
        K::Numpad6 => 0x5e,
        K::Numpad7 => 0x5f,
        K::Numpad8 => 0x60,
        K::Numpad9 => 0x61,
        K::Numpad0 => 0x62,
        K::NumpadDecimal => 0x63,

        K::ContextMenu => 0x65,
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6a,
        K::F16 => 0x6b,
        K::F17 => 0x6c,
        K::F18 => 0x6d,
        K::F19 => 0x6e,
        K::F20 => 0x6f,

        K::ControlLeft => 0xe0,
        K::ShiftLeft => 0xe1,
        K::AltLeft => 0xe2,
        K::SuperLeft => 0xe3,
        K::ControlRight => 0xe4,
        K::ShiftRight => 0xe5,
        K::AltRight => 0xe6,
        K::SuperRight => 0xe7,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_is_centred_and_keeps_its_shape() {
        // A 16:9 picture in a square window: bars top and bottom.
        let viewport = Viewport::fit((1000.0, 1000.0), (1920, 1080));
        assert!((viewport.width - 1000.0).abs() < 0.001);
        assert!((viewport.height - 562.5).abs() < 0.001);
        assert!((viewport.x - 0.0).abs() < 0.001);
        assert!((viewport.y - 218.75).abs() < 0.001);
        assert!((viewport.width / viewport.height - 1920.0 / 1080.0).abs() < 0.001);
    }

    #[test]
    fn positions_are_normalised_against_the_picture_not_the_window() {
        let viewport = Viewport::fit((1000.0, 1000.0), (1920, 1080));
        // The centre of the window is the centre of the picture.
        let (x, y) = viewport.normalise(500.0, 500.0).unwrap();
        assert!((x - 0.5).abs() < 0.001);
        assert!((y - 0.5).abs() < 0.001);

        // The top-left of the picture, not of the window.
        let (x, y) = viewport.normalise(0.0, viewport.y).unwrap();
        assert!(x.abs() < 0.001);
        assert!(y.abs() < 0.001);
    }

    #[test]
    fn the_bars_around_the_picture_produce_no_position() {
        // Clamping here would pin the remote pointer to an edge the user
        // never moved it to, which reads as the session having frozen.
        let viewport = Viewport::fit((1000.0, 1000.0), (1920, 1080));
        assert!(viewport.normalise(500.0, 10.0).is_none());
        assert!(viewport.normalise(500.0, 990.0).is_none());
    }

    #[test]
    fn a_window_with_no_area_does_not_divide_by_zero() {
        let viewport = Viewport::fit((0.0, 0.0), (1920, 1080));
        assert!(viewport.width > 0.0 && viewport.height > 0.0);
        let viewport = Viewport::fit((800.0, 600.0), (0, 0));
        assert!(viewport.width > 0.0 && viewport.height > 0.0);
    }

    #[test]
    fn keys_map_to_hid_usages_positionally() {
        assert_eq!(hid(KeyCode::KeyA), Some(0x04));
        assert_eq!(hid(KeyCode::KeyZ), Some(0x1d));
        // The digit row: 1 comes first and 0 comes last.
        assert_eq!(hid(KeyCode::Digit1), Some(0x1e));
        assert_eq!(hid(KeyCode::Digit0), Some(0x27));
        assert_eq!(hid(KeyCode::Escape), Some(0x29));
        assert_eq!(hid(KeyCode::SuperLeft), Some(0xe3));
    }

    #[test]
    fn no_two_keys_share_a_usage() {
        // A collision would make one key press another, silently.
        let codes = [
            KeyCode::KeyA,
            KeyCode::KeyZ,
            KeyCode::Digit0,
            KeyCode::Digit1,
            KeyCode::Enter,
            KeyCode::Escape,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::Space,
            KeyCode::Minus,
            KeyCode::Equal,
            KeyCode::BracketLeft,
            KeyCode::F1,
            KeyCode::F12,
            KeyCode::F13,
            KeyCode::F20,
            KeyCode::Numpad0,
            KeyCode::Numpad9,
            KeyCode::NumpadEnter,
            KeyCode::ControlLeft,
            KeyCode::ShiftLeft,
            KeyCode::AltLeft,
            KeyCode::SuperLeft,
            KeyCode::ControlRight,
            KeyCode::ShiftRight,
            KeyCode::AltRight,
            KeyCode::SuperRight,
            KeyCode::ArrowUp,
            KeyCode::ArrowDown,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
        ];
        let mut seen = std::collections::HashSet::new();
        for code in codes {
            let usage = hid(code).expect("this key must be mapped");
            assert!(seen.insert(usage), "two keys map to usage {usage:#x}");
        }
    }

    #[test]
    fn control_characters_are_not_sent_as_text() {
        // Return arrives with "\r" attached; sending both would type the key
        // and then type it again as text.
        let event = key(
            PhysicalKey::Code(KeyCode::Enter),
            ElementState::Pressed,
            Some("\r"),
            Modifiers::NONE,
        )
        .unwrap();
        assert_eq!(event.key.0, 0x28);
        assert_eq!(event.unicode, 0);

        let event = key(
            PhysicalKey::Code(KeyCode::KeyA),
            ElementState::Pressed,
            Some("a"),
            Modifiers::NONE,
        )
        .unwrap();
        assert_eq!(event.unicode, u32::from('a'));
        assert_eq!(event.kind, InputKind::KeyDown);
    }

    #[test]
    fn pixel_scrolling_is_converted_to_lines() {
        let event = scroll(
            MouseScrollDelta::PixelDelta((0.0, 32.0).into()),
            (0.5, 0.5),
            Modifiers::NONE,
        );
        assert!((event.scroll_y - 2.0).abs() < 0.001);
        assert_eq!(event.kind, InputKind::Wheel);

        let event = scroll(
            MouseScrollDelta::LineDelta(0.0, 3.0),
            (0.5, 0.5),
            Modifiers::NONE,
        );
        assert!((event.scroll_y - 3.0).abs() < 0.001);
    }

    #[test]
    fn unnamed_buttons_and_keys_send_nothing() {
        assert!(button(WinitButton::Other(9)).is_none());
        assert!(key(
            PhysicalKey::Unidentified(winit::keyboard::NativeKeyCode::Unidentified),
            ElementState::Pressed,
            None,
            Modifiers::NONE,
        )
        .is_none());
    }
}
