//! User-session Wayland capture, portal-authorized input and hardware VA-API.
//!
//! See docs/linux-media.md for native packages and compositor requirements.

mod audio;
mod capture;
pub mod h264;
mod input;
pub mod pipeline;
mod portal;

use std::sync::{Arc, Mutex};

use anyhow::Context;

use crate::clipboard::{ClipboardAccess, SystemClipboard};
use crate::media::{AudioSource, InputInjector, Platform, VideoSource};

/// Each scoped instance belongs to exactly one authenticated remote session.
#[derive(Default)]
pub struct Linux {
    allow_input: bool,
    state: Arc<Mutex<Option<portal::Portal>>>,
}

impl Platform for Linux {
    fn session_scope(&self, allow_input: bool) -> anyhow::Result<Option<Arc<dyn Platform>>> {
        Ok(Some(Arc::new(Self {
            allow_input,
            ..Self::default()
        })))
    }

    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
        desktop_session()?;
        Ok(Box::new(capture::LinuxVideo::new(
            self.allow_input,
            self.state.clone(),
        )))
    }

    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        anyhow::ensure!(
            self.allow_input,
            "this Linux session has no input permission"
        );
        Ok(Box::new(input::LinuxInput::new(self.state.clone())))
    }

    fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        desktop_session()?;
        Ok(Box::new(audio::LinuxAudio::default()))
    }

    fn clipboard(&self) -> anyhow::Result<Box<dyn ClipboardAccess>> {
        desktop_session()?;
        Ok(Box::new(SystemClipboard::open().context(
            "cannot open native clipboard; Wayland requires a data-control capable compositor",
        )?))
    }
}

fn desktop_session() -> anyhow::Result<()> {
    for name in [
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        anyhow::ensure!(
            std::env::var_os(name).is_some_and(|value| !value.is_empty()),
            "Linux media requires an active Wayland user session ({name} is missing); \
             run the agent as the desktop user, not a headless/root system service"
        );
    }
    Ok(())
}
