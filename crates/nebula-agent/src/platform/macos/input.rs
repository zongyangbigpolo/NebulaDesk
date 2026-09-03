//! Input injection on macOS.
//!
//! Everything here goes through `CGEvent` posted to the HID event tap, which
//! is the same path a physical device takes: events land in the window server
//! before any application sees them, so they work in every app rather than
//! only in ones that cooperate.
//!
//! This requires the Accessibility permission. macOS grants it per signed
//! binary and then silently drops events without it, giving no error at the
//! call site — a session where the picture moves but the mouse does not is
//! otherwise a very confusing thing to debug.

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use ndp_proto::{InputEvent, InputKind, KeyCode, Modifiers, MouseButton};

use crate::media::InputInjector;

/// Injects input into the local desktop.
///
/// CoreGraphics event objects are not safe to move between threads, and the
/// session that feeds this injector is a tokio task that can be scheduled
/// anywhere. So every CoreGraphics call is confined to one thread of its own
/// and reached by channel. That also serialises injection, which is required
/// for correctness rather than merely tidy: two events posted concurrently
/// can arrive at the window server out of order, turning a click-drag into a
/// click somewhere else.
pub struct MacInput {
    events: Option<std::sync::mpsc::Sender<InputEvent>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MacInput {
    /// Start the injection thread.
    pub fn new() -> anyhow::Result<Self> {
        let (events, inbox) = std::sync::mpsc::channel::<InputEvent>();
        let (ready, started) = std::sync::mpsc::channel::<anyhow::Result<()>>();

        let worker = std::thread::Builder::new()
            .name("nebula-input".into())
            .spawn(move || {
                let mut desktop = match Desktop::new() {
                    Ok(desktop) => {
                        let _ = ready.send(Ok(()));
                        desktop
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                // Ends when the sender is dropped, which is what releases
                // whatever the session left held.
                while let Ok(event) = inbox.recv() {
                    if let Err(error) = desktop.apply(&event) {
                        tracing::debug!(%error, "could not inject an event");
                    }
                }
            })?;

        started
            .recv()
            .map_err(|_| anyhow::anyhow!("the input thread died before it started"))??;

        Ok(Self {
            events: Some(events),
            worker: Some(worker),
        })
    }
}

impl Drop for MacInput {
    fn drop(&mut self) {
        drop(self.events.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl InputInjector for MacInput {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        self.events
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the input thread has shut down"))?
            .send(*event)
            .map_err(|_| anyhow::anyhow!("the input thread has shut down"))
    }
}

/// The CoreGraphics state, owned by the injection thread and never leaving it.
struct Desktop {
    source: CGEventSource,
    /// The desktop area input is mapped onto, in global display space.
    bounds: (f64, f64, f64, f64),
    /// Where the pointer was last put, so a button event with no preceding
    /// move still lands somewhere sensible.
    last: CGPoint,
    /// Which buttons this session believes are down, so they can be released
    /// if it ends mid-drag.
    held: Vec<CGMouseButton>,
}

impl Desktop {
    fn new() -> anyhow::Result<Self> {
        // `HIDSystemState` makes injected events indistinguishable from real
        // ones to applications, which is what makes modifier-aware apps
        // behave. A private state would be visible as synthetic.
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| anyhow::anyhow!("could not create a CoreGraphics event source"))?;

        let frame = CGDisplay::main().bounds();
        Ok(Self {
            source,
            bounds: (
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ),
            last: CGPoint::new(
                frame.origin.x + frame.size.width / 2.0,
                frame.origin.y + frame.size.height / 2.0,
            ),
            held: Vec::new(),
        })
    }

    /// Turn a normalised position into a point on the target display.
    ///
    /// Clamped rather than rejected: a client whose window is slightly larger
    /// than the stream produces values just outside the range, and a pointer
    /// that sticks to the edge is what a user expects.
    fn point(&self, x: f32, y: f32) -> CGPoint {
        let (ox, oy, w, h) = self.bounds;
        CGPoint::new(
            ox + f64::from(x.clamp(0.0, 1.0)) * w,
            oy + f64::from(y.clamp(0.0, 1.0)) * h,
        )
    }

    /// Release anything this session left held.
    fn release_all(&mut self) {
        let held = std::mem::take(&mut self.held);
        for button in held {
            let up = match button {
                CGMouseButton::Left => CGEventType::LeftMouseUp,
                CGMouseButton::Right => CGEventType::RightMouseUp,
                CGMouseButton::Center => CGEventType::OtherMouseUp,
            };
            if let Ok(event) = CGEvent::new_mouse_event(self.source.clone(), up, self.last, button)
            {
                event.post(CGEventTapLocation::HID);
            }
        }
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // A session that ends mid-drag must not leave a button stuck down;
        // the next person to sit at this machine would find a desktop that
        // behaves as though possessed.
        self.release_all();
    }
}

impl Desktop {
    fn apply(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        match event.kind {
            InputKind::MouseMove | InputKind::MouseDrag => {
                let point = self.point(event.x, event.y);
                self.last = point;
                let (kind, button) = match (event.kind, cg_button(event.button)) {
                    (InputKind::MouseDrag, Some(CGMouseButton::Left)) => {
                        (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                    }
                    (InputKind::MouseDrag, Some(CGMouseButton::Right)) => {
                        (CGEventType::RightMouseDragged, CGMouseButton::Right)
                    }
                    (InputKind::MouseDrag, Some(CGMouseButton::Center)) => {
                        (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                    }
                    _ => (CGEventType::MouseMoved, CGMouseButton::Left),
                };
                let cg = CGEvent::new_mouse_event(self.source.clone(), kind, point, button)
                    .map_err(|()| anyhow::anyhow!("could not build a mouse event"))?;
                apply_modifiers(&cg, event.modifiers);
                cg.post(CGEventTapLocation::HID);
            }

            InputKind::MouseDown | InputKind::MouseUp => {
                let Some(button) = cg_button(event.button) else {
                    return Ok(());
                };
                let point = self.point(event.x, event.y);
                self.last = point;
                let down = event.kind == InputKind::MouseDown;
                let kind = match (button, down) {
                    (CGMouseButton::Left, true) => CGEventType::LeftMouseDown,
                    (CGMouseButton::Left, false) => CGEventType::LeftMouseUp,
                    (CGMouseButton::Right, true) => CGEventType::RightMouseDown,
                    (CGMouseButton::Right, false) => CGEventType::RightMouseUp,
                    (CGMouseButton::Center, true) => CGEventType::OtherMouseDown,
                    (CGMouseButton::Center, false) => CGEventType::OtherMouseUp,
                };
                let cg = CGEvent::new_mouse_event(self.source.clone(), kind, point, button)
                    .map_err(|()| anyhow::anyhow!("could not build a mouse event"))?;
                apply_modifiers(&cg, event.modifiers);

                // Click state is what turns two clicks into a double click.
                // Without it no application will ever see one.
                cg.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, 1);
                cg.post(CGEventTapLocation::HID);

                if down {
                    if !self.held.iter().any(|b| *b as u32 == button as u32) {
                        self.held.push(button);
                    }
                } else {
                    self.held.retain(|b| *b as u32 != button as u32);
                }
            }

            InputKind::Wheel => {
                // Line units, not pixels: the wire carries lines, and macOS
                // applies its own acceleration and direction preferences to
                // line-based scrolls exactly as it would for a real wheel.
                post_scroll(
                    event.scroll_y.round() as i32,
                    event.scroll_x.round() as i32,
                    event.modifiers,
                );
            }

            InputKind::KeyDown | InputKind::KeyUp => {
                let Some(code) = virtual_key(event.key) else {
                    tracing::trace!(hid = event.key.0, "no macOS key for this HID usage");
                    return Ok(());
                };
                let down = event.kind == InputKind::KeyDown;
                let cg = CGEvent::new_keyboard_event(self.source.clone(), code, down)
                    .map_err(|()| anyhow::anyhow!("could not build a key event"))?;
                apply_modifiers(&cg, event.modifiers);
                cg.post(CGEventTapLocation::HID);
            }

            InputKind::PointerLeave => self.release_all(),
        }
        Ok(())
    }
}

/// Post a scroll event.
///
/// `core-graphics` does not wrap `CGEventCreateScrollWheelEvent`, so it is
/// declared here. Line units, not pixels: the wire carries lines, and macOS
/// then applies its own acceleration and natural-direction preference exactly
/// as it would for a real wheel, which is what makes scrolling feel local.
fn post_scroll(vertical: i32, horizontal: i32, modifiers: Modifiers) {
    // Units: 0 is pixels, 1 is lines.
    const LINE: u32 = 1;
    // The HID tap, the same place physical devices deliver to.
    const TAP_HID: u32 = 0;

    unsafe {
        // A null source means "no particular device", which is what a
        // synthesised scroll should look like; the wheel deltas carry all the
        // meaning here.
        let event = CGEventCreateScrollWheelEvent(std::ptr::null(), LINE, 2, vertical, horizontal);
        if event.is_null() {
            return;
        }
        CGEventSetFlags(event, cg_flags(modifiers).bits());
        CGEventPost(TAP_HID, event);
        CFRelease(event);
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateScrollWheelEvent(
        source: *const std::ffi::c_void,
        units: u32,
        wheel_count: u32,
        ...
    ) -> *mut std::ffi::c_void;
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CGEventSetFlags(event: *mut std::ffi::c_void, flags: u64);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *mut std::ffi::c_void);
}

/// Set the modifier flags a synthesised event is seen with.
///
/// Modifiers travel with every event rather than as separate key presses, so
/// a release lost on the way cannot leave this machine stuck in, say,
/// permanent Command.
fn apply_modifiers(event: &CGEvent, modifiers: Modifiers) {
    event.set_flags(cg_flags(modifiers));
}

/// Translate wire modifiers into CoreGraphics event flags.
fn cg_flags(modifiers: Modifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if modifiers.contains(Modifiers::SHIFT) {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers.contains(Modifiers::ALT) {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if modifiers.contains(Modifiers::META) {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    if modifiers.contains(Modifiers::CAPS_LOCK) {
        flags |= CGEventFlags::CGEventFlagAlphaShift;
    }
    if modifiers.contains(Modifiers::FN) {
        flags |= CGEventFlags::CGEventFlagSecondaryFn;
    }
    flags
}

fn cg_button(button: MouseButton) -> Option<CGMouseButton> {
    match button {
        MouseButton::Left => Some(CGMouseButton::Left),
        MouseButton::Right => Some(CGMouseButton::Right),
        MouseButton::Middle => Some(CGMouseButton::Center),
        // Back and forward have no CoreGraphics constant; posting them would
        // mean an other-button event carrying a button number, which this API
        // does not expose. Dropping beats sending the wrong button.
        MouseButton::Back | MouseButton::Forward | MouseButton::None => None,
    }
}

/// Map a USB HID keyboard usage to a macOS virtual key code.
///
/// The wire carries HID usages precisely so neither end has to know the
/// other's numbering; this table is where that promise is paid for on macOS.
/// The `kVK_*` values are positional, so the mapping holds whatever layout
/// either side is using.
fn virtual_key(key: KeyCode) -> Option<u16> {
    let vk: u16 = match key.0 {
        // Letters, HID a..z.
        0x04 => 0x00,
        0x05 => 0x0B,
        0x06 => 0x08,
        0x07 => 0x02,
        0x08 => 0x0E,
        0x09 => 0x03,
        0x0A => 0x05,
        0x0B => 0x04,
        0x0C => 0x22,
        0x0D => 0x26,
        0x0E => 0x28,
        0x0F => 0x25,
        0x10 => 0x2E,
        0x11 => 0x2D,
        0x12 => 0x1F,
        0x13 => 0x23,
        0x14 => 0x0C,
        0x15 => 0x0F,
        0x16 => 0x01,
        0x17 => 0x11,
        0x18 => 0x20,
        0x19 => 0x09,
        0x1A => 0x0D,
        0x1B => 0x07,
        0x1C => 0x10,
        0x1D => 0x06,

        // Digits, HID 1..9 then 0.
        0x1E => 0x12,
        0x1F => 0x13,
        0x20 => 0x14,
        0x21 => 0x15,
        0x22 => 0x17,
        0x23 => 0x16,
        0x24 => 0x1A,
        0x25 => 0x1C,
        0x26 => 0x19,
        0x27 => 0x1D,

        0x28 => 0x24, // Return
        0x29 => 0x35, // Escape
        0x2A => 0x33, // Backspace
        0x2B => 0x30, // Tab
        0x2C => 0x31, // Space
        0x2D => 0x1B, // -
        0x2E => 0x18, // =
        0x2F => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // backslash
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        0x39 => 0x39, // Caps Lock

        // Function keys F1..F12.
        0x3A => 0x7A,
        0x3B => 0x78,
        0x3C => 0x63,
        0x3D => 0x76,
        0x3E => 0x60,
        0x3F => 0x61,
        0x40 => 0x62,
        0x41 => 0x64,
        0x42 => 0x65,
        0x43 => 0x6D,
        0x44 => 0x67,
        0x45 => 0x6F,

        0x49 => 0x72, // Insert / Help
        0x4A => 0x73, // Home
        0x4B => 0x74, // Page Up
        0x4C => 0x75, // Forward Delete
        0x4D => 0x77, // End
        0x4E => 0x79, // Page Down
        0x4F => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up

        // Keypad.
        0x53 => 0x47, // Num Lock / Clear
        0x54 => 0x4B, // divide
        0x55 => 0x43, // multiply
        0x56 => 0x4E, // minus
        0x57 => 0x45, // plus
        0x58 => 0x4C, // Enter
        0x59 => 0x53,
        0x5A => 0x54,
        0x5B => 0x55,
        0x5C => 0x56,
        0x5D => 0x57,
        0x5E => 0x58,
        0x5F => 0x59,
        0x60 => 0x5B,
        0x61 => 0x5C,
        0x62 => 0x52,
        0x63 => 0x41, // decimal

        // Modifiers, when sent as keys in their own right.
        0xE0 => 0x3B,
        0xE1 => 0x38,
        0xE2 => 0x3A,
        0xE3 => 0x37,
        0xE4 => 0x3E,
        0xE5 => 0x3C,
        0xE6 => 0x3D,
        0xE7 => 0x36,

        _ => return None,
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_table_never_maps_two_usages_to_one_key() {
        // A collision would make one usage silently type the wrong character,
        // which is the sort of bug that only appears for the one person with
        // that keyboard.
        let mut seen = std::collections::HashMap::new();
        for hid in 0u32..=0xFF {
            if let Some(vk) = virtual_key(KeyCode(hid)) {
                if let Some(previous) = seen.insert(vk, hid) {
                    panic!("HID {hid:#x} and {previous:#x} both map to virtual key {vk:#x}");
                }
            }
        }
        assert!(
            seen.len() >= 90,
            "the table should cover a full keyboard, found {}",
            seen.len()
        );
    }

    #[test]
    fn the_keys_are_where_a_mac_puts_them() {
        // Spot-checked against the kVK_ANSI_* constants: an off-by-one would
        // still pass a collision test.
        assert_eq!(virtual_key(KeyCode::A), Some(0x00));
        assert_eq!(virtual_key(KeyCode::Z), Some(0x06));
        assert_eq!(virtual_key(KeyCode::SPACE), Some(0x31));
        assert_eq!(virtual_key(KeyCode::ENTER), Some(0x24));
        assert_eq!(virtual_key(KeyCode::ESCAPE), Some(0x35));
        assert_eq!(virtual_key(KeyCode::ARROW_LEFT), Some(0x7B));
    }

    #[test]
    fn an_unknown_usage_is_dropped_rather_than_guessed() {
        assert_eq!(virtual_key(KeyCode::NONE), None);
        assert_eq!(virtual_key(KeyCode(0xFFFF)), None);
    }
}
