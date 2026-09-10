//! Targeted window messages only: never inject global desktop input in APP mode.
//!
//! This supports ordinary Win32 controls/text. Raw Input, global modifier state,
//! IME composition and applications that reject synthetic messages are unsupported.
use std::{collections::BTreeSet, sync::Arc};

use anyhow::{bail, ensure};
use ndp_proto::{InputEvent, InputKind, Modifiers, MouseButton};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, RECT, WPARAM},
    Graphics::Gdi::ScreenToClient,
    UI::{
        Input::KeyboardAndMouse::{MapVirtualKeyW, KEYEVENTF_EXTENDEDKEY, MAPVK_VSC_TO_VK_EX},
        WindowsAndMessaging::*,
    },
};

use super::{application_native::Window, application_policy::window_point};

pub(super) struct WindowInput {
    window: Arc<Window>,
    held: BTreeSet<u32>,
    buttons: usize,
    geometry: Option<RECT>,
    key_target: Option<isize>,
}

impl WindowInput {
    pub fn new(window: Arc<Window>) -> Self {
        Self {
            window,
            held: BTreeSet::new(),
            buttons: 0,
            geometry: None,
            key_target: None,
        }
    }

    fn target(&self) -> anyhow::Result<HWND> {
        self.window.validate()?;
        let root = self.window.hwnd();
        ensure!(
            unsafe { GetForegroundWindow() } == root,
            "application surface is not foreground"
        );
        ensure!(
            !unsafe { IsIconic(root).as_bool() },
            "application surface is minimized"
        );
        let thread = unsafe { GetWindowThreadProcessId(root, None) };
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            GetGUIThreadInfo(thread, &mut info)?;
        }
        self.check_child(info.hwndFocus)?;
        Ok(info.hwndFocus)
    }

    fn check_child(&self, target: HWND) -> anyhow::Result<()> {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(target, Some(&mut pid));
        }
        ensure!(
            pid == self.window.instance.pid
                && unsafe { GetAncestor(target, GA_ROOT) } == self.window.hwnd(),
            "input child does not belong to this application surface"
        );
        Ok(())
    }

    fn send(
        &self,
        target: HWND,
        message: u32,
        data: usize,
        parameter: isize,
    ) -> anyhow::Result<()> {
        let focus = self.target()?;
        self.check_child(target)?;
        if matches!(message, WM_KEYDOWN | WM_KEYUP | WM_CHAR) {
            ensure!(focus == target, "application keyboard focus changed");
        }
        ensure!(
            self.geometry == Some(self.window.bounds()?),
            "application geometry changed before message delivery"
        );
        let mut result = 0;
        let delivered = unsafe {
            SendMessageTimeoutW(
                target,
                message,
                WPARAM(data),
                LPARAM(parameter),
                SMTO_ABORTIFHUNG | SMTO_BLOCK | SMTO_ERRORONEXIT,
                100,
                Some(&mut result),
            )
        };
        ensure!(
            delivered.0 != 0,
            "owned application did not accept scoped input within 100 ms"
        );
        Ok(())
    }

    pub fn inject(&mut self, event: &InputEvent, expected: RECT) -> anyhow::Result<()> {
        ensure!(
            event.pointer_id == 0,
            "application input supports one pointer"
        );
        ensure!(
            [event.x, event.y, event.scroll_x, event.scroll_y]
                .iter()
                .all(|v| v.is_finite()),
            "non-finite application input"
        );
        ensure!(
            self.window.bounds()? == expected,
            "window geometry changed before input"
        );
        let focus = self.target()?;
        self.geometry = Some(expected);
        ensure!(
            self.held.is_empty() || self.key_target == Some(focus.0 as isize),
            "application keyboard focus changed while a key was held"
        );
        ensure!(
            ![Modifiers::CONTROL, Modifiers::ALT, Modifiers::META]
                .iter()
                .any(|flag| event.modifiers.contains(*flag)),
            "global modifier shortcuts are unavailable in scoped Windows message input"
        );
        match event.kind {
            InputKind::KeyDown | InputKind::KeyUp => {
                ensure!(
                    !(0xe0..=0xe7).contains(&event.key.0),
                    "global modifier state is unavailable in application message input"
                );
                if event.unicode != 0 {
                    if event.kind == InputKind::KeyDown {
                        let character = char::from_u32(event.unicode)
                            .ok_or_else(|| anyhow::anyhow!("invalid Unicode scalar"))?;
                        let mut utf16 = [0; 2];
                        for unit in character.encode_utf16(&mut utf16) {
                            self.send(focus, WM_CHAR, usize::from(*unit), 1)?;
                        }
                    }
                    return Ok(());
                }
                ensure!(
                    !event.modifiers.contains(Modifiers::SHIFT),
                    "shifted non-text input requires a scoped native input provider"
                );
                let key = super::input::scan(event.key.0)?;
                let scan = u32::from(key.wScan)
                    | if key.dwFlags.contains(KEYEVENTF_EXTENDEDKEY) {
                        0xe000
                    } else {
                        0
                    };
                let vk = if key.wVk.0 != 0 {
                    u32::from(key.wVk.0)
                } else {
                    unsafe { MapVirtualKeyW(scan, MAPVK_VSC_TO_VK_EX) }
                };
                ensure!(vk != 0, "unmapped application key");
                let up = event.kind == InputKind::KeyUp;
                let parameter = 1
                    | ((scan & 0xff) << 16)
                    | if scan & 0xe000 != 0 { 1 << 24 } else { 0 }
                    | if up {
                        3 << 30
                    } else if self.held.contains(&vk) {
                        1 << 30
                    } else {
                        0
                    };
                if up {
                    self.held.remove(&vk);
                } else {
                    self.key_target = Some(focus.0 as isize);
                    self.held.insert(vk);
                }
                self.send(
                    focus,
                    if up { WM_KEYUP } else { WM_KEYDOWN },
                    vk as usize,
                    parameter as isize,
                )?;
            }
            InputKind::PointerLeave => {
                // WM_MOUSELEAVE is declared by the optional common-controls binding.
                self.send(focus, 0x02a3, 0, 0)?;
            }
            _ => {
                let (x, y) = window_point(
                    [expected.left, expected.top, expected.right, expected.bottom],
                    event.x,
                    event.y,
                )
                .map_err(anyhow::Error::msg)?;
                let screen = POINT { x, y };
                let target = unsafe { WindowFromPoint(screen) };
                self.check_child(target)?;
                let mut point = screen;
                ensure!(
                    unsafe { ScreenToClient(target, &mut point).as_bool() },
                    "cannot resolve input geometry"
                );
                let mut client = RECT::default();
                unsafe {
                    GetClientRect(target, &mut client)?;
                }
                ensure!(
                    point.x >= client.left
                        && point.x < client.right
                        && point.y >= client.top
                        && point.y < client.bottom,
                    "non-client chrome uses scoped lifecycle commands, not desktop input"
                );
                let (message, bit) = match (event.kind, event.button) {
                    (InputKind::MouseDown, MouseButton::Left) => (WM_LBUTTONDOWN, 1),
                    (InputKind::MouseUp, MouseButton::Left) => (WM_LBUTTONUP, 1),
                    (InputKind::MouseDown, MouseButton::Right) => (WM_RBUTTONDOWN, 2),
                    (InputKind::MouseUp, MouseButton::Right) => (WM_RBUTTONUP, 2),
                    (InputKind::MouseDown, MouseButton::Middle) => (WM_MBUTTONDOWN, 16),
                    (InputKind::MouseUp, MouseButton::Middle) => (WM_MBUTTONUP, 16),
                    (InputKind::MouseMove | InputKind::MouseDrag, _) => (WM_MOUSEMOVE, 0),
                    (InputKind::Wheel, _) => {
                        let parameter = packed(screen)?;
                        for (delta, message) in [
                            (event.scroll_x, WM_MOUSEHWHEEL),
                            (event.scroll_y, WM_MOUSEWHEEL),
                        ] {
                            let ticks = (delta * 120.0).round();
                            ensure!(
                                ticks >= i16::MIN as f32 && ticks <= i16::MAX as f32,
                                "wheel delta exceeds native range"
                            );
                            if ticks != 0.0 {
                                self.send(
                                    target,
                                    message,
                                    self.buttons | ((ticks as i16 as u16 as usize) << 16),
                                    parameter,
                                )?;
                            }
                        }
                        return Ok(());
                    }
                    _ => bail!("unsupported application pointer button"),
                };
                if event.kind == InputKind::MouseDown {
                    self.buttons |= bit;
                }
                if event.kind == InputKind::MouseUp {
                    self.buttons &= !bit;
                }
                self.send(target, message, self.buttons, packed(point)?)?;
            }
        }
        Ok(())
    }

    pub fn release(&mut self) {
        // Message input never changes global key/button state. Do not send
        // releases to a new focus recipient when the local user takes control.
        if let Ok(target) = self.target() {
            if self.key_target == Some(target.0 as isize) {
                for key in std::mem::take(&mut self.held) {
                    let _ = self.send(target, WM_KEYUP, key as usize, (3u32 << 30) as isize);
                }
            }
            if self.buttons != 0 {
                let _ = self.send(target, WM_CANCELMODE, 0, 0);
            }
        }
        self.held.clear();
        self.buttons = 0;
        self.key_target = None;
        self.geometry = None;
    }
}

fn packed(point: POINT) -> anyhow::Result<isize> {
    let x = i16::try_from(point.x)
        .map_err(|_| anyhow::anyhow!("input X exceeds native message range"))?;
    let y = i16::try_from(point.y)
        .map_err(|_| anyhow::anyhow!("input Y exceeds native message range"))?;
    Ok(((x as u16 as u32) | ((y as u16 as u32) << 16)) as isize)
}

impl Drop for WindowInput {
    fn drop(&mut self) {
        self.release();
    }
}
