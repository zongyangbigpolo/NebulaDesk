use crate::media::InputInjector;
use anyhow::{bail, ensure};
use ndp_proto::{InputEvent, InputKind, Modifiers, MouseButton};
use std::collections::{BTreeMap, BTreeSet};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

#[derive(Default)]
pub struct WindowsInput {
    held: BTreeMap<u32, Vec<KEYBDINPUT>>,
    synthetic_modifiers: BTreeSet<u32>,
    buttons: BTreeSet<u8>,
    wheel: [f64; 2],
}

fn send(inputs: &[INPUT]) -> anyhow::Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    ensure!(sent as usize == inputs.len(),
        "SendInput delivered {sent}/{} events; unlock the interactive desktop and run the agent at the target application's integrity level (Windows UIPI blocks injection into elevated apps)", inputs.len());
    Ok(())
}

fn keyboard(key: KEYBDINPUT, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                dwFlags: key.dwFlags
                    | if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                ..key
            },
        },
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, data: u32, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn button(button: u8, up: bool) -> anyhow::Result<INPUT> {
    let (down, release, data) = match button {
        1 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        2 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        3 => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        4 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        5 => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
        _ => bail!("mouse button is missing"),
    };
    Ok(mouse(if up { release } else { down }, data, 0, 0))
}

impl WindowsInput {
    fn release(&mut self, usage: u32) -> anyhow::Result<()> {
        if let Some(keys) = self.held.get(&usage) {
            send(
                &keys
                    .iter()
                    .map(|&key| keyboard(key, true))
                    .collect::<Vec<_>>(),
            )?;
            self.held.remove(&usage);
            self.synthetic_modifiers.remove(&usage);
        }
        Ok(())
    }

    fn press(&mut self, usage: u32, keys: Vec<KEYBDINPUT>) -> anyhow::Result<()> {
        // Remember before injection so Drop also releases partially delivered SendInput batches.
        self.held.insert(usage, keys.clone());
        send(
            &keys
                .into_iter()
                .map(|key| keyboard(key, false))
                .collect::<Vec<_>>(),
        )
    }

    fn modifiers(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        for (flag, left, right) in [
            (Modifiers::CONTROL, 0xe0, 0xe4),
            (Modifiers::SHIFT, 0xe1, 0xe5),
            (Modifiers::ALT, 0xe2, 0xe6),
            (Modifiers::META, 0xe3, 0xe7),
        ] {
            if matches!(event.kind, InputKind::KeyDown | InputKind::KeyUp)
                && matches!(event.key.0, key if key == left || key == right)
            {
                continue;
            }
            if !event.modifiers.contains(flag) {
                self.release(left)?;
                self.release(right)?;
            } else if !self.held.contains_key(&left) && !self.held.contains_key(&right) {
                self.press(left, vec![scan(left)?])?;
                self.synthetic_modifiers.insert(left);
            }
        }
        Ok(())
    }
}

impl InputInjector for WindowsInput {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        ensure!(
            event.display == 0 && event.pointer_id == 0,
            "Windows capture currently supports primary display/mouse only"
        );
        ensure!(
            [event.x, event.y, event.scroll_x, event.scroll_y]
                .iter()
                .all(|v| v.is_finite()),
            "non-finite pointer input"
        );
        if matches!(event.kind, InputKind::KeyDown | InputKind::KeyUp)
            && (0xe0..=0xe7).contains(&event.key.0)
        {
            let left = 0xe0 + (event.key.0 - 0xe0) % 4;
            if left != event.key.0 && self.synthetic_modifiers.contains(&left) {
                self.release(left)?;
            }
            self.synthetic_modifiers.remove(&event.key.0);
        }
        self.modifiers(event)?;
        if event.kind.is_pointer() {
            // Absolute without VIRTUALDESK maps to the primary display, exactly the WGC capture target.
            send(&[mouse(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                0,
                (event.x.clamp(0.0, 1.0) * 65535.0).round() as i32,
                (event.y.clamp(0.0, 1.0) * 65535.0).round() as i32,
            )])?;
        }
        match event.kind {
            InputKind::MouseDown | InputKind::MouseUp => {
                ensure!(event.button != MouseButton::None, "mouse button is missing");
                let up = event.kind == InputKind::MouseUp;
                if !up {
                    self.buttons.insert(event.button as u8);
                }
                send(&[button(event.button as u8, up)?])?;
                if up {
                    self.buttons.remove(&(event.button as u8));
                }
            }
            InputKind::Wheel => {
                for (axis, delta, flag) in [
                    (0, event.scroll_x, MOUSEEVENTF_HWHEEL),
                    (1, event.scroll_y, MOUSEEVENTF_WHEEL),
                ] {
                    self.wheel[axis] += f64::from(delta) * 120.0;
                    let ticks = self.wheel[axis]
                        .clamp(i32::MIN as f64, i32::MAX as f64)
                        .trunc() as i32;
                    if ticks != 0 {
                        send(&[mouse(flag, ticks as u32, 0, 0)])?;
                        self.wheel[axis] -= ticks as f64;
                    }
                }
            }
            InputKind::KeyUp => self.release(event.key.0)?,
            InputKind::KeyDown => {
                let keys = if let Some(keys) = self.held.get(&event.key.0) {
                    keys.clone()
                } else if event.unicode != 0
                    && ![Modifiers::CONTROL, Modifiers::ALT, Modifiers::META]
                        .iter()
                        .any(|&m| event.modifiers.contains(m))
                {
                    let character = char::from_u32(event.unicode)
                        .ok_or_else(|| anyhow::anyhow!("invalid Unicode input scalar"))?;
                    let mut utf16 = [0; 2];
                    character
                        .encode_utf16(&mut utf16)
                        .iter()
                        .map(|&unit| KEYBDINPUT {
                            wScan: unit,
                            dwFlags: KEYEVENTF_UNICODE,
                            ..Default::default()
                        })
                        .collect()
                } else {
                    vec![scan(event.key.0)?]
                };
                self.press(event.key.0, keys)?;
                if event.key.0 == 0 {
                    self.release(0)?;
                }
            }
            InputKind::PointerLeave | InputKind::MouseMove | InputKind::MouseDrag => {}
        }
        Ok(())
    }
}

impl Drop for WindowsInput {
    fn drop(&mut self) {
        for &usage in &self.held.keys().copied().collect::<Vec<_>>() {
            if let Err(error) = self.release(usage) {
                tracing::warn!(%error, "failed to release remote key");
            }
        }
        for &held in &self.buttons {
            if let Err(error) = button(held, true).and_then(|input| send(&[input])) {
                tracing::warn!(%error, "failed to release remote mouse button");
            }
        }
    }
}

pub(super) fn scan(usage: u32) -> anyhow::Result<KEYBDINPUT> {
    let virtual_key = match usage {
        0x75 => Some(VK_HELP),
        0x7f => Some(VK_VOLUME_MUTE),
        0x80 => Some(VK_VOLUME_UP),
        0x81 => Some(VK_VOLUME_DOWN),
        _ => None,
    };
    if let Some(key) = virtual_key {
        return Ok(KEYBDINPUT {
            wVk: key,
            ..Default::default()
        });
    }
    let code = scan_code(usage)
        .ok_or_else(|| anyhow::anyhow!("unsupported USB keyboard usage 0x{usage:02x}"))?;
    if code == 0xe145 {
        return Ok(KEYBDINPUT {
            wVk: VK_PAUSE,
            ..Default::default()
        });
    }
    Ok(KEYBDINPUT {
        wScan: code & 0xff,
        dwFlags: KEYEVENTF_SCANCODE
            | if code & 0xe000 != 0 {
                KEYEVENTF_EXTENDEDKEY
            } else {
                KEYBD_EVENT_FLAGS(0)
            },
        ..Default::default()
    })
}

fn scan_code(usage: u32) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c,
    ];
    Some(match usage {
        0x04..=0x1d => LETTERS[(usage - 4) as usize],
        0x1e..=0x26 => (usage - 0x1e + 2) as u16,
        0x27 => 0x0b,
        0x28 => 0x1c,
        0x29 => 0x01,
        0x2a => 0x0e,
        0x2b => 0x0f,
        0x2c => 0x39,
        0x2d => 0x0c,
        0x2e => 0x0d,
        0x2f => 0x1a,
        0x30 => 0x1b,
        0x31 | 0x32 => 0x2b,
        0x33 => 0x27,
        0x34 => 0x28,
        0x35 => 0x29,
        0x36 => 0x33,
        0x37 => 0x34,
        0x38 => 0x35,
        0x39 => 0x3a,
        0x3a..=0x43 => (usage - 0x3a + 0x3b) as u16,
        0x44 => 0x57,
        0x45 => 0x58,
        0x46 => 0xe037,
        0x47 => 0x46,
        0x48 => 0xe145,
        0x49 => 0xe052,
        0x4a => 0xe047,
        0x4b => 0xe049,
        0x4c => 0xe053,
        0x4d => 0xe04f,
        0x4e => 0xe051,
        0x4f => 0xe04d,
        0x50 => 0xe04b,
        0x51 => 0xe050,
        0x52 => 0xe048,
        0x53 => 0x45,
        0x54 => 0xe035,
        0x55 => 0x37,
        0x56 => 0x4a,
        0x57 => 0x4e,
        0x58 => 0xe01c,
        0x59 => 0x4f,
        0x5a => 0x50,
        0x5b => 0x51,
        0x5c => 0x4b,
        0x5d => 0x4c,
        0x5e => 0x4d,
        0x5f => 0x47,
        0x60 => 0x48,
        0x61 => 0x49,
        0x62 => 0x52,
        0x63 => 0x53,
        0x64 => 0x56,
        0x65 => 0xe05d,
        0x67 => 0x59,
        0x68..=0x72 => (usage - 0x68 + 0x64) as u16,
        0x73 => 0x76,
        0x87 => 0x73,
        0x89 => 0x7d,
        0x8a => 0x79,
        0x8b => 0x7b,
        0x8c => 0x5c,
        0x90 => 0xf2,
        0x91 => 0xf1,
        0x92 => 0x70,
        0x93 => 0x78,
        0x94 => 0x77,
        0xe0 => 0x1d,
        0xe1 => 0x2a,
        0xe2 => 0x38,
        0xe3 => 0xe05b,
        0xe4 => 0xe01d,
        0xe5 => 0x36,
        0xe6 => 0xe038,
        0xe7 => 0xe05c,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hid_is_not_a_windows_virtual_key() {
        assert_eq!(scan_code(0x04), Some(0x1e));
        assert_eq!(scan_code(0x1e), Some(0x02));
        assert_eq!(scan_code(0x50), Some(0xe04b));
        assert_eq!(scan_code(0xe4), Some(0xe01d));
        assert_eq!(scan_code(0x58), Some(0xe01c));
        assert_eq!(scan_code(0x48), Some(0xe145));
        assert_eq!(scan_code(0), None);
        assert_eq!(scan_code(0xe8), None);
    }

    #[test]
    fn extended_keys_and_releases_keep_their_flags() {
        let right_control = scan(0xe4).unwrap();
        assert_eq!(right_control.wScan, 0x1d);
        assert!(right_control.dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
        let release = keyboard(right_control, true);
        let key = unsafe { release.Anonymous.ki };
        assert!(key
            .dwFlags
            .contains(KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP));
        assert_eq!(scan(0x7f).unwrap().wVk, VK_VOLUME_MUTE);
        assert!(!scan(0xe1).unwrap().dwFlags.contains(KEYEVENTF_EXTENDEDKEY));
    }

    #[test]
    fn extra_buttons_use_xbutton_data() {
        let back = unsafe { button(4, false).unwrap().Anonymous.mi };
        assert_eq!(back.mouseData, 1);
        assert_eq!(back.dwFlags, MOUSEEVENTF_XDOWN);
        let forward = unsafe { button(5, true).unwrap().Anonymous.mi };
        assert_eq!(forward.mouseData, 2);
        assert_eq!(forward.dwFlags, MOUSEEVENTF_XUP);
        assert!(button(0, false).is_err());
    }
}
