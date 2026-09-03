//! The agent's persistent identity.
//!
//! Three secrets live here and nowhere else: the machine credential the
//! manager knows it by, and the Noise static keypair that every client
//! ultimately encrypts to. Losing the file means re-enrolling; leaking it
//! means an attacker can impersonate the machine, which is why it is written
//! with an owner-only mode and never logged.

use std::path::{Path, PathBuf};

use ndp_crypto::StaticKeypair;
use serde::{Deserialize, Serialize};

/// What the agent remembers between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// The machine's id, as registered with the manager.
    pub machine_id: uuid::Uuid,
    /// The machine credential, `<uuid>.<secret>`.
    pub credential: String,
    /// The Noise static secret key, hex encoded.
    ///
    /// This one must survive reinstalls: it is published to the manager as a
    /// public key and handed to clients inside their tickets, so regenerating
    /// it silently would make every client refuse to connect.
    pub noise_secret: String,
    /// The manager this machine is enrolled with.
    pub manager_url: String,
}

impl Identity {
    /// The default location, honouring `NEBULA_AGENT_STATE`.
    #[must_use]
    pub fn default_path() -> PathBuf {
        if let Ok(path) = std::env::var("NEBULA_AGENT_STATE") {
            return PathBuf::from(path);
        }
        let base = dirs_state().unwrap_or_else(|| PathBuf::from("."));
        base.join("nebula").join("agent.json")
    }

    /// Read the identity, or `None` if this machine has not been enrolled.
    pub fn load(path: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the identity with owner-only permissions.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;

        // Written to a temporary file and renamed so a crash midway through
        // cannot leave a half-written identity that the agent would then fail
        // to parse on every subsequent start.
        let temp = path.with_extension("tmp");
        std::fs::write(&temp, &json)?;
        restrict(&temp)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// The Noise static keypair this machine is known by.
    pub fn keypair(&self) -> anyhow::Result<StaticKeypair> {
        let bytes = hex::decode(&self.noise_secret)
            .map_err(|_| anyhow::anyhow!("the stored Noise secret is not hex"))?;
        Ok(StaticKeypair::from_secret(&bytes)?)
    }
}

#[cfg(unix)]
fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> std::io::Result<()> {
    // Windows inherits the parent directory's ACL, which for a per-user
    // application data directory is already owner-only.
    Ok(())
}

fn dirs_state() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|h| h.join("Library").join("Application Support"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|h| h.join(".local").join("state"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("agent.json");
        let keys = StaticKeypair::generate();

        let identity = Identity {
            machine_id: uuid::Uuid::now_v7(),
            credential: "abc.def".into(),
            noise_secret: hex::encode(keys.secret_bytes()),
            manager_url: "http://manager.test".into(),
        };
        identity.save(&path).unwrap();

        let loaded = Identity::load(&path).unwrap().unwrap();
        assert_eq!(loaded.machine_id, identity.machine_id);
        assert_eq!(
            loaded.keypair().unwrap().public().as_bytes(),
            keys.public().as_bytes(),
            "the Noise identity must survive a restart or every client is locked out"
        );
    }

    #[test]
    fn a_missing_identity_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Identity::load(&dir.path().join("absent.json"))
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_identity_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.json");
        Identity {
            machine_id: uuid::Uuid::now_v7(),
            credential: "abc.def".into(),
            noise_secret: hex::encode(StaticKeypair::generate().secret_bytes()),
            manager_url: "http://manager.test".into(),
        }
        .save(&path)
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "the machine's secrets must be owner-only");
    }
}
