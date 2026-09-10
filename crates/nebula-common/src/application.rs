//! Trusted application admission contracts. No client-supplied command is executable authority.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{ResourceId, SessionPolicy};

/// Version of the isolated, per-native-window protocol.
pub const APPLICATION_PROTOCOL_VERSION: u16 = 1;
/// Session-wide surface ID budget. Retired IDs must never be reused.
pub const MAX_APPLICATION_SURFACES: u8 = 32;

/// A publisher-configured executable and argv, passed separately without a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationLaunch {
    /// Trusted executable path. Must be absolute on the serving platform.
    pub launch_path: String,
    /// Arguments, never shell-concatenated.
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Optional trusted working directory.
    #[serde(default)]
    pub working_dir: Option<String>,
}

impl ApplicationLaunch {
    /// Content version bound into the signed session target.
    pub fn version(&self) -> String {
        hex::encode(Sha256::digest(
            serde_json::to_vec(self).expect("launch strings serialize"),
        ))
    }

    /// Bound resource size and reject NUL/relative executable paths on any platform.
    pub fn validate(&self) -> crate::Result<()> {
        let absolute = |s: &str| {
            s.starts_with('/')
                || s.starts_with("\\\\")
                || (s.as_bytes().get(1) == Some(&b':')
                    && s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                    && matches!(s.as_bytes().get(2), Some(b'\\' | b'/')))
        };
        let text = |s: &str| s.len() <= 4096 && !s.contains('\0');
        if !absolute(&self.launch_path)
            || !text(&self.launch_path)
            || self.launch_args.len() > 64
            || self.launch_args.iter().any(|s| !text(s))
            || self.launch_args.iter().map(String::len).sum::<usize>() > 16 * 1024
            || self
                .working_dir
                .as_deref()
                .is_some_and(|s| !absolute(s) || !text(s))
            || serde_json::to_vec(self).map_or(true, |encoded| encoded.len() > 16 * 1024)
        {
            return Err(crate::Error::Invalid(
                "invalid application launch configuration".into(),
            ));
        }
        Ok(())
    }
}

/// Signed immutable authority for the lifetime of one logical session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchTarget {
    /// Legacy peers and absent fields always mean desktop, never application.
    #[default]
    Desktop,
    /// A bound publisher resource snapshot; not a client command.
    Application {
        /// Must equal the enclosing ticket resource ID.
        resource_id: ResourceId,
        /// SHA-256 hex digest of the serialized launch configuration.
        version: String,
        /// Trusted launch configuration.
        launch: ApplicationLaunch,
    },
}

impl LaunchTarget {
    /// Whether this target requires explicit isolated-window negotiation.
    pub const fn is_application(&self) -> bool {
        matches!(self, Self::Application { .. })
    }

    /// Check resource binding and the policy ceiling before opening a session.
    pub fn validate(&self, resource: ResourceId, policy: SessionPolicy) -> crate::Result<()> {
        if let Self::Application {
            resource_id,
            version,
            launch,
        } = self
        {
            if *resource_id != resource
                || version.len() != 64
                || !version.bytes().all(|b| b.is_ascii_hexdigit())
                || *version != launch.version()
                || policy != application_policy(policy)
            {
                return Err(crate::Error::Denied(
                    "invalid application session authority".into(),
                ));
            }
            launch.validate()?;
        }
        Ok(())
    }
}

/// No global clipboard, system audio or filesystem transfer in application sessions.
pub const fn application_policy(policy: SessionPolicy) -> SessionPolicy {
    SessionPolicy {
        clipboard: false,
        audio: false,
        file_transfer: false,
        input: policy.input,
    }
}

/// Stable non-sensitive reasons, not native errors or launch paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationUnavailableReason {
    /// No isolated backend exists for this OS.
    UnsupportedPlatform,
    /// The backend is absent or not operational.
    BackendUnavailable,
    /// Required platform permissions are missing.
    PermissionDenied,
    /// The backend cannot guarantee application-only capture/input.
    IsolationUnavailable,
}

/// Runtime backend advertisement under machine `capabilities.application`.
/// An OS name alone is never evidence of support or hardware acceptance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplicationCapability {
    /// True only after probing an actual isolated backend.
    pub supported: bool,
    /// Supported application protocol version.
    pub protocol_version: u16,
    /// Maximum distinct surfaces in one logical session.
    pub max_surfaces: u8,
    /// True only for an application-scoped menu implementation. The desktop
    /// system menu bar must never be captured as a substitute.
    #[serde(default)]
    pub global_menu_supported: bool,
    /// A stable failure reason when unsupported.
    pub reason: Option<ApplicationUnavailableReason>,
}

impl Default for ApplicationCapability {
    fn default() -> Self {
        Self {
            supported: false,
            protocol_version: APPLICATION_PROTOCOL_VERSION,
            max_surfaces: 0,
            global_menu_supported: false,
            reason: Some(ApplicationUnavailableReason::BackendUnavailable),
        }
    }
}

impl ApplicationCapability {
    /// Reject partial, contradictory, unknown-version or excessive advertisements.
    pub fn is_supported(&self) -> bool {
        self.supported
            && self.protocol_version == APPLICATION_PROTOCOL_VERSION
            && (1..=MAX_APPLICATION_SURFACES).contains(&self.max_surfaces)
            && self.reason.is_none()
    }

    /// Read a runtime machine advertisement, failing closed on absent/malformed data.
    pub fn from_machine_capabilities(value: &serde_json::Value) -> Self {
        value
            .get("application")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_fails_closed() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"application":{"supported":true}}),
        ] {
            assert!(!ApplicationCapability::from_machine_capabilities(&value).is_supported());
        }
        let mut capability = ApplicationCapability {
            supported: true,
            protocol_version: 1,
            max_surfaces: 32,
            global_menu_supported: false,
            reason: None,
        };
        assert!(capability.is_supported());
        capability.reason = Some(ApplicationUnavailableReason::PermissionDenied);
        assert!(!capability.is_supported());
        capability.reason = None;
        capability.max_surfaces = 33;
        assert!(!capability.is_supported());
    }

    #[test]
    fn launch_bounds_and_resource_policy_binding() {
        let launch = ApplicationLaunch {
            launch_path: "/Applications/Example".into(),
            launch_args: vec!["literal; not a shell".into()],
            working_dir: None,
        };
        assert!(launch.validate().is_ok());
        let resource = ResourceId::new();
        let target = LaunchTarget::Application {
            resource_id: resource,
            version: launch.version(),
            launch,
        };
        assert!(target
            .validate(resource, application_policy(SessionPolicy::full()))
            .is_ok());
        assert!(target
            .validate(ResourceId::new(), SessionPolicy::view_only())
            .is_err());
        assert!(target.validate(resource, SessionPolicy::full()).is_err());
        for path in ["relative", "C:relative", "/bad\0path"] {
            assert!(ApplicationLaunch {
                launch_path: path.into(),
                launch_args: vec![],
                working_dir: None
            }
            .validate()
            .is_err());
        }
    }
}
