//! Choosing the media backend for the machine this agent is running on.
//!
//! The backend is picked at compile time, not configured: a macOS binary has
//! no use for PipeWire, and a Linux binary cannot link VideoToolbox. What is
//! left at runtime is a single honest question — does this machine actually
//! allow capture and input — which each backend answers for itself.

use std::sync::Arc;

use crate::media::{Platform, TestPattern};

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

/// The backend for the platform this binary was built for.
///
/// A platform without a finished backend falls back to the test pattern
/// rather than refusing to start. That is deliberate: enrolment, tunnels and
/// policy are worth exercising on a platform whose capture code is not
/// written yet, and a machine that streams a visible placeholder is easier to
/// diagnose than one that never appears at all.
#[must_use]
pub fn native() -> Arc<dyn Platform> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::MacOs)
    }
    #[cfg(target_os = "linux")]
    {
        Arc::new(linux::Linux::default())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        tracing::warn!(
            os = std::env::consts::OS,
            "no capture backend for this platform yet; streaming a test pattern"
        );
        Arc::new(TestPattern::default())
    }
}

/// Keep the fallback referenced on every platform so it cannot rot.
#[allow(dead_code)]
fn _fallback() -> TestPattern {
    TestPattern::default()
}
