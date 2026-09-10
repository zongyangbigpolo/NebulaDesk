//! User-session Wayland capture, portal-authorized input and hardware VA-API.
//!
//! See docs/linux-media.md for native packages and compositor requirements.

mod application_policy;
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
    fn application_capability(&self) -> nebula_common::ApplicationCapability {
        nebula_common::ApplicationCapability {
            reason: Some(
                nebula_common::application::ApplicationUnavailableReason::IsolationUnavailable,
            ),
            ..nebula_common::ApplicationCapability::default()
        }
    }

    fn application(
        &self,
        _launch: &nebula_common::ApplicationLaunch,
    ) -> anyhow::Result<Box<dyn crate::application::ApplicationBackend>> {
        anyhow::bail!("{}", application_policy::APP_UNAVAILABLE)
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_portal_cannot_admit_published_application_targets() {
        let platform = Linux::default();
        assert!(!platform.application_capability().is_supported());
        let launch = nebula_common::ApplicationLaunch {
            launch_path: "/definitely-not-launched".into(),
            launch_args: vec![],
            working_dir: None,
        };
        let error = platform
            .application(&launch)
            .err()
            .expect("must refuse APP mode");
        assert!(error
            .to_string()
            .contains("not a trusted PID/window identity"));
        assert!(platform.input().is_err());
    }

    #[test]
    fn input_permission_is_scoped_without_opening_a_desktop() {
        let factory = Linux::default();
        assert!(factory.input().is_err());
        let viewer = factory.session_scope(false).unwrap().unwrap();
        let controller = factory.session_scope(true).unwrap().unwrap();
        assert!(viewer.input().is_err());
        let mut input = controller.input().unwrap();
        assert!(input
            .inject(&ndp_proto::InputEvent::mouse_move(
                0.5,
                0.5,
                ndp_proto::Modifiers::NONE
            ))
            .is_err());
        assert!(factory.input().is_err());
    }
}
