use std::{collections::HashMap, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader}, process::Command, sync::{mpsc, Mutex}};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{error::{DesktopError, Result}, model::*};

const MAX_LINE: usize = 64 * 1024;
const MAX_SESSIONS: usize = 32;
const MAX_TRANSFERS: usize = 256;

#[derive(Deserialize, Serialize)]
pub struct Ticket {
    pub session_id: Uuid,
    pub ticket: String,
    pub gateway_addr: String,
    pub gateway_pin: String,
    pub agent_key: String,
    pub policy: Policy,
}

#[derive(Serialize)]
struct Launch {
    version: u32,
    resource_id: Uuid,
    resource_name: String,
    ticket: Ticket,
}

#[derive(Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum ChildCommand {
    Focus,
    Disconnect,
    SendFiles { paths: Vec<PathBuf> },
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    State { state: SessionState, path: Option<ConnectionPath>, error: Option<String> },
    Metrics { rtt_ms: Option<f64> },
    Transfer { id: String, name: String, direction: Direction, transferred: u64, total: u64, state: TransferState, error: Option<String> },
}

struct Entry {
    view: Session,
    commands: mpsc::Sender<ChildCommand>,
    stop: CancellationToken,
    done: CancellationToken,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<Uuid, Entry>,
    transfers: HashMap<String, Transfer>,
}

#[derive(Clone)]
pub struct Sessions {
    binary: PathBuf,
    registry: Arc<Mutex<Registry>>,
}

impl Sessions {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary, registry: Arc::new(Mutex::new(Registry::default())) }
    }

    pub async fn list(&self) -> Vec<Session> {
        self.registry.lock().await.sessions.values().map(|e| e.view.clone()).collect()
    }

    pub async fn transfers(&self) -> Vec<Transfer> {
        self.registry.lock().await.transfers.values().cloned().collect()
    }

    pub async fn existing(&self, resource: Uuid) -> Option<Session> {
        self.registry.lock().await.sessions.values()
            .find(|e| e.view.resource_id == resource && e.view.state.active())
            .map(|e| e.view.clone())
    }

    pub async fn start(&self, resource: Resource, ticket: Ticket) -> Result<Session> {
        let mut registry = self.registry.lock().await;
        if registry.sessions.values().filter(|e| e.view.state.active()).count() >= MAX_SESSIONS {
            return Err(DesktopError::new("limit", "Too many active sessions."));
        }
        registry.sessions.retain(|_, e| e.view.state.active());
        let id = ticket.session_id;
        let mut child = Command::new(&self.binary)
            .arg("desktop-session")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn().map_err(|_| DesktopError::new("client_unavailable", "The native client could not start. Install or build the bundled sidecar."))?;
        let stdin = child.stdin.take().ok_or_else(DesktopError::protocol)?;
        let stdout = child.stdout.take().ok_or_else(DesktopError::protocol)?;
        let view = Session {
            session_id: id, resource_id: resource.id, name: resource.name.clone(),
            state: SessionState::Connecting, path: None,
            started_at: time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).map_err(|_| DesktopError::protocol())?,
            error: None, rtt_ms: None,
        };
        let launch = Launch { version: 1, resource_id: resource.id, resource_name: resource.name, ticket };
        let (commands, mut receiver) = mpsc::channel(16);
        let stop = CancellationToken::new();
        let done = CancellationToken::new();
        registry.sessions.insert(id, Entry { view: view.clone(), commands, stop: stop.clone(), done: done.clone() });
        let this = self.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            let mut reader = BufReader::new(stdout);
            let launch_result = write_line(&mut stdin, &launch).await;
            drop(launch);
            let mut failed = launch_result.is_err();
            if !failed {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        status = child.wait() => {
                            failed = !status.is_ok_and(|s| s.success());
                            break;
                        },
                        line = read_line(&mut reader) => {
                            match line {
                                Ok(Some(line)) => match parse_event(&line) {
                                    Ok(event) => {
                                        let terminal = this.event(id, event).await;
                                        if terminal { break; }
                                    }
                                    Err(_) => { failed = true; break; }
                                },
                                Ok(None) => break,
                                Err(_) => { failed = true; break; }
                            }
                        },
                        command = receiver.recv() => {
                            match command {
                                Some(command) => {
                                    if write_line(&mut stdin, &command).await.is_err() { failed = true; break; }
                                }
                                None => break,
                            }
                        }
                    }
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), write_line(&mut stdin, &ChildCommand::Disconnect)).await;
            drop(stdin);
            match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
                Ok(Ok(status)) => failed |= !stop.is_cancelled() && !status.success(),
                Ok(Err(_)) => failed = true,
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    failed |= !stop.is_cancelled();
                }
            }
            this.finish(id, failed).await;
            done.cancel();
        });
        Ok(view)
    }

    async fn event(&self, id: Uuid, event: Event) -> bool {
        let mut registry = self.registry.lock().await;
        match event {
            Event::State { state, path, error } => {
                if let Some(entry) = registry.sessions.get_mut(&id) {
                    if entry.view.state.active() {
                        entry.view.state = state;
                        entry.view.path = path;
                        entry.view.error = error.map(|_| "The native session reported an error.".into());
                    }
                }
                !state.active()
            }
            Event::Metrics { rtt_ms } => {
                if let Some(entry) = registry.sessions.get_mut(&id) {
                    entry.view.rtt_ms = rtt_ms.filter(|n| n.is_finite() && *n >= 0.0);
                }
                false
            }
            Event::Transfer { id: transfer_id, name, direction, transferred, total, state, error } => {
                let key = format!("{id}:{transfer_id}");
                if registry.transfers.len() >= MAX_TRANSFERS && !registry.transfers.contains_key(&key) {
                    registry.transfers.retain(|_, t| matches!(t.state, TransferState::Offered | TransferState::Transferring));
                }
                if registry.transfers.len() < MAX_TRANSFERS || registry.transfers.contains_key(&key) {
                    registry.transfers.insert(key.clone(), Transfer {
                        id: key, session_id: id, name: name.chars().take(255).collect(),
                        direction, transferred, total, state,
                        error: error.map(|_| "The file transfer failed.".into()),
                    });
                }
                false
            }
        }
    }

    async fn finish(&self, id: Uuid, failed: bool) {
        let mut registry = self.registry.lock().await;
        if let Some(entry) = registry.sessions.get_mut(&id) {
            if entry.view.state.active() || failed {
                entry.view.state = if failed { SessionState::Failed } else { SessionState::Disconnected };
                entry.view.error = failed.then(|| "The native session ended unexpectedly.".into());
            }
        }
        for transfer in registry.transfers.values_mut().filter(|t| t.session_id == id) {
            if matches!(transfer.state, TransferState::Offered | TransferState::Transferring) {
                transfer.state = TransferState::Failed;
                transfer.error = Some("The session ended before this transfer completed.".into());
            }
        }
    }

    pub async fn command(&self, id: Uuid, command: ChildCommand) -> Result<()> {
        let registry = self.registry.lock().await;
        let entry = registry.sessions.get(&id).filter(|e| e.view.state.active())
            .ok_or_else(|| DesktopError::new("session_closed", "This session is no longer active."))?;
        entry.commands.try_send(command).map_err(|_| DesktopError::new("session_busy", "The session command queue is unavailable."))
    }

    pub async fn disconnect(&self, id: Uuid) -> Result<()> {
        let done = {
            let registry = self.registry.lock().await;
            let entry = registry.sessions.get(&id)
                .ok_or_else(|| DesktopError::new("session_closed", "This session is no longer active."))?;
            entry.stop.cancel();
            entry.done.clone()
        };
        done.cancelled().await;
        Ok(())
    }

    pub async fn stop_all(&self) {
        let done: Vec<_> = {
            let registry = self.registry.lock().await;
            registry.sessions.values().map(|entry| {
                entry.stop.cancel();
                entry.done.clone()
            }).collect()
        };
        for done in done { done.cancelled().await; }
        let mut registry = self.registry.lock().await;
        registry.sessions.clear();
        registry.transfers.clear();
    }
}

async fn write_line<T: Serialize>(writer: &mut tokio::process::ChildStdin, value: &T) -> Result<()> {
    let mut bytes = zeroize::Zeroizing::new(serde_json::to_vec(value).map_err(|_| DesktopError::protocol())?);
    if bytes.len() > MAX_LINE { return Err(DesktopError::protocol()); }
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(2), writer.write_all(&bytes)).await
        .map_err(|_| DesktopError::protocol())?.map_err(|_| DesktopError::protocol())
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let n = reader.take((MAX_LINE + 1) as u64).read_until(b'\n', &mut bytes).await
        .map_err(|_| DesktopError::protocol())?;
    if n == 0 { return Ok(None); }
    if n > MAX_LINE || bytes.last() != Some(&b'\n') { return Err(DesktopError::protocol()); }
    Ok(Some(bytes))
}

fn parse_event(bytes: &[u8]) -> Result<Event> {
    if bytes.len() > MAX_LINE { return Err(DesktopError::protocol()); }
    serde_json::from_slice(bytes).map_err(|_| DesktopError::protocol())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_ndjson_rejects_partial_and_oversized_lines() {
        assert!(read_line(&mut &b"{}"[..]).await.is_err());
        assert!(read_line(&mut &vec![b'x'; MAX_LINE + 2][..]).await.is_err());
        assert!(read_line(&mut &b"{}\n"[..]).await.unwrap().is_some());
    }

    #[test]
    fn invalid_events_do_not_expose_payload() {
        let error = parse_event(br#"{"event":"secret-ticket"}"#).err().unwrap();
        assert!(!format!("{error:?}").contains("secret-ticket"));
    }

    #[test]
    fn terminal_states_are_not_deduplicated() {
        assert!(!SessionState::Failed.active());
        assert!(!SessionState::Disconnected.active());
        assert!(SessionState::Connecting.active());
    }
}
