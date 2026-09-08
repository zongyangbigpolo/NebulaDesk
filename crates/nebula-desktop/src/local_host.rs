//! Desktop-owned identities and conservative, detached Agent supervision.
//!
//! A launch reservation deliberately survives an unclean desktop exit. Without
//! an Agent-side control protocol, a restarted desktop cannot prove that the
//! detached process exited and must not launch or terminate a replacement.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex as StdMutex},
    time::Duration,
};

use fs2::FileExt;
use ndp_crypto::StaticKeypair;
use nebula_agent::Identity;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    error::{DesktopError, Result},
    manager::Manager,
};

const UNOWNED: &str = "This identity is managed outside this desktop. Its Agent status is unknown.";
const DETACHED: &str = "An Agent may still be running from another desktop instance. Status and stop control are unavailable; no replacement will be started.";
const EXITED: &str = "The local Agent exited. Check the Agent installation and system permissions.";

#[derive(Clone, Debug, Serialize)]
pub struct LocalHost {
    pub enrolled: bool,
    pub machine_id: Option<String>,
    pub manager_url: Option<String>,
    pub name: Option<String>,
    pub running: bool,
    pub permissions: Permissions,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Permissions {
    pub screen: &'static str,
    pub input: &'static str,
    pub audio: &'static str,
}

#[derive(Serialize, Deserialize)]
struct StoredIdentity {
    #[serde(flatten)]
    identity: Identity,
    #[serde(default)]
    desktop: Option<DesktopMetadata>,
}

impl Drop for StoredIdentity {
    fn drop(&mut self) {
        self.identity.credential.zeroize();
        self.identity.noise_secret.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
struct DesktopMetadata {
    version: u32,
    name: String,
}

#[derive(Deserialize)]
struct Enrolled {
    machine_id: uuid::Uuid,
    credential: String,
}

impl Drop for Enrolled {
    fn drop(&mut self) {
        self.credential.zeroize();
    }
}

#[derive(Default)]
struct Runtime {
    running: bool,
    error: Option<&'static str>,
    stop: Option<mpsc::Sender<oneshot::Sender<Result<()>>>>,
}

pub struct LocalAgent {
    state_path: PathBuf,
    binary: PathBuf,
    operations: Mutex<()>,
    runtime: Arc<StdMutex<Runtime>>,
}

impl LocalAgent {
    pub fn new(state_path: PathBuf, binary: PathBuf) -> Self {
        Self {
            state_path,
            binary,
            operations: Mutex::new(()),
            runtime: Arc::new(StdMutex::new(Runtime::default())),
        }
    }

    pub async fn snapshot(&self) -> Result<LocalHost> {
        let _operation = self.operations.lock().await;
        self.snapshot_inner()
    }

    fn snapshot_inner(&self) -> Result<LocalHost> {
        let identity = read_identity(&self.state_path)?;
        let runtime = self.runtime.lock().map_err(|_| state_error())?;
        let managed = identity.as_ref().is_some_and(is_managed);
        let uncertain = path_exists(&sibling(&self.state_path, ".running"))?;
        let other_owner = if managed && !runtime.running {
            match acquire_lock(&self.state_path) {
                Ok(_) => false,
                Err(error) if error.code == "agent_busy" => true,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        let error = if identity.is_some() && !managed {
            Some(UNOWNED)
        } else if !runtime.running && (uncertain || other_owner) {
            Some(DETACHED)
        } else {
            runtime.error
        };
        Ok(LocalHost {
            enrolled: identity.is_some(),
            machine_id: identity.as_ref().map(|i| i.identity.machine_id.to_string()),
            manager_url: identity
                .as_ref()
                .and_then(|i| public_manager_url(&i.identity.manager_url)),
            name: identity
                .as_ref()
                .and_then(|i| i.desktop.as_ref())
                .map(|m| m.name.clone()),
            running: runtime.running,
            permissions: Permissions {
                screen: "unknown",
                input: "unknown",
                audio: "unknown",
            },
            error: error.map(str::to_owned),
        })
    }

    pub async fn enroll(
        &self,
        manager_url: String,
        token: String,
        name: String,
        allow_http: bool,
    ) -> Result<LocalHost> {
        let token = Zeroizing::new(token);
        let _operation = self.operations.lock().await;
        // Test existence, not successful parsing: even malformed identities and
        // dangling links are never an invitation to consume another token.
        reject_existing(&self.state_path)?;
        let name = name.trim();
        if name.is_empty() || name.len() > 200 || token.is_empty() || token.len() > 8192 {
            return Err(DesktopError::new(
                "invalid_enrollment",
                "Enter a device name and a valid enrollment token.",
            ));
        }
        let manager = Manager::new(&manager_url, allow_http)?;
        let _lock = acquire_lock(&self.state_path)?;
        reject_existing(&self.state_path)?;
        if path_exists(&sibling(&self.state_path, ".running"))? {
            return Err(DesktopError::new("agent_unowned", DETACHED));
        }
        // Identity::save renames over its destination and sets permissions only
        // after writing. Reserve the final path exclusively and write through
        // that same owner-only handle instead.
        let mut destination = private_new(&self.state_path)?;
        destination.sync_all().map_err(|_| state_error())?;
        sync_parent(&self.state_path)?;
        let keys = StaticKeypair::generate();
        let mut body = serde_json::json!({
            "token": token.as_str(),
            "name": name,
            "os": match std::env::consts::OS {
                "macos" => "MACOS",
                "windows" => "WINDOWS",
                _ => "LINUX",
            },
            "os_version": "",
            "arch": std::env::consts::ARCH,
            "agent_version": env!("CARGO_PKG_VERSION"),
            "noise_public_key": hex::encode(keys.public().as_bytes()),
        });
        let response = manager
            .request::<Enrolled>(Method::POST, "v1/machines/enroll", None, Some(&body))
            .await;
        if let Some(serde_json::Value::String(secret)) = body.get_mut("token") {
            secret.zeroize();
        }
        // On ambiguous network failure the reservation remains. Retrying must
        // not silently overwrite an identity or redeem a second one-time token.
        let mut enrolled = response.map_err(|_| {
            DesktopError::new(
                "enrollment_incomplete",
                "Enrollment did not complete. The local identity path remains reserved; resolve the enrollment with your administrator before trying again.",
            )
        })?;
        if enrolled.credential.is_empty() {
            return Err(DesktopError::new(
                "enrollment_incomplete",
                "The manager did not return a machine credential. The identity path remains reserved.",
            ));
        }
        let identity = StoredIdentity {
            identity: Identity {
                machine_id: enrolled.machine_id,
                credential: std::mem::take(&mut enrolled.credential),
                noise_secret: hex::encode(keys.secret_bytes()),
                manager_url: manager.url().to_owned(),
            },
            desktop: Some(DesktopMetadata {
                version: 1,
                name: name.to_owned(),
            }),
        };
        let json = Zeroizing::new(serde_json::to_vec(&identity).map_err(|_| state_error())?);
        destination.write_all(&json).map_err(|_| state_error())?;
        destination.sync_all().map_err(|_| state_error())?;
        drop(_lock);
        self.snapshot_inner()
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<LocalHost> {
        let _operation = self.operations.lock().await;
        if !enabled {
            let stop = self.runtime.lock().map_err(|_| state_error())?.stop.clone();
            if let Some(stop) = stop {
                let (reply, done) = oneshot::channel();
                stop.send(reply).map_err(|_| state_error())?;
                done.await.map_err(|_| state_error())??;
            } else if path_exists(&sibling(&self.state_path, ".running"))? {
                return Err(DesktopError::new("agent_unowned", DETACHED));
            } else if read_identity(&self.state_path)?
                .as_ref()
                .is_some_and(|i| !is_managed(i))
            {
                return Err(DesktopError::new("agent_unowned", UNOWNED));
            }
            return self.snapshot_inner();
        }
        if self.runtime.lock().map_err(|_| state_error())?.running {
            return self.snapshot_inner();
        }
        let identity = read_identity(&self.state_path)?.ok_or_else(|| {
            DesktopError::new(
                "not_enrolled",
                "Enroll this device before starting its Agent.",
            )
        })?;
        if !is_managed(&identity) {
            return Err(DesktopError::new("agent_unowned", UNOWNED));
        }
        identity.identity.keypair().map_err(|_| state_error())?;
        let lock = acquire_lock(&self.state_path)?;
        let marker = sibling(&self.state_path, ".running");
        if path_exists(&marker)? {
            return Err(DesktopError::new("agent_unowned", DETACHED));
        }
        // Resolve before detaching: a relative binary must never become a PATH
        // lookup, and the child's identity must not depend on its working dir.
        let binary = fs::canonicalize(&self.binary).map_err(|_| {
            DesktopError::new(
                "agent_missing",
                "The first-party Agent executable is unavailable.",
            )
        })?;
        let state = fs::canonicalize(&self.state_path).map_err(|_| state_error())?;
        let reservation = private_new(&marker)?;
        reservation.sync_all().map_err(|_| state_error())?;
        sync_parent(&marker)?;
        let mut command = Command::new(binary);
        command
            .arg("--state")
            .arg(state)
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("NEBULA_ENROLLMENT_TOKEN");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0000_0008 | 0x0000_0200);
        }
        let (stop, requests) = mpsc::channel();
        let (ready, started) = oneshot::channel();
        let runtime = Arc::clone(&self.runtime);
        let thread_marker = marker.clone();
        // The thread outlives this LocalAgent and owns both Child and lock.
        // Dropping the desktop service never kills the Agent or leaks zombies.
        let monitor = std::thread::Builder::new()
            .name("desktop-agent".into())
            .spawn(move || supervise(command, lock, thread_marker, runtime, stop, requests, ready));
        if monitor.is_err() {
            let _ = fs::remove_file(marker);
            return Err(state_error());
        }
        started.await.map_err(|_| state_error())??;
        self.snapshot_inner()
    }
}

fn supervise(
    mut command: Command,
    _lock: File,
    marker: PathBuf,
    runtime: Arc<StdMutex<Runtime>>,
    stop: mpsc::Sender<oneshot::Sender<Result<()>>>,
    requests: mpsc::Receiver<oneshot::Sender<Result<()>>>,
    ready: oneshot::Sender<Result<()>>,
) {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            let _ = fs::remove_file(marker);
            let _ = ready.send(Err(DesktopError::new(
                "agent_start_failed",
                "The local Agent could not be started.",
            )));
            return;
        }
    };
    if let Ok(mut state) = runtime.lock() {
        state.running = true;
        state.error = None;
        state.stop = Some(stop);
    }
    let _ = ready.send(Ok(()));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                finish(&runtime, &marker, Some(EXITED));
                return;
            }
            Err(_) => {
                // Keep the reservation and never signal a numeric PID. Child's
                // wait still owns reaping if a transient status query failed.
                if let Ok(mut state) = runtime.lock() {
                    state.running = false;
                    state.error = Some(DETACHED);
                    state.stop = None;
                }
                if child.wait().is_ok() {
                    finish(&runtime, &marker, Some(EXITED));
                }
                return;
            }
            Ok(None) => {}
        }
        match requests.recv_timeout(Duration::from_millis(100)) {
            Ok(reply) => {
                // Only this thread's Child handle can be terminated, never a
                // persisted PID or a process discovered outside the desktop.
                let result = match child.try_wait() {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) => child.kill().and_then(|_| child.wait().map(|_| ())),
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    finish(&runtime, &marker, None);
                    drop(_lock);
                    let _ = reply.send(Ok(()));
                    return;
                }
                let _ = reply.send(Err(DesktopError::new(
                    "agent_stop_failed",
                    "The local Agent could not be stopped.",
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn finish(runtime: &StdMutex<Runtime>, marker: &Path, error: Option<&'static str>) {
    let removed = fs::remove_file(marker).is_ok();
    if let Ok(mut state) = runtime.lock() {
        state.running = false;
        state.stop = None;
        state.error = if removed { error } else { Some(DETACHED) };
    }
}

fn is_managed(identity: &StoredIdentity) -> bool {
    identity.desktop.as_ref().is_some_and(|m| m.version == 1)
}

fn public_manager_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    Some(value.to_owned())
}

fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )
        .and_then(|parent| parent.sync_all())
        .map_err(|_| state_error())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(state_error()),
    }
}

fn reject_existing(path: &Path) -> Result<()> {
    if path_exists(path)? {
        return Err(DesktopError::new(
            "already_enrolled",
            "An identity or enrollment reservation already exists. It will not be overwritten.",
        ));
    }
    Ok(())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn private_new(path: &Path) -> Result<File> {
    private_options()
        .create_new(true)
        .open(path)
        .map_err(|_| state_error())
}

fn acquire_lock(path: &Path) -> Result<File> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent).map_err(|_| state_error())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(parent)
            .map_err(|_| state_error())?
            .permissions()
            .mode()
            & 0o022
            != 0
        {
            return Err(state_error());
        }
    }
    let lock_path = sibling(path, ".lock");
    if path_exists(&lock_path)?
        && !fs::symlink_metadata(&lock_path)
            .map_err(|_| state_error())?
            .is_file()
    {
        return Err(state_error());
    }
    // Never unlink lock files: all instances must lock the same inode.
    let file = private_options()
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| state_error())?;
    verify_file(&lock_path, &file)?;
    FileExt::try_lock_exclusive(&file).map_err(|_| {
        DesktopError::new(
            "agent_busy",
            "Another desktop instance owns this Agent or is enrolling it.",
        )
    })?;
    Ok(file)
}

fn verify_file(path: &Path, file: &File) -> Result<()> {
    let named = fs::symlink_metadata(path).map_err(|_| state_error())?;
    let opened = file.metadata().map_err(|_| state_error())?;
    if !named.is_file() || !opened.is_file() {
        return Err(state_error());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if named.dev() != opened.dev()
            || named.ino() != opened.ino()
            || opened.mode() & 0o077 != 0
            || opened.nlink() != 1
        {
            return Err(state_error());
        }
    }
    Ok(())
}

fn read_identity(path: &Path) -> Result<Option<StoredIdentity>> {
    if !path_exists(path)? {
        return Ok(None);
    }
    if !fs::symlink_metadata(path)
        .map_err(|_| state_error())?
        .is_file()
    {
        return Err(state_error());
    }
    let file = File::open(path).map_err(|_| state_error())?;
    verify_file(path, &file)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| state_error())?;
    if bytes.len() > 64 * 1024 {
        return Err(state_error());
    }
    let identity = serde_json::from_slice(&bytes).map_err(|_| state_error())?;
    Ok(Some(identity))
}

fn state_error() -> DesktopError {
    DesktopError::new(
        "agent_state",
        "The local Agent state could not be safely read or written. Check the dedicated identity path and its private permissions.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!("local-host-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn identity(&self) -> PathBuf {
            self.0.join("agent.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn save_identity(path: &Path, managed: bool) {
        let identity = StoredIdentity {
            identity: Identity {
                machine_id: uuid::Uuid::nil(),
                credential: "never-return-this-credential".into(),
                noise_secret: hex::encode(StaticKeypair::generate().secret_bytes()),
                manager_url: "https://manager.example".into(),
            },
            desktop: managed.then(|| DesktopMetadata {
                version: 1,
                name: "My device".into(),
            }),
        };
        let mut file = private_new(path).unwrap();
        serde_json::to_writer(&mut file, &identity).unwrap();
    }

    #[tokio::test]
    async fn missing_identity_is_not_enrolled_and_permissions_are_unknown() {
        let dir = TestDir::new();
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        let status = host.snapshot().await.unwrap();
        assert!(!status.enrolled && !status.running);
        assert_eq!(status.permissions.screen, "unknown");
        assert_eq!(status.permissions.input, "unknown");
        assert_eq!(status.permissions.audio, "unknown");
        assert!(status.machine_id.is_none());
        assert!(status.error.is_none());
        assert_eq!(
            host.set_enabled(true).await.unwrap_err().code,
            "not_enrolled"
        );
    }

    #[tokio::test]
    async fn existing_malformed_identity_is_never_overwritten_or_sent_to_manager() {
        let dir = TestDir::new();
        let path = dir.identity();
        private_new(&path).unwrap().write_all(b"reserved").unwrap();
        let host = LocalAgent::new(path.clone(), "must-not-run".into());
        let error = host
            .enroll("not a URL".into(), "secret".into(), "device".into(), false)
            .await
            .unwrap_err();
        assert_eq!(error.code, "already_enrolled");
        assert_eq!(fs::read(path).unwrap(), b"reserved");
    }

    #[test]
    fn lock_is_exclusive_and_reusable_without_unlinking() {
        let dir = TestDir::new();
        let first = acquire_lock(&dir.identity()).unwrap();
        assert_eq!(
            acquire_lock(&dir.identity()).unwrap_err().code,
            "agent_busy"
        );
        drop(first);
        assert!(acquire_lock(&dir.identity()).is_ok());
    }

    #[test]
    fn identity_urls_with_credentials_are_not_returned_to_the_ui() {
        assert!(public_manager_url("https://user:secret@manager.example").is_none());
        assert!(public_manager_url("https://manager.example?token=secret").is_none());
        assert!(public_manager_url("https://manager.example#secret").is_none());
        assert_eq!(
            public_manager_url("https://manager.example").as_deref(),
            Some("https://manager.example")
        );
    }

    #[tokio::test]
    async fn external_identity_is_read_only_and_secrets_are_not_serialized() {
        let dir = TestDir::new();
        save_identity(&dir.identity(), false);
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        let status = host.snapshot().await.unwrap();
        assert!(status.enrolled && !status.running);
        assert_eq!(status.error.as_deref(), Some(UNOWNED));
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("credential") && !json.contains("noise_secret"));
        for enabled in [true, false] {
            assert_eq!(
                host.set_enabled(enabled).await.unwrap_err().code,
                "agent_unowned"
            );
        }
    }

    #[tokio::test]
    async fn restart_reservation_never_claims_liveness_or_starts_a_duplicate() {
        let dir = TestDir::new();
        save_identity(&dir.identity(), true);
        private_new(&sibling(&dir.identity(), ".running")).unwrap();
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        let status = host.snapshot().await.unwrap();
        assert!(!status.running);
        assert_eq!(status.error.as_deref(), Some(DETACHED));
        assert_eq!(status.name.as_deref(), Some("My device"));
        for enabled in [true, false] {
            assert_eq!(
                host.set_enabled(enabled).await.unwrap_err().code,
                "agent_unowned"
            );
        }
    }

    #[tokio::test]
    async fn another_owner_lock_blocks_start_without_a_process_probe() {
        let dir = TestDir::new();
        save_identity(&dir.identity(), true);
        let _owner = acquire_lock(&dir.identity()).unwrap();
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        let status = host.snapshot().await.unwrap();
        assert!(!status.running);
        assert_eq!(status.error.as_deref(), Some(DETACHED));
        assert_eq!(host.set_enabled(true).await.unwrap_err().code, "agent_busy");
    }

    #[tokio::test]
    async fn unsafe_manager_url_does_not_reserve_an_identity() {
        let dir = TestDir::new();
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        assert_eq!(
            host.enroll(
                "http://public.example".into(),
                "secret".into(),
                "device".into(),
                false,
            )
            .await
            .unwrap_err()
            .code,
            "invalid_url"
        );
        assert!(!path_exists(&dir.identity()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_creation_is_private_and_rejects_symlinks() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};
        let dir = TestDir::new();
        let path = dir.identity();
        let file = private_new(&path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert!(private_new(&path).is_err());
        let link = dir.0.join("linked.json");
        symlink(&path, &link).unwrap();
        assert!(private_new(&link).is_err());
        assert!(read_identity(&link).is_err());
        assert_eq!(reject_existing(&link).unwrap_err().code, "already_enrolled");
    }
}
