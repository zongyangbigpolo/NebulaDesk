//! Desktop-owned identities and authenticated, restartable Agent control.

#[path = "agent_daemon.rs"]
pub mod daemon;

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
const DETACHED: &str = "An Agent holds this identity but its authenticated control endpoint is unavailable. No replacement will be started.";

#[derive(Clone, Debug, Serialize)]
pub struct LocalHost {
    pub enrolled: bool,
    pub machine_id: Option<String>,
    pub manager_url: Option<String>,
    pub name: Option<String>,
    pub running: bool,
    /// Process liveness does not establish Manager/gateway connectivity.
    pub connection_state: &'static str,
    pub permissions: Permissions,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Permissions {
    pub screen: &'static str,
    pub input: &'static str,
    pub audio: &'static str,
}

impl Permissions {
    fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            use nebula_agent::platform::macos::{capture, input};
            Self {
                screen: if capture::screen_capture_allowed() {
                    "granted"
                } else {
                    "denied"
                },
                input: if input::trusted() {
                    "granted"
                } else {
                    "denied"
                },
                audio: "unknown",
            }
        }
        #[cfg(not(target_os = "macos"))]
        Self {
            screen: "unknown",
            input: "unknown",
            audio: "unknown",
        }
    }
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

pub struct LocalAgent {
    state_path: PathBuf,
    binary: PathBuf,
    operations: Mutex<()>,
}

impl LocalAgent {
    pub fn new(state_path: PathBuf, binary: PathBuf) -> Self {
        Self {
            state_path,
            binary,
            operations: Mutex::new(()),
        }
    }

    pub async fn snapshot(&self) -> Result<LocalHost> {
        let _operation = self.operations.lock().await;
        self.snapshot_inner().await
    }

    async fn snapshot_inner(&self) -> Result<LocalHost> {
        let identity = read_identity(&self.state_path)?;
        let managed = identity.as_ref().is_some_and(is_managed);
        let mut running = false;
        let mut error = None;
        if identity.is_some() && !managed {
            error = Some(UNOWNED);
        } else if path_exists(&sibling(&self.state_path, ".running"))? {
            // Older direct-Agent launches have no authenticated control
            // protocol. Never adopt or replace a possibly live legacy Agent.
            error = Some(DETACHED);
        } else if let Some(identity) = identity.as_ref() {
            if service_busy(&self.state_path)? {
                match daemon::request(
                    &self.state_path,
                    identity.identity.machine_id,
                    daemon::ControlCommand::Status,
                )
                .await
                {
                    Ok(status) => running = status.running,
                    Err(_) => error = Some(DETACHED),
                }
            }
        }
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
            running,
            connection_state: "unknown",
            permissions: Permissions::current(),
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
        let _service = acquire_service_lock(&self.state_path)?;
        // The final identity is installed with a no-clobber hard link. Failed
        // enrollment never leaves an empty permanent identity behind.
        let (staging, mut destination) = StagingFile::new(&self.state_path)?;
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
        let mut enrolled = response.map_err(|_| {
            DesktopError::new(
                "enrollment_incomplete",
                "Enrollment did not complete. The token may have been consumed; check with your administrator before retrying.",
            )
        })?;
        if enrolled.credential.is_empty() {
            return Err(DesktopError::new(
                "enrollment_incomplete",
                "The manager did not return a machine credential.",
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
        fs::hard_link(&staging.0, &self.state_path).map_err(|_| state_error())?;
        sync_parent(&self.state_path)?;
        drop(destination);
        drop(staging);
        sync_parent(&self.state_path)?;
        drop(_service);
        drop(_lock);
        self.snapshot_inner().await
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<LocalHost> {
        let _operation = self.operations.lock().await;
        let _lock = acquire_lock(&self.state_path)?;
        let Some(identity) = read_identity(&self.state_path)? else {
            if !enabled {
                return self.snapshot_inner().await;
            }
            return Err(DesktopError::new(
                "not_enrolled",
                "Enroll this device before starting its Agent.",
            ));
        };
        if !is_managed(&identity) {
            return Err(DesktopError::new("agent_unowned", UNOWNED));
        }
        identity.identity.keypair().map_err(|_| state_error())?;
        if path_exists(&sibling(&self.state_path, ".running"))? {
            return Err(DesktopError::new("agent_unowned", DETACHED));
        }
        if service_busy(&self.state_path)? {
            let command = if enabled {
                daemon::ControlCommand::Status
            } else {
                daemon::ControlCommand::Stop
            };
            daemon::request(&self.state_path, identity.identity.machine_id, command).await?;
            if !enabled {
                wait_stopped(&self.state_path).await?;
            }
            return self.snapshot_inner().await;
        }
        // Cleanup is permitted only while holding the service lock: an
        // unresponsive, live daemon must never lose its control capability.
        {
            let _service = acquire_service_lock(&self.state_path)?;
            daemon::remove_control(&self.state_path)?;
        }
        if !enabled {
            return self.snapshot_inner().await;
        }
        let binary = fs::canonicalize(&self.binary).map_err(|_| {
            DesktopError::new(
                "agent_missing",
                "The first-party Agent helper executable is unavailable.",
            )
        })?;
        let state = fs::canonicalize(&self.state_path).map_err(|_| state_error())?;
        let mut command = Command::new(binary);
        command
            .arg("--state")
            .arg(state)
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
        let (ready, started) = oneshot::channel();
        let (exited, mut exit) = oneshot::channel();
        // A dedicated reaper outlives LocalAgent, without a kill-on-drop handle.
        // When the desktop exits, the helper is reparented by the OS.
        std::thread::Builder::new()
            .name("desktop-agent".into())
            .spawn(move || match command.spawn() {
                Ok(mut child) => {
                    let _ = ready.send(Ok(()));
                    let _ = child.wait();
                    let _ = exited.send(());
                }
                Err(_) => {
                    let _ = ready.send(Err(start_error()));
                }
            })
            .map_err(|_| start_error())?;
        started.await.map_err(|_| state_error())??;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if !matches!(exit.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                return Err(start_error());
            }
            if service_busy(&self.state_path)? {
                if let Ok(status) = daemon::request(
                    &self.state_path,
                    identity.identity.machine_id,
                    daemon::ControlCommand::Status,
                )
                .await
                {
                    if status.running {
                        return self.snapshot_inner().await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(DesktopError::new("agent_start_timeout", DETACHED))
    }
}

async fn wait_stopped(path: &Path) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if !service_busy(path)? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(DesktopError::new(
        "agent_stop_timeout",
        "The Agent has not finished stopping.",
    ))
}

fn start_error() -> DesktopError {
    DesktopError::new(
        "agent_start_failed",
        "The local Agent helper could not be started.",
    )
}

struct StagingFile(PathBuf);

impl StagingFile {
    fn new(path: &Path) -> Result<(Self, File)> {
        let path = sibling(path, &format!(".pending-{}", uuid::Uuid::new_v4()));
        let file = private_new(&path)?;
        Ok((Self(path), file))
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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
    #[cfg(windows)]
    return windows_private::create_file(path);
    #[cfg(not(windows))]
    {
        let file = private_options()
            .create_new(true)
            .open(path)
            .map_err(|_| state_error())?;
        verify_file(path, &file)?;
        Ok(file)
    }
}

fn acquire_lock(path: &Path) -> Result<File> {
    acquire_named_lock(path, ".lock")
}

fn acquire_service_lock(path: &Path) -> Result<File> {
    acquire_named_lock(path, ".service.lock")
}

fn service_busy(path: &Path) -> Result<bool> {
    match acquire_service_lock(path) {
        Ok(_) => Ok(false),
        Err(error) if error.code == "agent_busy" => Ok(true),
        Err(error) => Err(error),
    }
}

fn acquire_named_lock(path: &Path, suffix: &str) -> Result<File> {
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
    #[cfg(not(windows))]
    builder.create(parent).map_err(|_| state_error())?;
    #[cfg(windows)]
    windows_private::ensure_directory(parent)?;
    #[cfg(target_os = "macos")]
    verify_private_acl(&File::open(parent).map_err(|_| state_error())?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let directory = fs::metadata(parent).map_err(|_| state_error())?;
        if directory.mode() & 0o022 != 0 || directory.uid() != effective_uid() {
            return Err(state_error());
        }
    }
    let lock_path = sibling(path, suffix);
    if path_exists(&lock_path)?
        && !fs::symlink_metadata(&lock_path)
            .map_err(|_| state_error())?
            .is_file()
    {
        return Err(state_error());
    }
    // Never unlink lock files: all instances must lock the same inode.
    let file = match private_new(&lock_path) {
        Ok(file) => file,
        Err(_) if path_exists(&lock_path)? => private_options()
            .open(&lock_path)
            .map_err(|_| state_error())?,
        Err(error) => return Err(error),
    };
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
            || opened.uid() != effective_uid()
        {
            return Err(state_error());
        }
    }
    #[cfg(windows)]
    windows_private::verify(path, file)?;
    #[cfg(target_os = "macos")]
    verify_private_acl(file)?;
    Ok(())
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // Supported Unix targets use an unsigned 32-bit uid_t.
    unsafe { geteuid() }
}

#[cfg(target_os = "macos")]
fn verify_private_acl(file: &File) -> Result<()> {
    use std::{ffi::c_void, os::fd::AsRawFd, ptr::null_mut};
    unsafe extern "C" {
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
        fn acl_valid(acl: *mut c_void) -> i32;
        fn acl_get_entry(acl: *mut c_void, id: i32, entry: *mut *mut c_void) -> i32;
        fn acl_free(acl: *mut c_void) -> i32;
        fn __error() -> *mut i32;
    }
    // Darwin extended ACLs can grant access independently of mode 0600.
    // Conservatively reject all extended entries, including inherited ones,
    // rather than claiming confidentiality based only on POSIX mode bits.
    unsafe {
        let acl = acl_get_fd_np(file.as_raw_fd(), 0x100);
        if acl.is_null() {
            // On an already-verified open descriptor Darwin reports ENOENT
            // when the object has no extended ACL.
            return if *__error() == 2 {
                Ok(())
            } else {
                Err(state_error())
            };
        }
        let valid = acl_valid(acl) == 0;
        let mut entry = null_mut();
        let empty = acl_get_entry(acl, 0, &mut entry) == -1 && *__error() == 22;
        acl_free(acl);
        if !valid || !empty {
            return Err(state_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
mod windows_private {
    use super::*;
    use std::{
        ffi::c_void,
        os::windows::{
            ffi::OsStrExt,
            fs::OpenOptionsExt,
            io::{AsRawHandle, FromRawHandle},
        },
        ptr::{null_mut, read_unaligned},
    };

    type Handle = *mut c_void;
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    #[repr(C)]
    struct Acl {
        revision: u8,
        reserved: u8,
        size: u16,
        count: u16,
        reserved2: u16,
    }
    #[repr(C)]
    struct AceHeader {
        kind: u8,
        flags: u8,
        size: u16,
    }
    #[derive(Default)]
    #[repr(C)]
    struct FileInformation {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
        fn GetTokenInformation(
            token: Handle,
            class: i32,
            buffer: *mut c_void,
            size: u32,
            needed: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, text: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
        fn GetSecurityInfo(
            handle: Handle,
            kind: i32,
            information: u32,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut Acl,
            sacl: *mut *mut Acl,
            descriptor: *mut *mut c_void,
        ) -> u32;
        fn GetAce(acl: *mut Acl, index: u32, ace: *mut *mut c_void) -> i32;
        fn EqualSid(left: *mut c_void, right: *mut c_void) -> i32;
        fn IsValidSid(sid: *mut c_void) -> i32;
        fn GetLengthSid(sid: *mut c_void) -> u32;
        fn IsWellKnownSid(sid: *mut c_void, kind: i32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn CreateFileW(
            path: *const u16,
            access: u32,
            sharing: u32,
            security: *const SecurityAttributes,
            disposition: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn CreateDirectoryW(path: *const u16, security: *const SecurityAttributes) -> i32;
        fn GetFileInformationByHandle(handle: Handle, information: *mut FileInformation) -> i32;
    }

    struct LocalMemory(*mut c_void);
    impl Drop for LocalMemory {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    struct Token(Handle);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn current_user() -> Result<Vec<usize>> {
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), 8, &mut token) == 0 {
                return Err(state_error());
            }
            let token = Token(token);
            let mut needed = 0;
            GetTokenInformation(token.0, 1, null_mut(), 0, &mut needed);
            if !(16..=65536).contains(&needed) {
                return Err(state_error());
            }
            let mut bytes = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetTokenInformation(token.0, 1, bytes.as_mut_ptr().cast(), needed, &mut needed) == 0
            {
                return Err(state_error());
            }
            Ok(bytes)
        }
    }

    fn user_sid(user: &[usize]) -> *mut c_void {
        // TOKEN_USER begins with a SID_AND_ATTRIBUTES containing this pointer;
        // the GetTokenInformation buffer owns the pointed-to SID.
        unsafe { read_unaligned(user.as_ptr().cast::<*mut c_void>()) }
    }

    fn descriptor() -> Result<LocalMemory> {
        let user = current_user()?;
        unsafe {
            let mut text = null_mut();
            if ConvertSidToStringSidW(user_sid(&user), &mut text) == 0 {
                return Err(state_error());
            }
            let _text = LocalMemory(text.cast());
            let mut length = 0;
            while length < 184 && *text.add(length) != 0 {
                length += 1;
            }
            if length == 184 {
                return Err(state_error());
            }
            let sid = String::from_utf16(std::slice::from_raw_parts(text, length))
                .map_err(|_| state_error())?;
            // Protected DACL: only this user and LocalSystem. No inherited
            // Users/Everyone ACE can expose credentials or control capability.
            let sddl: Vec<u16> = format!("O:{sid}D:P(A;;FA;;;SY)(A;;FA;;;{sid})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(state_error());
            }
            Ok(LocalMemory(descriptor))
        }
    }

    fn wide(path: &Path) -> Result<Vec<u16>> {
        let mut path: Vec<u16> = path.as_os_str().encode_wide().collect();
        if path.contains(&0) {
            return Err(state_error());
        }
        path.push(0);
        Ok(path)
    }

    pub(super) fn create_file(path: &Path) -> Result<File> {
        let descriptor = descriptor()?;
        let security = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: descriptor.0,
            inherit: 0,
        };
        let path = wide(path)?;
        unsafe {
            let handle = CreateFileW(
                path.as_ptr(),
                0xc0000000,
                7,
                &security,
                1,
                0x00200080,
                null_mut(),
            );
            if handle as isize == -1 {
                return Err(state_error());
            }
            Ok(File::from_raw_handle(handle))
        }
    }

    pub(super) fn ensure_directory(path: &Path) -> Result<()> {
        if !path_exists(path)? {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent).map_err(|_| state_error())?;
            }
            let descriptor = descriptor()?;
            let security = SecurityAttributes {
                length: std::mem::size_of::<SecurityAttributes>() as u32,
                descriptor: descriptor.0,
                inherit: 0,
            };
            let encoded = wide(path)?;
            if unsafe { CreateDirectoryW(encoded.as_ptr(), &security) } == 0 && !path_exists(path)?
            {
                return Err(state_error());
            }
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(0x02200000)
            .open(path)
            .map_err(|_| state_error())?;
        let information = information(&directory)?;
        if information.attributes & 0x400 != 0 || information.attributes & 0x10 == 0 {
            return Err(state_error());
        }
        verify_security(&directory)
    }

    fn information(file: &File) -> Result<FileInformation> {
        let mut information = FileInformation::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(state_error());
        }
        Ok(information)
    }

    pub(super) fn verify(path: &Path, file: &File) -> Result<()> {
        let named = OpenOptions::new()
            .read(true)
            .custom_flags(0x00200000)
            .open(path)
            .map_err(|_| state_error())?;
        let named = information(&named)?;
        let opened = information(file)?;
        if named.attributes & 0x400 != 0
            || opened.attributes & 0x400 != 0
            || opened.links != 1
            || named.volume != opened.volume
            || named.index_high != opened.index_high
            || named.index_low != opened.index_low
        {
            return Err(state_error());
        }
        verify_security(file)
    }

    fn verify_security(file: &File) -> Result<()> {
        let user = current_user()?;
        unsafe {
            let mut owner = null_mut();
            let mut acl = null_mut();
            let mut descriptor = null_mut();
            if GetSecurityInfo(
                file.as_raw_handle(),
                1,
                5,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            ) != 0
            {
                return Err(state_error());
            }
            let _descriptor = LocalMemory(descriptor);
            if owner.is_null()
                || EqualSid(owner, user_sid(&user)) == 0
                || acl.is_null()
                || (*acl).count > 64
            {
                return Err(state_error());
            }
            for index in 0..(*acl).count {
                let mut ace = null_mut();
                if GetAce(acl, index.into(), &mut ace) == 0 {
                    return Err(state_error());
                }
                let header = &*ace.cast::<AceHeader>();
                if header.flags & 8 != 0 {
                    continue;
                }
                if header.kind != 0 || header.size < 16 {
                    return Err(state_error());
                }
                let sid = ace.cast::<u8>().add(8).cast::<c_void>();
                if IsValidSid(sid) == 0
                    || GetLengthSid(sid) + 8 > u32::from(header.size)
                    || (EqualSid(sid, user_sid(&user)) == 0
                        && IsWellKnownSid(sid, 22) == 0
                        && IsWellKnownSid(sid, 26) == 0)
                {
                    return Err(state_error());
                }
            }
        }
        Ok(())
    }
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

    pub(super) struct TestDir(PathBuf);

    impl TestDir {
        pub(super) fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!("local-host-test-{}", uuid::Uuid::new_v4()));
            #[cfg(not(windows))]
            fs::create_dir_all(&path).unwrap();
            #[cfg(windows)]
            windows_private::ensure_directory(&path).unwrap();
            Self(path)
        }

        pub(super) fn identity(&self) -> PathBuf {
            self.0.join("agent.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    pub(super) fn save_identity(path: &Path, managed: bool) {
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
        assert_eq!(status.connection_state, "unknown");
        #[cfg(target_os = "macos")]
        {
            assert!(matches!(status.permissions.screen, "granted" | "denied"));
            assert!(matches!(status.permissions.input, "granted" | "denied"));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(status.permissions.screen, "unknown");
            assert_eq!(status.permissions.input, "unknown");
        }
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

    #[tokio::test]
    async fn stale_control_is_reclaimed_only_when_service_is_absent() {
        let dir = TestDir::new();
        save_identity(&dir.identity(), true);
        let control = sibling(&dir.identity(), ".control.json");
        private_new(&control).unwrap().write_all(b"stale").unwrap();
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        {
            let _service = acquire_service_lock(&dir.identity()).unwrap();
            assert!(host.set_enabled(false).await.is_err());
            assert!(path_exists(&control).unwrap());
        }
        assert!(!host.set_enabled(false).await.unwrap().running);
        assert!(!path_exists(&control).unwrap());
    }

    #[test]
    fn staging_cleanup_never_reserves_or_clobbers_final_identity() {
        let dir = TestDir::new();
        let (staging, mut file) = StagingFile::new(&dir.identity()).unwrap();
        let staging_path = staging.0.clone();
        file.write_all(b"candidate").unwrap();
        private_new(&dir.identity())
            .unwrap()
            .write_all(b"existing")
            .unwrap();
        assert!(fs::hard_link(&staging.0, dir.identity()).is_err());
        drop(file);
        drop(staging);
        assert!(!path_exists(&staging_path).unwrap());
        assert_eq!(fs::read(dir.identity()).unwrap(), b"existing");
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
        let _owner = acquire_service_lock(&dir.identity()).unwrap();
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        let status = host.snapshot().await.unwrap();
        assert!(!status.running);
        assert_eq!(status.error.as_deref(), Some(DETACHED));
        assert_eq!(
            host.set_enabled(true).await.unwrap_err().code,
            "agent_control"
        );
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

    #[cfg(unix)]
    #[test]
    fn broad_file_permissions_and_hard_links_are_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TestDir::new();
        save_identity(&dir.identity(), true);
        fs::set_permissions(dir.identity(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_identity(&dir.identity()).is_err());
        fs::set_permissions(dir.identity(), fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(dir.identity(), dir.0.join("alias.json")).unwrap();
        assert!(read_identity(&dir.identity()).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_private_creation_proves_owner_and_rejects_hard_links() {
        let dir = TestDir::new();
        let file = private_new(&dir.identity()).unwrap();
        verify_file(&dir.identity(), &file).unwrap();
        fs::hard_link(dir.identity(), dir.0.join("alias.json")).unwrap();
        assert!(verify_file(&dir.identity(), &file).is_err());
    }
}
