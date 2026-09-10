//! Windows desktop media. All COM media objects live on their owning MTA worker.

mod application;
mod application_input;
mod application_native;
mod application_policy;
mod audio;
mod bitstream;
mod capture;
pub mod decoder;
mod input;
mod mf;

use crate::clipboard::{ClipboardAccess, SystemClipboard};
use crate::media::{AudioSource, InputInjector, Platform, VideoSource};

/// Native Windows desktop capture, hardware media, input, and clipboard backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct Windows;

impl Platform for Windows {
    fn application_capability(&self) -> nebula_common::ApplicationCapability {
        application::capability()
    }

    fn application(
        &self,
        launch: &nebula_common::ApplicationLaunch,
    ) -> anyhow::Result<Box<dyn crate::application::ApplicationBackend>> {
        anyhow::ensure!(application::capability().is_supported(),
            "Windows APP capture/lifecycle foundation exists, but complete application-scoped \
             input is unavailable. HWND message input covers only ordinary controls/text; \
             Raw Input, modifiers and IME require a scoped provider. Global SendInput fallback is forbidden."
        );
        Ok(Box::new(application::Application::launch(launch)?))
    }

    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
        Ok(Box::new(capture::WindowsVideo::default()))
    }

    fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        Ok(Box::new(audio::WindowsAudio::default()))
    }

    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(input::WindowsInput::default()))
    }

    fn clipboard(&self) -> anyhow::Result<Box<dyn ClipboardAccess>> {
        Ok(Box::new(SystemClipboard::open()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_input_provider_refuses_application_launch_before_starting_a_process() {
        let platform = Windows;
        assert!(!platform.application_capability().is_supported());
        let launch = nebula_common::ApplicationLaunch {
            launch_path: r"C:\definitely-not-launched.exe".into(),
            launch_args: vec![],
            working_dir: None,
        };
        let error = platform
            .application(&launch)
            .err()
            .expect("must refuse partial APP mode");
        assert!(error
            .to_string()
            .contains("Global SendInput fallback is forbidden"));
    }
}
