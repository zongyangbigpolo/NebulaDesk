use std::{collections::HashMap, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

pub use nebula_desktop_protocol::Command as ChildCommand;
use nebula_desktop_protocol::{
    is_safe_session_error, Event, Launch, LaunchTicket, MAX_LINE_BYTES, VERSION,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    error::{DesktopError, Result},
    model::*,
};

const MAX_LINE: usize = MAX_LINE_BYTES;
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
    #[serde(default)]
    pub application_windows: bool,
}

struct Entry {
    view: Session,
    policy: Policy,
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
        Self {
            binary,
            registry: Arc::new(Mutex::new(Registry::default())),
        }
    }

    pub async fn list(&self) -> Vec<Session> {
        self.registry
            .lock()
            .await
            .sessions
            .values()
            .map(|e| e.view.clone())
            .collect()
    }

    pub async fn transfers(&self) -> Vec<Transfer> {
        self.registry
            .lock()
            .await
            .transfers
            .values()
            .cloned()
            .collect()
    }

    pub async fn existing(&self, resource: Uuid) -> Option<Session> {
        self.registry
            .lock()
            .await
            .sessions
            .values()
            .find(|e| e.view.resource_id == resource && e.view.state.active())
            .map(|e| e.view.clone())
    }

    pub async fn start(
        &self,
        resource: Resource,
        ticket: Ticket,
        keyboard_profile: ApplicationKeyboardProfile,
    ) -> Result<Session> {
        if !resource.launch_supported
            || matches!(resource.kind, Kind::App) != ticket.application_windows
            || (!ticket.application_windows
                && keyboard_profile != ApplicationKeyboardProfile::Physical)
            || (ticket.application_windows
                && (ticket.policy.audio || ticket.policy.clipboard || ticket.policy.file_transfer))
        {
            return Err(DesktopError::protocol());
        }
        let mut registry = self.registry.lock().await;
        if registry
            .sessions
            .values()
            .filter(|e| !e.done.is_cancelled())
            .count()
            >= MAX_SESSIONS
        {
            return Err(DesktopError::new("limit", "Too many active sessions."));
        }
        registry.sessions.retain(|_, e| !e.done.is_cancelled());
        let id = ticket.session_id;
        let mut command = Command::new(&self.binary);
        command.arg("desktop-session");
        if ticket.application_windows {
            command.args(keyboard_profile.client_args());
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| {
                DesktopError::new(
                    "client_unavailable",
                    "The native client could not start. Install or build the bundled sidecar.",
                )
            })?;
        let stdin = child.stdin.take().ok_or_else(DesktopError::protocol)?;
        let stdout = child.stdout.take().ok_or_else(DesktopError::protocol)?;
        let view = Session {
            session_id: id,
            resource_id: resource.id,
            name: resource.name.clone(),
            state: SessionState::Connecting,
            path: None,
            started_at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| DesktopError::protocol())?,
            error: None,
            rtt_ms: None,
        };
        let launch = Launch {
            version: VERSION,
            resource_id: resource.id.to_string(),
            resource_name: resource.name,
            application_windows: ticket.application_windows,
            ticket: LaunchTicket {
                session_id: ticket.session_id.to_string(),
                ticket: ticket.ticket,
                gateway_addr: ticket.gateway_addr,
                gateway_pin: ticket.gateway_pin,
                agent_key: ticket.agent_key,
                policy: ticket.policy,
            },
        };
        let policy = launch.ticket.policy;
        let (commands, mut receiver) = mpsc::channel(16);
        let stop = CancellationToken::new();
        let done = CancellationToken::new();
        registry.sessions.insert(
            id,
            Entry {
                view: view.clone(),
                policy,
                commands,
                stop: stop.clone(),
                done: done.clone(),
            },
        );
        let this = self.clone();
        tokio::spawn(async move {
            let mut stdin = stdin;
            let mut reader = BufReader::new(stdout);
            let (events, mut event_rx) = mpsc::channel(32);
            // The reader is never cancelled between bytes when a command arrives.
            let reader_task = tokio::spawn(async move {
                loop {
                    let event = match read_line(&mut reader).await {
                        Ok(Some(line)) => parse_event(&line).map(Some),
                        Ok(None) => Ok(None),
                        Err(error) => Err(error),
                    };
                    let finished = !matches!(event, Ok(Some(_)));
                    if events.send(event).await.is_err() || finished {
                        break;
                    }
                }
            });
            let launch_result = write_line(&mut stdin, &launch).await;
            drop(launch);
            let mut failed = launch_result.is_err();
            if !failed {
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        event = event_rx.recv() => {
                            match event {
                                Some(Ok(Some(event))) => {
                                    let terminal = this.event(id, event).await;
                                    if terminal { break; }
                                }
                                Some(Ok(None)) | None => break,
                                Some(Err(_)) => { failed = true; break; }
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
            reader_task.abort();
            let _ = reader_task.await;
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                write_line(&mut stdin, &ChildCommand::Disconnect {}),
            )
            .await;
            drop(stdin);
            match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
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
                        entry.view.error = error.map(|message| {
                            if is_safe_session_error(&message) {
                                message
                            } else {
                                "The native session reported an error.".into()
                            }
                        });
                    }
                }
                !state.active()
            }
            Event::Metrics { rtt_ms, .. } => {
                if let Some(entry) = registry.sessions.get_mut(&id) {
                    entry.view.rtt_ms = rtt_ms.filter(|n| n.is_finite() && *n >= 0.0);
                }
                false
            }
            Event::Transfer {
                id: transfer_id,
                name,
                direction,
                transferred,
                total,
                state,
                error,
            } => {
                let key = format!("{id}:{transfer_id}");
                if registry.transfers.len() >= MAX_TRANSFERS
                    && !registry.transfers.contains_key(&key)
                {
                    registry.transfers.retain(|_, t| {
                        matches!(
                            t.state,
                            TransferState::Offered | TransferState::Transferring
                        )
                    });
                }
                if registry.transfers.len() < MAX_TRANSFERS || registry.transfers.contains_key(&key)
                {
                    registry.transfers.insert(
                        key.clone(),
                        Transfer {
                            id: key,
                            session_id: id,
                            name: name.chars().take(255).collect(),
                            direction,
                            transferred,
                            total,
                            state,
                            error: error.map(|_| "The file transfer failed.".into()),
                        },
                    );
                }
                false
            }
        }
    }

    async fn finish(&self, id: Uuid, failed: bool) {
        let mut registry = self.registry.lock().await;
        if let Some(entry) = registry.sessions.get_mut(&id) {
            if entry.view.state.active() || failed {
                entry.view.state = if failed {
                    SessionState::Failed
                } else {
                    SessionState::Disconnected
                };
                if failed && entry.view.error.is_none() {
                    entry.view.error = Some("The native session ended unexpectedly.".into());
                } else if !failed {
                    entry.view.error = None;
                }
            }
        }
        for transfer in registry
            .transfers
            .values_mut()
            .filter(|t| t.session_id == id)
        {
            if matches!(
                transfer.state,
                TransferState::Offered | TransferState::Transferring
            ) {
                transfer.state = TransferState::Failed;
                transfer.error = Some("The session ended before this transfer completed.".into());
            }
        }
    }

    pub async fn file_transfer_allowed(&self, id: Uuid) -> bool {
        self.registry
            .lock()
            .await
            .sessions
            .get(&id)
            .is_some_and(|entry| {
                entry.view.state == SessionState::Connected && entry.policy.file_transfer
            })
    }

    pub async fn command(&self, id: Uuid, command: ChildCommand) -> Result<()> {
        command.validate().map_err(|_| DesktopError::protocol())?;
        let registry = self.registry.lock().await;
        let entry = registry
            .sessions
            .get(&id)
            .filter(|e| e.view.state.active())
            .ok_or_else(|| {
                DesktopError::new("session_closed", "This session is no longer active.")
            })?;
        let allowed = match &command {
            ChildCommand::SendFiles { .. } => {
                entry.view.state == SessionState::Connected && entry.policy.file_transfer
            }
            ChildCommand::SetAudio { enabled: true } => entry.policy.audio,
            ChildCommand::SetClipboard { enabled: true } => entry.policy.clipboard,
            _ => true,
        };
        if !allowed {
            return Err(DesktopError::new(
                "permission_denied",
                "This session does not allow that channel.",
            ));
        }
        entry.commands.try_send(command).map_err(|_| {
            DesktopError::new("session_busy", "The session command queue is unavailable.")
        })
    }

    pub async fn disconnect(&self, id: Uuid) -> Result<()> {
        let done = {
            let registry = self.registry.lock().await;
            let entry = registry.sessions.get(&id).ok_or_else(|| {
                DesktopError::new("session_closed", "This session is no longer active.")
            })?;
            entry.stop.cancel();
            entry.done.clone()
        };
        done.cancelled().await;
        Ok(())
    }

    pub async fn stop_all(&self) {
        let done: Vec<_> = {
            let registry = self.registry.lock().await;
            registry
                .sessions
                .values()
                .map(|entry| {
                    entry.stop.cancel();
                    entry.done.clone()
                })
                .collect()
        };
        for done in done {
            done.cancelled().await;
        }
        let mut registry = self.registry.lock().await;
        registry.sessions.clear();
        registry.transfers.clear();
    }
}

async fn write_line<T: Serialize>(
    writer: &mut tokio::process::ChildStdin,
    value: &T,
) -> Result<()> {
    let mut bytes =
        zeroize::Zeroizing::new(serde_json::to_vec(value).map_err(|_| DesktopError::protocol())?);
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE {
        return Err(DesktopError::protocol());
    }
    tokio::time::timeout(Duration::from_secs(2), writer.write_all(&bytes))
        .await
        .map_err(|_| DesktopError::protocol())?
        .map_err(|_| DesktopError::protocol())
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let n = reader
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|_| DesktopError::protocol())?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_LINE || bytes.last() != Some(&b'\n') {
        return Err(DesktopError::protocol());
    }
    Ok(Some(bytes))
}

fn parse_event(bytes: &[u8]) -> Result<Event> {
    if bytes.len() > MAX_LINE {
        return Err(DesktopError::protocol());
    }
    serde_json::from_slice(bytes).map_err(|_| DesktopError::protocol())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_desktop_protocol::{
        APPLICATION_BACKEND_UNAVAILABLE, APPLICATION_PERMISSION_REQUIRED, APPLICATION_START_FAILED,
        REMOTE_SESSION_ENDED,
    };

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

    #[tokio::test]
    async fn active_resource_dedup_and_terminal_snapshot_are_truthful() {
        terminal_snapshot("SECRET").await;
    }

    #[tokio::test]
    async fn fixed_remote_failure_guidance_survives_child_exit() {
        terminal_snapshot(REMOTE_SESSION_ENDED).await;
    }

    #[tokio::test]
    async fn application_failure_guidance_is_preserved_but_arbitrary_details_are_not() {
        for message in [
            APPLICATION_START_FAILED,
            APPLICATION_BACKEND_UNAVAILABLE,
            APPLICATION_PERMISSION_REQUIRED,
        ] {
            terminal_snapshot(message).await;
            terminal_snapshot(&format!("{message} SECRET /private/application")).await;
        }
    }

    #[tokio::test]
    async fn application_mode_and_global_permissions_are_checked_before_spawn() {
        for (kind, application_windows, supported, global_channel, allowed) in [
            ("APP", false, true, None, false),
            ("DESKTOP", true, true, None, false),
            ("APP", true, false, None, false),
            ("APP", true, true, Some("audio"), false),
            ("APP", true, true, Some("clipboard"), false),
            ("APP", true, true, Some("file_transfer"), false),
            ("APP", true, true, None, true),
            ("DESKTOP", false, true, Some("audio"), true),
        ] {
            let mut policy = serde_json::json!({
                "input":true,"audio":false,"clipboard":false,"file_transfer":false
            });
            if let Some(channel) = global_channel {
                policy[channel] = true.into();
            }
            let resource = serde_json::from_value(serde_json::json!({
                "id":Uuid::new_v4(),"name":"Resource","kind":kind,
                "description":"","machine_status":"ONLINE","role":"CONTROLLER",
                "policy":policy,"owned":false,"launch_supported":supported
            }))
            .unwrap();
            let ticket = serde_json::from_value(serde_json::json!({
                "session_id":Uuid::new_v4(),"ticket":"opaque-test-ticket",
                "gateway_addr":"127.0.0.1:7443","gateway_pin":"test-pin",
                "agent_key":"test-key","policy":policy,
                "application_windows":application_windows
            }))
            .unwrap();
            let sessions = Sessions::new("missing-application-test-client".into());
            let error = sessions
                .start(resource, ticket, ApplicationKeyboardProfile::Physical)
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.code,
                if allowed {
                    "client_unavailable"
                } else {
                    "protocol"
                }
            );
            assert!(sessions.list().await.is_empty());
        }
    }

    #[test]
    fn old_tickets_default_to_desktop_mode_only() {
        let ticket: Ticket = serde_json::from_value(serde_json::json!({
            "session_id":Uuid::new_v4(),"ticket":"opaque-test-ticket",
            "gateway_addr":"127.0.0.1:7443","gateway_pin":"test-pin",
            "agent_key":"test-key",
            "policy":{"input":true,"audio":true,"clipboard":true,"file_transfer":true}
        }))
        .unwrap();
        assert!(!ticket.application_windows);
    }

    #[test]
    fn keyboard_profiles_are_opt_in_and_become_only_fixed_local_arguments() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "op":"connect","resource_id":Uuid::nil()
        }))
        .unwrap();
        assert!(matches!(
            request,
            Request::Connect {
                keyboard_profile: ApplicationKeyboardProfile::Physical,
                ..
            }
        ));
        assert_eq!(
            ApplicationKeyboardProfile::Editing.client_args(),
            [
                "--keyboard-mode",
                "semantic",
                "--keyboard-profile",
                "editing"
            ]
        );
        assert_eq!(
            ApplicationKeyboardProfile::Terminal.client_args(),
            [
                "--keyboard-mode",
                "semantic",
                "--keyboard-profile",
                "terminal"
            ]
        );
        assert!(serde_json::from_value::<Request>(serde_json::json!({
            "op":"connect","resource_id":Uuid::nil(),"keyboard_profile":"--run-command"
        }))
        .is_err());
    }

    async fn terminal_snapshot(message: &str) {
        let sessions = Sessions::new("unused-client".into());
        let id = Uuid::new_v4();
        let resource = Uuid::new_v4();
        let (commands, _receiver) = mpsc::channel(1);
        let done = CancellationToken::new();
        let view = Session {
            session_id: id,
            resource_id: resource,
            name: "Desktop".into(),
            state: SessionState::Connecting,
            path: None,
            started_at: String::new(),
            error: None,
            rtt_ms: None,
        };
        sessions.registry.lock().await.sessions.insert(
            id,
            Entry {
                view,
                policy: Policy::default(),
                commands,
                stop: CancellationToken::new(),
                done: done.clone(),
            },
        );
        let (a, b) = tokio::join!(sessions.existing(resource), sessions.existing(resource));
        assert_eq!(a.unwrap().session_id, b.unwrap().session_id);
        assert!(!sessions.file_transfer_allowed(id).await);
        for command in [
            ChildCommand::SendFiles {
                paths: vec!["/private/test-file".into()],
            },
            ChildCommand::SetAudio { enabled: true },
            ChildCommand::SetClipboard { enabled: true },
        ] {
            assert_eq!(
                sessions.command(id, command).await.unwrap_err().code,
                "permission_denied"
            );
        }
        sessions
            .event(
                id,
                Event::State {
                    state: SessionState::Failed,
                    path: None,
                    error: Some(message.into()),
                },
            )
            .await;
        assert!(sessions.existing(resource).await.is_none());
        sessions
            .event(
                id,
                Event::State {
                    state: SessionState::Connected,
                    path: None,
                    error: None,
                },
            )
            .await;
        assert_eq!(sessions.list().await[0].state, SessionState::Failed);
        assert!(!sessions.list().await[0]
            .error
            .as_ref()
            .unwrap()
            .contains("SECRET"));
        let expected = if is_safe_session_error(message) {
            message
        } else {
            "The native session reported an error."
        };
        sessions.finish(id, true).await;
        assert_eq!(sessions.list().await[0].error.as_deref(), Some(expected));
        sessions.finish(id, false).await;
        assert_eq!(sessions.list().await[0].state, SessionState::Failed);
        assert_eq!(sessions.list().await[0].error.as_deref(), Some(expected));
        done.cancel();
        sessions.stop_all().await;
        assert!(sessions.list().await.is_empty());
    }
}
