//! The macOS backend.
//!
//! ScreenCaptureKit for pixels, VideoToolbox for encoding, CoreGraphics for
//! input. All three are the paths the system itself uses, which is what makes
//! a session feel like sitting at the machine rather than watching it.

pub mod audio;
pub mod capture;
pub mod input;

use crate::clipboard::{ClipboardAccess, SystemClipboard};
use crate::media::{AudioSource, InputInjector, Platform, VideoSource};

/// Media backend for macOS.
#[derive(Debug, Default, Clone, Copy)]
pub struct MacOs;

impl Platform for MacOs {
    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
        Ok(Box::new(capture::MacVideo::new()))
    }

    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(input::MacInput::new()?))
    }

    fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
        Ok(Box::new(audio::MacAudio::new()))
    }

    fn clipboard(&self) -> anyhow::Result<Box<dyn ClipboardAccess>> {
        Ok(Box::new(SystemClipboard::open()?))
    }
}
