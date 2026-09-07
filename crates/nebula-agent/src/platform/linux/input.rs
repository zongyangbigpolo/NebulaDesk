use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use ashpd::desktop::{
    remote_desktop::{Axis, KeyState, RemoteDesktop},
    Session,
};
use ndp_proto::{InputEvent, InputKind, MouseButton};

use super::portal::Portal;
use crate::media::InputInjector;

pub(super) struct LinuxInput {
    portal: Arc<Mutex<Option<Portal>>>,
}

impl LinuxInput {
    pub fn new(portal: Arc<Mutex<Option<Portal>>>) -> Self {
        Self { portal }
    }
}

impl InputInjector for LinuxInput {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        self.portal
            .lock()
            .map_err(|_| anyhow::anyhow!("portal state poisoned"))?
            .as_ref()
            .context("screen sharing has not started")?
            .inject(*event)
    }
}

#[derive(Default)]
pub(super) struct InputState {
    keys: BTreeSet<i32>,
    buttons: BTreeSet<i32>,
    wheel: [f64; 2],
}

impl InputState {
    pub async fn inject(
        &mut self,
        remote: &RemoteDesktop<'static>,
        session: &Session<'static, RemoteDesktop<'static>>,
        node: u32,
        (width, height): (u32, u32),
        event: &InputEvent,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            event.display == 0 && event.pointer_id == 0,
            "Linux portal supports one selected monitor and one pointer"
        );
        if event.kind.is_pointer() {
            anyhow::ensure!(
                event.x.is_finite() && event.y.is_finite(),
                "non-finite pointer position"
            );
            remote
                .notify_pointer_motion_absolute(
                    session,
                    node,
                    f64::from(event.x.clamp(0.0, 1.0)) * f64::from(width.saturating_sub(1)),
                    f64::from(event.y.clamp(0.0, 1.0)) * f64::from(height.saturating_sub(1)),
                )
                .await?;
        }
        match event.kind {
            InputKind::MouseDown | InputKind::MouseUp => {
                let code = button(event.button)?;
                let down = event.kind == InputKind::MouseDown;
                if down != self.buttons.contains(&code) {
                    remote
                        .notify_pointer_button(session, code, state(down))
                        .await?;
                    if down {
                        self.buttons.insert(code);
                    } else {
                        self.buttons.remove(&code);
                    }
                }
            }
            InputKind::KeyDown | InputKind::KeyUp => {
                let code =
                    evdev(event.key.0).context("unsupported USB keyboard usage for Linux")?;
                let down = event.kind == InputKind::KeyDown;
                // The compositor owns repeat. Forwarding client repeats as additional
                // presses also repeats shortcuts and leaves some compositors stuck.
                if down != self.keys.contains(&code) {
                    remote
                        .notify_keyboard_keycode(session, code, state(down))
                        .await?;
                    if down {
                        self.keys.insert(code);
                    } else {
                        self.keys.remove(&code);
                    }
                }
            }
            InputKind::Wheel => {
                for (index, delta, axis) in [
                    (0, event.scroll_x, Axis::Horizontal),
                    (1, -event.scroll_y, Axis::Vertical),
                ] {
                    anyhow::ensure!(
                        delta.is_finite() && delta.abs() <= 120.0,
                        "invalid wheel delta"
                    );
                    self.wheel[index] += f64::from(delta);
                    let steps = self.wheel[index].trunc() as i32;
                    if steps != 0 {
                        remote
                            .notify_pointer_axis_discrete(session, axis, steps)
                            .await?;
                        self.wheel[index] -= f64::from(steps);
                    }
                }
            }
            InputKind::PointerLeave => {
                for &key in &self.keys {
                    remote
                        .notify_keyboard_keycode(session, key, KeyState::Released)
                        .await?;
                }
                for &button in &self.buttons {
                    remote
                        .notify_pointer_button(session, button, KeyState::Released)
                        .await?;
                }
                self.keys.clear();
                self.buttons.clear();
                self.wheel = [0.0; 2];
            }
            InputKind::MouseMove | InputKind::MouseDrag => {}
        }
        Ok(())
    }
}

fn state(down: bool) -> KeyState {
    if down {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

fn button(button: MouseButton) -> anyhow::Result<i32> {
    Ok(match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x116,
        MouseButton::Forward => 0x115,
        MouseButton::None => anyhow::bail!("button event has no button"),
    })
}

/// USB keyboard page to Linux input-event-codes.h (not XKB's evdev + 8).
fn evdev(usage: u32) -> Option<i32> {
    const LETTERS: [i32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    Some(match usage {
        0x04..=0x1d => LETTERS[(usage - 4) as usize],
        0x1e..=0x26 => (usage - 0x1e + 2) as i32,
        0x27 => 11,
        0x28 => 28,
        0x29 => 1,
        0x2a => 14,
        0x2b => 15,
        0x2c => 57,
        0x2d => 12,
        0x2e => 13,
        0x2f => 26,
        0x30 => 27,
        0x31 => 43,
        0x32 => 43,
        0x33 => 39,
        0x34 => 40,
        0x35 => 41,
        0x36 => 51,
        0x37 => 52,
        0x38 => 53,
        0x39 => 58,
        0x3a..=0x43 => (usage - 0x3a + 59) as i32,
        0x44 => 87,
        0x45 => 88,
        0x46 => 99,
        0x47 => 70,
        0x48 => 119,
        0x49 => 110,
        0x4a => 102,
        0x4b => 104,
        0x4c => 111,
        0x4d => 107,
        0x4e => 109,
        0x4f => 106,
        0x50 => 105,
        0x51 => 108,
        0x52 => 103,
        0x53 => 69,
        0x54 => 98,
        0x55 => 55,
        0x56 => 74,
        0x57 => 78,
        0x58 => 96,
        0x59 => 79,
        0x5a => 80,
        0x5b => 81,
        0x5c => 75,
        0x5d => 76,
        0x5e => 77,
        0x5f => 71,
        0x60 => 72,
        0x61 => 73,
        0x62 => 82,
        0x63 => 83,
        0x64 => 86,
        0x65 => 127,
        0x66 => 116,
        0x67 => 117,
        0x68..=0x73 => (usage - 0x68 + 183) as i32,
        0x75 => 138,
        0x76 => 139,
        0x78 => 128,
        0x79 => 129,
        0x7a => 131,
        0x7b => 137,
        0x7c => 133,
        0x7d => 135,
        0x7e => 136,
        0x7f => 113,
        0x80 => 115,
        0x81 => 114,
        0x85 => 121,
        0x87 => 89,
        0x88 => 93,
        0x89 => 124,
        0x8a => 92,
        0x8b => 94,
        0x90 => 122,
        0x91 => 123,
        0xe0 => 29,
        0xe1 => 42,
        0xe2 => 56,
        0xe3 => 125,
        0xe4 => 97,
        0xe5 => 54,
        0xe6 => 100,
        0xe7 => 126,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_evdev_not_x11_or_usb_codes() {
        for (hid, expected) in [(4, 30), (0x28, 28), (0x4f, 106), (0xe3, 125), (0xe7, 126)] {
            assert_eq!(evdev(hid), Some(expected));
        }
        assert_eq!(evdev(0), None);
        assert_eq!(evdev(0xffff), None);
        assert_eq!(button(MouseButton::Right).unwrap(), 273);
        assert!(button(MouseButton::None).is_err());
    }
}
