//! Windows desktop media. All COM media objects live on their owning MTA worker.

mod audio;
pub mod bitstream;
mod capture;
pub mod decoder;
mod input;
mod mf;

use crate::clipboard::{ClipboardAccess, SystemClipboard};
use crate::media::{AudioSource, InputInjector, Platform, VideoSource};

#[derive(Debug, Default, Clone, Copy)]
pub struct Windows;

impl Platform for Windows {
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
