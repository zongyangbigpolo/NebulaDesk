//! Process-lifetime Agent service with private, authenticated loopback control.
//!
//! This module is included by local_host; `run` is only for the helper binary,
//! never the desktop process. Its service lock deliberately lives until OS exit.

use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use nebula_agent::{native, Agent};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
    time::timeout,
};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::{
    acquire_service_lock, is_managed, path_exists, read_identity, sibling, state_error,
    sync_parent, verify_file, StagingFile, UNOWNED,
};
use crate::error::{DesktopError, Result};

const MAX_LINE: usize = 4096;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CONNECTIONS: usize = 8;

/// Synchronous process entrypoint so the helper package needs only this crate.
pub fn run_process(state_path: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| state_error())?;
    runtime.block_on(run(state_path))
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ControlCommand {
    Status,
    Stop,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    capability: String,
    command: ControlCommand,
}

impl Drop for ControlRequest {
    fn drop(&mut self) {
        self.capability.zeroize();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlManifest {
    port: u16,
    capability: String,
    machine_id: Uuid,
}

impl Drop for ControlManifest {
    fn drop(&mut self) {
        self.capability.zeroize();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControlStatus {
    pub machine_id: Uuid,
    pub running: bool,
}

/// Run the first-party helper until authenticated stop. Call once per process.
/// The retained lock is released by the OS only after the Tokio runtime and all
/// Agent session tasks have exited, not merely when this future returns.
pub async fn run(state_path: PathBuf) -> Result<()> {
    run_service(state_path, |identity| {
        let agent = Agent::new(identity, native()).map_err(|_| state_error())?;
        Ok(async move {
            agent.run().await;
        })
    })
    .await
}

async fn run_service<F, A>(state_path: PathBuf, make_agent: F) -> Result<()>
where
    F: FnOnce(nebula_agent::Identity) -> Result<A>,
    A: std::future::Future<Output = ()>,
{
    let lock = acquire_service_lock(&state_path)?;
    let identity = read_identity(&state_path)?.ok_or_else(state_error)?;
    if !is_managed(&identity) {
        return Err(DesktopError::new("agent_unowned", UNOWNED));
    }
    if path_exists(&sibling(&state_path, ".running"))? {
        return Err(control_error());
    }
    remove_control(&state_path)?;
    let activity = make_agent(identity.identity.clone())?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|_| control_error())?;
    let manifest = Arc::new(ControlManifest {
        port: listener.local_addr().map_err(|_| control_error())?.port(),
        capability: Uuid::new_v4().to_string(),
        machine_id: identity.identity.machine_id,
    });
    write_manifest(&state_path, &manifest)?;
    // A helper process owns exactly one service. Intentionally retaining this
    // descriptor also covers runtime shutdown after `run` returns. Never use
    // this entrypoint in the desktop process or an in-process test.
    std::mem::forget(lock);
    let result = tokio::select! {
        _ = activity => Err(control_error()),
        result = serve(listener, manifest) => result,
    };
    let cleanup = remove_control(&state_path);
    result.and(cleanup)
}

async fn serve(listener: TcpListener, manifest: Arc<ControlManifest>) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            completed = connections.join_next(), if !connections.is_empty() => {
                if matches!(completed, Some(Ok(true))) {
                    connections.abort_all();
                    while connections.join_next().await.is_some() {}
                    return Ok(());
                }
            }
            incoming = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                let (stream, peer) = incoming.map_err(|_| control_error())?;
                if !peer.ip().is_loopback() {
                    continue;
                }
                let manifest = Arc::clone(&manifest);
                connections.spawn(async move {
                    timeout(CONTROL_TIMEOUT, respond(stream, &manifest))
                        .await
                        .ok()
                        .and_then(std::result::Result::ok)
                        .unwrap_or(false)
                });
            }
        }
    }
}

async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    manifest: &ControlManifest,
) -> Result<bool> {
    let bytes = read_line(&mut stream).await?;
    let request = authenticate(&bytes, &manifest.capability)?;
    let stopping = request.command == ControlCommand::Stop;
    let response = serde_json::to_vec(&ControlStatus {
        machine_id: manifest.machine_id,
        running: !stopping,
    })
    .map_err(|_| control_error())?;
    stream
        .write_all(&response)
        .await
        .map_err(|_| control_error())?;
    stream.write_all(b"\n").await.map_err(|_| control_error())?;
    stream.shutdown().await.map_err(|_| control_error())?;
    Ok(stopping)
}

fn authenticate(bytes: &[u8], capability: &str) -> Result<ControlRequest> {
    let request: ControlRequest = serde_json::from_slice(bytes).map_err(|_| control_error())?;
    if !capability_matches(&request.capability, capability) {
        return Err(control_error());
    }
    Ok(request)
}

fn capability_matches(candidate: &str, expected: &str) -> bool {
    if candidate.len() != 36 || expected.len() != 36 {
        return false;
    }
    candidate
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

pub(super) async fn request(
    state: &Path,
    machine_id: Uuid,
    command: ControlCommand,
) -> Result<ControlStatus> {
    let manifest = read_manifest(state)?;
    if manifest.machine_id != machine_id {
        return Err(control_error());
    }
    timeout(CONTROL_TIMEOUT, async {
        let mut stream = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, manifest.port))
            .await
            .map_err(|_| control_error())?;
        let request = ControlRequest {
            capability: manifest.capability.clone(),
            command,
        };
        let mut bytes = Zeroizing::new(serde_json::to_vec(&request).map_err(|_| control_error())?);
        bytes.push(b'\n');
        stream
            .write_all(&bytes)
            .await
            .map_err(|_| control_error())?;
        let bytes = read_line(&mut stream).await?;
        let response: ControlStatus =
            serde_json::from_slice(&bytes).map_err(|_| control_error())?;
        if response.machine_id != machine_id {
            return Err(control_error());
        }
        Ok(response)
    })
    .await
    .map_err(|_| control_error())?
}

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(256));
    // Include the newline in the maximum so neither peer can allocate a
    // growing buffer or rely on EOF as an alternative framing protocol.
    for _ in 0..MAX_LINE {
        let byte = reader.read_u8().await.map_err(|_| control_error())?;
        if byte == b'\n' {
            return Ok(bytes);
        }
        bytes.push(byte);
    }
    Err(control_error())
}

fn manifest_path(state: &Path) -> PathBuf {
    sibling(state, ".control.json")
}

fn read_manifest(state: &Path) -> Result<ControlManifest> {
    let path = manifest_path(state);
    if !fs::symlink_metadata(&path)
        .map_err(|_| control_error())?
        .is_file()
    {
        return Err(control_error());
    }
    let file = File::open(&path).map_err(|_| control_error())?;
    verify_file(&path, &file)?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_LINE as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| control_error())?;
    if bytes.len() > MAX_LINE {
        return Err(control_error());
    }
    let manifest: ControlManifest = serde_json::from_slice(&bytes).map_err(|_| control_error())?;
    let capability = Uuid::parse_str(&manifest.capability).map_err(|_| control_error())?;
    if manifest.port == 0
        || capability.get_version_num() != 4
        || capability.to_string() != manifest.capability
    {
        return Err(control_error());
    }
    Ok(manifest)
}

fn write_manifest(state: &Path, manifest: &ControlManifest) -> Result<()> {
    let path = manifest_path(state);
    let (staging, mut file) = StagingFile::new(&path)?;
    let bytes = Zeroizing::new(serde_json::to_vec(manifest).map_err(|_| control_error())?);
    file.write_all(&bytes).map_err(|_| control_error())?;
    file.sync_all().map_err(|_| control_error())?;
    fs::hard_link(&staging.0, &path).map_err(|_| control_error())?;
    drop(file);
    drop(staging);
    sync_parent(&path)
}

// Callers must own the service lock; no live daemon's endpoint is ever erased.
pub(super) fn remove_control(state: &Path) -> Result<()> {
    match fs::remove_file(manifest_path(state)) {
        Ok(()) => sync_parent(state),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(control_error()),
    }
}

fn control_error() -> DesktopError {
    DesktopError::new(
        "agent_control",
        "The local Agent could not be reached through its authenticated control endpoint.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_host::{
        private_new,
        tests::{save_identity, TestDir},
        LocalAgent,
    };
    use std::process::{Child, Command, ExitStatus, Stdio};

    struct Fixture(Child);

    impl Fixture {
        fn start(state: &Path) -> Self {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "local_host::daemon::tests::daemon_process_fixture",
                    "--ignored",
                ])
                .env("NEBULA_DESKTOP_CONTROL_TEST_STATE", state)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
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
            Self(command.spawn().unwrap())
        }

        async fn exit(&mut self) -> ExitStatus {
            for _ in 0..200 {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("the owned control fixture did not exit");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Test-only cleanup, solely through this test's owned Child handle.
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    async fn await_process(host: &LocalAgent) {
        for _ in 0..100 {
            if host.snapshot().await.unwrap().running {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the isolated control fixture did not start");
    }

    #[test]
    #[ignore = "launched only as an isolated child by the process-control tests"]
    fn daemon_process_fixture() {
        let Some(state) = std::env::var_os("NEBULA_DESKTOP_CONTROL_TEST_STATE") else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Never create native(), Agent, or a Manager connection in tests.
        runtime
            .block_on(run_service(PathBuf::from(state), |_| {
                Ok(std::future::pending::<()>())
            }))
            .unwrap();
    }

    #[tokio::test]
    async fn detached_service_survives_desktop_restart_and_stops_from_new_instance() {
        let dir = TestDir::new();
        save_identity(&dir.identity(), true);
        let mut service = Fixture::start(&dir.identity());
        let original = LocalAgent::new(dir.identity(), "must-not-run".into());
        await_process(&original).await;
        assert_eq!(
            original.snapshot().await.unwrap().connection_state,
            "unknown"
        );
        drop(original);
        let restarted = LocalAgent::new(dir.identity(), "must-not-run".into());
        assert!(restarted.set_enabled(true).await.unwrap().running);
        let mut duplicate = Fixture::start(&dir.identity());
        assert!(!duplicate.exit().await.success());
        assert!(restarted.snapshot().await.unwrap().running);
        assert!(!restarted.set_enabled(false).await.unwrap().running);
        assert!(service.exit().await.success());
        assert!(!path_exists(&manifest_path(&dir.identity())).unwrap());
    }

    #[tokio::test]
    async fn crashed_service_reclaims_stale_manifest_and_rotates_capability() {
        let dir = TestDir::new();
        save_identity(&dir.identity(), true);
        let host = LocalAgent::new(dir.identity(), "must-not-run".into());
        let mut first = Fixture::start(&dir.identity());
        await_process(&host).await;
        let old = read_manifest(&dir.identity()).unwrap();
        first.0.kill().unwrap();
        first.exit().await;
        assert!(path_exists(&manifest_path(&dir.identity())).unwrap());
        assert!(!host.snapshot().await.unwrap().running);
        let mut replacement = Fixture::start(&dir.identity());
        await_process(&host).await;
        let replacement_manifest = read_manifest(&dir.identity()).unwrap();
        assert!(!capability_matches(
            &old.capability,
            &replacement_manifest.capability
        ));
        host.set_enabled(false).await.unwrap();
        assert!(replacement.exit().await.success());
    }

    #[tokio::test]
    async fn malformed_and_external_identities_are_never_adopted_or_overwritten() {
        for external in [false, true] {
            let dir = TestDir::new();
            if external {
                save_identity(&dir.identity(), false);
            } else {
                private_new(&dir.identity())
                    .unwrap()
                    .write_all(b"malformed")
                    .unwrap();
            }
            let before = Zeroizing::new(fs::read(dir.identity()).unwrap());
            let mut service = Fixture::start(&dir.identity());
            assert!(!service.exit().await.success());
            assert!(before.as_slice() == fs::read(dir.identity()).unwrap());
            assert!(!path_exists(&manifest_path(&dir.identity())).unwrap());
        }
    }

    #[test]
    fn protocol_authenticates_and_rejects_unknown_operations_and_fields() {
        let capability = Uuid::new_v4().to_string();
        let body = serde_json::json!({"capability":capability, "command":"status"});
        assert!(authenticate(&serde_json::to_vec(&body).unwrap(), &capability).is_ok());
        assert!(authenticate(
            &serde_json::to_vec(&body).unwrap(),
            &Uuid::new_v4().to_string()
        )
        .is_err());
        let unknown =
            serde_json::json!({"capability":capability,"command":"run","path":"/ignored"});
        assert!(authenticate(&serde_json::to_vec(&unknown).unwrap(), &capability).is_err());
        let extra = serde_json::json!({"capability":capability,"command":"stop","path":"/ignored"});
        assert!(authenticate(&serde_json::to_vec(&extra).unwrap(), &capability).is_err());
    }

    #[tokio::test]
    async fn line_reader_is_strictly_bounded_and_requires_newline() {
        let mut valid = &b"{\"command\":\"status\"}\nignored"[..];
        assert_eq!(
            &**read_line(&mut valid).await.unwrap(),
            b"{\"command\":\"status\"}"
        );
        let large = vec![b'x'; MAX_LINE + 20];
        assert!(read_line(&mut large.as_slice()).await.is_err());
        assert!(read_line(&mut &b"no-newline"[..]).await.is_err());
    }

    #[tokio::test]
    async fn stalled_input_obeys_deadline_without_network() {
        let (_writer, mut reader) = tokio::io::duplex(64);
        assert!(timeout(Duration::from_millis(10), read_line(&mut reader))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn authenticated_status_and_stop_work_without_owning_a_child_handle() {
        let manifest = ControlManifest {
            port: 1,
            capability: Uuid::new_v4().to_string(),
            machine_id: Uuid::new_v4(),
        };
        for command in [ControlCommand::Status, ControlCommand::Stop] {
            let (mut client, service) = tokio::io::duplex(MAX_LINE);
            let request = ControlRequest {
                capability: manifest.capability.clone(),
                command,
            };
            let mut bytes = Zeroizing::new(serde_json::to_vec(&request).unwrap());
            bytes.push(b'\n');
            client.write_all(&bytes).await.unwrap();
            let (stop, response) =
                tokio::join!(respond(service, &manifest), read_line(&mut client));
            assert_eq!(stop.unwrap(), command == ControlCommand::Stop);
            let status: ControlStatus = serde_json::from_slice(&response.unwrap()).unwrap();
            assert_eq!(status.machine_id, manifest.machine_id);
            assert_eq!(status.running, command == ControlCommand::Status);
        }
    }

    #[test]
    fn capability_comparison_requires_exact_random_capability() {
        let capability = Uuid::new_v4().to_string();
        assert!(capability_matches(&capability, &capability));
        assert!(!capability_matches("", &capability));
        assert!(!capability_matches(
            &Uuid::new_v4().to_string(),
            &capability
        ));
    }
}
