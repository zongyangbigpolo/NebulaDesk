use std::collections::BTreeMap;

use anyhow::{ensure, Context};
use ndp_proto::{application::ApplicationMessage, InputEvent};
use nebula_common::{
    application::ApplicationUnavailableReason, ApplicationCapability, ApplicationLaunch,
};
use windows::Win32::{
    Foundation::RECT,
    UI::{
        HiDpi::{
            GetDpiForWindow, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        },
        Input::KeyboardAndMouse::IsWindowEnabled,
        WindowsAndMessaging::{IsIconic, IsWindowVisible, SW_MINIMIZE},
    },
};

use super::{
    application_input::WindowInput,
    application_native::{Instance, Windows},
};
use crate::{
    application::{ApplicationBackend, NativeSurface},
    media::VideoSource,
};

pub(super) fn capability() -> ApplicationCapability {
    // Standard HWND messages cannot provide the full negotiated keyboard/IME/
    // Raw Input contract. Do not advertise a partial provider as a complete APP
    // backend, or silently replace scoped delivery with global SendInput.
    ApplicationCapability {
        reason: Some(ApplicationUnavailableReason::IsolationUnavailable),
        ..ApplicationCapability::default()
    }
}

/// Implemented native foundation, deliberately not advertised until the
/// platform has a complete scoped-input capability negotiation/provider.
pub(super) struct Application {
    windows: Windows,
    geometry: BTreeMap<u64, (RECT, u32, bool)>,
    input: BTreeMap<u64, WindowInput>,
    stopped: bool,
}

impl Application {
    pub fn launch(launch: &ApplicationLaunch) -> anyhow::Result<Self> {
        launch.validate()?;
        let instance = Instance::launch(
            &launch.launch_path,
            &launch.launch_args,
            launch.working_dir.as_deref(),
        )?;
        Ok(Self {
            windows: Windows::new(instance),
            geometry: BTreeMap::new(),
            input: BTreeMap::new(),
            stopped: false,
        })
    }

    fn live(
        &self,
        native_id: u64,
        generation: u32,
    ) -> anyhow::Result<(std::sync::Arc<super::application_native::Window>, RECT)> {
        ensure!(!self.stopped, "application session is stopped");
        let (_, window) = self
            .windows
            .known
            .values()
            .find(|(id, _)| *id == native_id)
            .context("application surface is closed or unknown")?;
        let (expected, current, minimized) = self
            .geometry
            .get(&native_id)
            .context("application surface geometry is unavailable")?;
        ensure!(
            generation == *current,
            "stale application geometry generation"
        );
        ensure!(
            window.bounds()? == *expected
                && unsafe { IsIconic(window.hwnd()).as_bool() } == *minimized,
            "application geometry changed since the last snapshot"
        );
        Ok((window.clone(), *expected))
    }
}

impl ApplicationBackend for Application {
    fn unavailable(&self) -> Vec<u64> {
        self.windows
            .known
            .values()
            .filter_map(|(id, window)| {
                (window.validate().is_ok()
                    && !super::application_policy::capture_available(
                        unsafe { IsWindowVisible(window.hwnd()).as_bool() },
                        unsafe { IsIconic(window.hwnd()).as_bool() },
                    ))
                .then_some(*id)
            })
            .collect()
    }

    fn snapshot(&mut self) -> anyhow::Result<Vec<NativeSurface>> {
        let _dpi = DpiScope::new()?;
        ensure!(!self.stopped, "application session is stopped");
        self.windows.refresh()?;
        let mut surfaces = Vec::new();
        for (id, window) in self.windows.known.values() {
            let minimized = unsafe { IsIconic(window.hwnd()).as_bool() };
            let available = super::application_policy::capture_available(
                unsafe { IsWindowVisible(window.hwnd()).as_bool() },
                minimized,
            );
            let bounds = match window.bounds() {
                Ok(bounds) => bounds,
                Err(_) if !available && self.geometry.contains_key(id) => self.geometry[id].0,
                Err(error) => return Err(error),
            };
            let geometry = self.geometry.entry(*id).or_insert((bounds, 1, minimized));
            if geometry.0 != bounds || geometry.2 != minimized {
                geometry.0 = bounds;
                geometry.1 = geometry
                    .1
                    .checked_add(1)
                    .context("application geometry generation exhausted")?;
                geometry.2 = minimized;
                if let Some(input) = self.input.get_mut(id) {
                    input.release();
                }
            }
            let parent = window
                .owner()
                .and_then(|owner| self.windows.known.get(&owner).map(|(id, _)| *id));
            let modal = window
                .owner()
                .and_then(|owner| self.windows.known.get(&owner))
                .is_some_and(|(_, owner)| !unsafe { IsWindowEnabled(owner.hwnd()).as_bool() });
            let dpi = unsafe { GetDpiForWindow(window.hwnd()) };
            ensure!(dpi != 0, "application window DPI is unavailable");
            surfaces.push(NativeSurface {
                native_id: *id,
                parent,
                title: window.title()?,
                width: u32::try_from(i64::from(bounds.right) - i64::from(bounds.left))?,
                height: u32::try_from(i64::from(bounds.bottom) - i64::from(bounds.top))?,
                scale: dpi as f32 / 96.0,
                geometry_generation: geometry.1,
                modal,
                minimized,
            });
        }
        self.geometry
            .retain(|id, _| surfaces.iter().any(|surface| surface.native_id == *id));
        self.input
            .retain(|id, _| surfaces.iter().any(|surface| surface.native_id == *id));
        Ok(surfaces)
    }

    fn video(&mut self, native_id: u64) -> anyhow::Result<Box<dyn VideoSource>> {
        ensure!(!self.stopped, "application session is stopped");
        let (_, window) = self
            .windows
            .known
            .values()
            .find(|(id, _)| *id == native_id)
            .context("unknown application capture surface")?;
        ensure!(
            super::application_policy::capture_available(
                unsafe { IsWindowVisible(window.hwnd()).as_bool() },
                unsafe { IsIconic(window.hwnd()).as_bool() }
            ),
            "hidden or minimized application capture is paused"
        );
        Ok(Box::new(super::capture::WindowsVideo::for_window(
            window.clone(),
        )?))
    }

    fn input(&mut self, native_id: u64, generation: u32, event: &InputEvent) -> anyhow::Result<()> {
        let _dpi = DpiScope::new()?;
        let result = (|| {
            let (window, bounds) = self.live(native_id, generation)?;
            self.input
                .entry(native_id)
                .or_insert_with(|| WindowInput::new(window))
                .inject(event, bounds)
        })();
        if result.is_err() {
            self.release_input();
        }
        result
    }

    fn operate(&mut self, native_id: u64, command: &ApplicationMessage) -> anyhow::Result<()> {
        let _dpi = DpiScope::new()?;
        let (_, generation) = command
            .command_target()
            .context("not an application command")?;
        let (window, _) = self.live(native_id, generation)?;
        self.release_input();
        match command {
            ApplicationMessage::Focus { .. } => window.focus(),
            ApplicationMessage::Resize { width, height, .. } => window.resize(*width, *height),
            ApplicationMessage::Close { .. } => window.close(),
            ApplicationMessage::Minimize { .. } => window.show(SW_MINIMIZE),
            ApplicationMessage::RequestKeyframe { .. } => Ok(()),
            _ => anyhow::bail!("invalid native lifecycle operation"),
        }
    }

    fn release_input(&mut self) {
        for input in self.input.values_mut() {
            input.release();
        }
    }

    fn stop(&mut self) {
        self.release_input();
        self.input.clear();
        // Close owned windows normally; never kill the process on disconnect,
        // including when an application has unsaved documents or refuses close.
        for (_, window) in self.windows.known.values() {
            let _ = window.close();
        }
        self.windows.instance.revoke();
        self.windows.known.clear();
        self.stopped = true;
    }
}

impl Drop for Application {
    fn drop(&mut self) {
        self.stop();
    }
}

struct DpiScope(DPI_AWARENESS_CONTEXT);
impl DpiScope {
    fn new() -> anyhow::Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        ensure!(
            !previous.0.is_null(),
            "cannot establish physical application window coordinates"
        );
        Ok(Self(previous))
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn partial_message_input_never_advertises_complete_application_support() {
        let capability = super::capability();
        assert!(!capability.is_supported());
        assert_eq!(
            capability.reason,
            Some(nebula_common::application::ApplicationUnavailableReason::IsolationUnavailable)
        );
    }
}
