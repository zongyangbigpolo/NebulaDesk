use std::os::fd::OwnedFd;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use anyhow::Context;
use ashpd::desktop::{
    remote_desktop::{DeviceType, RemoteDesktop},
    screencast::{CursorMode, Screencast, SourceType},
    PersistMode, Session,
};
use futures::StreamExt;
use ndp_proto::InputEvent;
use tokio::sync::{mpsc as async_mpsc, watch};

use super::input::InputState;

const CONSENT_TIMEOUT: Duration = Duration::from_secs(120);
const CALL_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct Stream {
    pub fd: OwnedFd,
    pub node: u32,
    pub width: u32,
    pub height: u32,
}

struct Command {
    event: InputEvent,
    reply: mpsc::SyncSender<anyhow::Result<()>>,
}

/// Owns the D-Bus connection's executor, independently of the caller's runtime.
pub(super) struct Portal {
    commands: async_mpsc::Sender<Command>,
    stop: watch::Sender<bool>,
    pub active: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Portal {
    pub fn open(allow_input: bool) -> anyhow::Result<(Self, Stream)> {
        let (commands, incoming) = async_mpsc::channel(32);
        let (stop, stopped) = watch::channel(false);
        let (ready, started) = mpsc::sync_channel(1);
        let active = Arc::new(AtomicBool::new(false));
        let worker_active = active.clone();
        let worker = std::thread::Builder::new()
            .name("nebula-portal".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(error.into()));
                        return;
                    }
                };
                runtime.block_on(async {
                    if let Err(error) =
                        run(allow_input, incoming, stopped, &ready, &worker_active).await
                    {
                        tracing::error!(%error, "Linux desktop portal stopped");
                        let _ = ready.try_send(Err(error));
                    }
                    worker_active.store(false, Ordering::Release);
                });
            })?;
        let portal = Self {
            commands,
            stop,
            active,
            worker: Some(worker),
        };
        let stream = started
            .recv_timeout(CONSENT_TIMEOUT + CALL_TIMEOUT)
            .context("desktop portal consent timed out or its worker stopped")??;
        Ok((portal, stream))
    }

    pub fn inject(&self, event: InputEvent) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.active.load(Ordering::Acquire),
            "desktop portal session is closed"
        );
        let (reply, response) = mpsc::sync_channel(1);
        self.commands
            .try_send(Command { event, reply })
            .context("desktop portal input queue is full or closed")?;
        response
            .recv_timeout(CALL_TIMEOUT + Duration::from_secs(1))
            .context("desktop portal input acknowledgement timed out")?
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("desktop portal worker panicked");
            }
        }
    }
}

enum DesktopSession {
    View(Session<'static, Screencast<'static>>),
    Control(Session<'static, RemoteDesktop<'static>>),
}

impl DesktopSession {
    async fn close(&self) {
        let result = tokio::time::timeout(CALL_TIMEOUT, async {
            match self {
                Self::View(session) => session.close().await,
                Self::Control(session) => session.close().await,
            }
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(?result, "could not close desktop portal session cleanly");
        }
    }
}

async fn run(
    allow_input: bool,
    mut commands: async_mpsc::Receiver<Command>,
    mut stopped: watch::Receiver<bool>,
    ready: &mpsc::SyncSender<anyhow::Result<Stream>>,
    active: &AtomicBool,
) -> anyhow::Result<()> {
    let cast = tokio::time::timeout(CALL_TIMEOUT, Screencast::new()).await??;
    let remote = if allow_input {
        Some(tokio::time::timeout(CALL_TIMEOUT, RemoteDesktop::new()).await??)
    } else {
        None
    };
    let session = tokio::time::timeout(CALL_TIMEOUT, async {
        Ok::<_, anyhow::Error>(match &remote {
            Some(remote) => DesktopSession::Control(remote.create_session().await?),
            None => DesktopSession::View(cast.create_session().await?),
        })
    })
    .await??;
    // Close explicitly on cancellation, denial, timeout, or a failed media setup.
    let result = tokio::select! {
        _ = stopped.changed() => Ok(()),
        result = async {
            let stream = tokio::time::timeout(CONSENT_TIMEOUT, select(&cast, remote.as_ref(), &session))
                .await.context("desktop sharing consent timed out")??;
            let node = stream.node;
            let geometry = (stream.width, stream.height);
            let mut closed = match &session {
                DesktopSession::View(s) => s.receive_closed().await?.boxed(),
                DesktopSession::Control(s) => s.receive_closed().await?.boxed(),
            };
            active.store(true, Ordering::Release);
            ready.send(Ok(stream)).context("capture stopped while waiting for portal consent")?;
            let mut input = InputState::default();
            loop {
                tokio::select! {
                    _ = closed.next() => anyhow::bail!("desktop user revoked sharing permission"),
                    command = commands.recv() => {
                        let Some(command) = command else { break; };
                        let result = tokio::time::timeout(CALL_TIMEOUT, async {
                            let (Some(remote), DesktopSession::Control(session)) = (&remote, &session) else {
                                anyhow::bail!("view-only portal session cannot inject input");
                            };
                            input.inject(remote, session, node, geometry, &command.event).await
                        }).await.context("portal input call timed out").and_then(|result| result);
                        // A failed/partial input must revoke the session: a queued release
                        // must not execute later against a stale permission or stuck key.
                        let failed = result.is_err();
                        let _ = command.reply.send(result);
                        if failed { anyhow::bail!("portal input failed; revoking session"); }
                    }
                }
            }
            Ok(())
        } => result,
    };
    active.store(false, Ordering::Release);
    session.close().await;
    result
}

async fn select(
    cast: &Screencast<'static>,
    remote: Option<&RemoteDesktop<'static>>,
    session: &DesktopSession,
) -> anyhow::Result<Stream> {
    let (streams, fd) = match session {
        DesktopSession::View(session) => {
            cast.select_sources(
                session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await?
            .response()?;
            let response = cast.start(session, None).await?.response()?;
            (
                response.streams().to_vec(),
                cast.open_pipe_wire_remote(session).await?,
            )
        }
        DesktopSession::Control(session) => {
            let remote = remote.context("missing RemoteDesktop proxy")?;
            remote
                .select_devices(
                    session,
                    DeviceType::Keyboard | DeviceType::Pointer,
                    None,
                    PersistMode::DoNot,
                )
                .await?
                .response()?;
            cast.select_sources(
                session,
                CursorMode::Embedded,
                SourceType::Monitor.into(),
                false,
                None,
                PersistMode::DoNot,
            )
            .await?
            .response()?;
            let response = remote.start(session, None).await?.response()?;
            anyhow::ensure!(
                response
                    .devices()
                    .contains(DeviceType::Keyboard | DeviceType::Pointer),
                "desktop user did not grant keyboard and pointer control"
            );
            (
                response
                    .streams()
                    .context("portal returned no screen streams")?
                    .to_vec(),
                cast.open_pipe_wire_remote(session).await?,
            )
        }
    };
    anyhow::ensure!(
        streams.len() == 1,
        "select exactly one monitor in the sharing dialog"
    );
    let stream = &streams[0];
    let (width, height) = stream
        .size()
        .context("portal omitted monitor logical size")?;
    anyhow::ensure!(
        width > 0 && height > 0,
        "portal returned invalid monitor geometry"
    );
    Ok(Stream {
        fd,
        node: stream.pipe_wire_node_id(),
        width: width as u32,
        height: height as u32,
    })
}
