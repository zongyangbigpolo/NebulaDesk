use serde::Serialize;

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct DesktopError {
    pub code: &'static str,
    pub message: &'static str,
}

impl DesktopError {
    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    pub fn network(_: reqwest::Error) -> Self {
        Self::new("network", "The manager could not be reached securely.")
    }

    pub fn protocol() -> Self {
        Self::new(
            "protocol",
            "The service returned an invalid or oversized response.",
        )
    }

    pub fn cancelled() -> Self {
        Self::new("cancelled", "This operation was cancelled.")
    }
}

pub type Result<T> = std::result::Result<T, DesktopError>;
