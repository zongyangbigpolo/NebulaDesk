//! The macOS backend.
//!
//! Capture and encode are not written yet, so this platform provides real
//! input injection over a synthetic picture. That combination is not a
//! finished product but it is an honest one: input can be verified against a
//! real desktop today, and the video source is replaced without anything
//! above this module changing.

pub mod input;

use crate::media::{InputInjector, Platform, SyntheticVideo, VideoSource};

/// Media backend for macOS.
#[derive(Debug, Default, Clone, Copy)]
pub struct MacOs;

impl Platform for MacOs {
    fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
        Ok(Box::new(SyntheticVideo::default()))
    }

    fn input(&self) -> anyhow::Result<Box<dyn InputInjector>> {
        Ok(Box::new(input::MacInput::new()?))
    }
}
