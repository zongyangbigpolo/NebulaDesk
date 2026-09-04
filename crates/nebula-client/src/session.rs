//! Running one session: window, decoder, and the two directions of traffic.
//!
//! Three threads of control, deliberately:
//!
//! * The event loop owns the window and the GPU, because both platforms
//!   require it and neither can be driven from anywhere else.
//! * A network task owns the transport and the decoder, so a slow frame
//!   cannot stall the window and a resize cannot stall the network.
//! * Between them, a single-slot mailbox holding the newest picture.
//!
//! The mailbox is a slot rather than a queue on purpose. If the renderer
//! falls behind, the right thing to show is the newest picture, not the
//! oldest one held back by everything queued in front of it. A queue here
//! would convert a brief hiccup into permanent latency.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ndp_proto::{Channel, InputEvent, InputKind, MouseButton, MsgHeader, MsgKind};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use crate::input::{self, Viewport};
use crate::manager::SessionTicket;
use crate::render::Renderer;
use crate::video::{self, Picture};

/// The newest picture, waiting to be drawn.
type Mailbox = Arc<Mutex<Option<Picture>>>;

/// Connect, open a window, and run until the user closes it.
///
/// This takes over the calling thread: both macOS and Windows require the
/// event loop to run on the thread the process started on, so the network
/// side is what gets moved onto a runtime of its own.
pub fn run(ticket: SessionTicket, resource_name: &str) -> anyhow::Result<()> {
    let event_loop = EventLoop::with_user_event().build()?;
    let mailbox: Mailbox = Arc::new(Mutex::new(None));
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
    // Dropping a file on the window is the send gesture. It is deliberately
    // the only one: a session that could reach into this machine's
    // filesystem on the remote side's say-so would be a different product.
    let (drop_tx, drop_rx) = tokio::sync::mpsc::unbounded_channel::<std::path::PathBuf>();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("nebula-net")
        .build()?;

    // Waking the event loop from the network side is what keeps the window
    // redrawing without a spin loop: nothing is drawn until a frame arrives.
    let waker = event_loop.create_proxy();
    let network = runtime.spawn(pump(
        ticket,
        Arc::clone(&mailbox),
        input_rx,
        drop_rx,
        move || {
            let _ = waker.send_event(());
        },
    ));

    let mut app = App {
        title: format!("{resource_name} — NebulaDesk"),
        window: None,
        renderer: None,
        mailbox,
        input: input_tx,
        dropped: drop_tx,
        modifiers: ndp_proto::Modifiers::NONE,
        viewport: Viewport::fit((1.0, 1.0), (1, 1)),
        pointer: None,
    };
    event_loop.run_app(&mut app)?;

    // The window is gone; stop the session rather than leaving the agent
    // capturing a screen nobody is watching.
    network.abort();
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(())
}

/// The window, the GPU, and the input side.
struct App {
    title: String,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    mailbox: Mailbox,
    input: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    dropped: tokio::sync::mpsc::UnboundedSender<std::path::PathBuf>,
    modifiers: ndp_proto::Modifiers,
    viewport: Viewport,
    pointer: Option<(f32, f32)>,
}

impl App {
    /// Send one event, ignoring a closed session: the window will be told
    /// about that separately and there is nothing useful to do here.
    fn send(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    /// Recompute where the picture sits after a resize or a new resolution.
    fn refit(&mut self) {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return;
        };
        let size = window.inner_size();
        if let Some(picture) = renderer.picture_size() {
            self.viewport = Viewport::fit((f64::from(size.width), f64::from(size.height)), picture);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // Resuming happens more than once on mobile-style lifecycles;
            // rebuilding the surface here would throw away a working one.
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(&self.title)
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                tracing::error!(%error, "could not open a window");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        match pollster::block_on(Renderer::new(window.clone(), (size.width, size.height))) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(error) => {
                tracing::error!(%error, "could not set up the GPU");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, _: &ActiveEventLoop, (): ()) {
        // A frame arrived.
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                self.refit();
            }

            WindowEvent::RedrawRequested => {
                // Take rather than clone: whatever is in the slot is the
                // newest picture, and holding it after drawing would only
                // keep a few megabytes alive for nothing.
                let picture = self.mailbox.lock().ok().and_then(|mut slot| slot.take());
                if let (Some(renderer), Some(picture)) = (&mut self.renderer, picture) {
                    renderer.upload(&picture);
                    // The picture's resolution can change mid-session, so
                    // where it sits in the window is recomputed from what was
                    // actually uploaded rather than from what was expected.
                    self.refit();
                }
                if let Some(renderer) = &mut self.renderer {
                    if let Err(error) = renderer.draw() {
                        tracing::warn!(%error, "a frame could not be drawn");
                    }
                }
            }

            WindowEvent::ModifiersChanged(state) => {
                self.modifiers = input::modifiers(state.state());
            }

            WindowEvent::CursorMoved { position, .. } => {
                match self.viewport.normalise(position.x, position.y) {
                    Some(at) => {
                        self.pointer = Some(at);
                        self.send(InputEvent::mouse_move(at.0, at.1, self.modifiers));
                    }
                    None => {
                        // The pointer moved into the letterbox bars. Tell the
                        // agent it left rather than pinning it to an edge.
                        if self.pointer.take().is_some() {
                            let mut event = InputEvent::mouse_move(0.0, 0.0, self.modifiers);
                            event.kind = InputKind::PointerLeave;
                            self.send(event);
                        }
                    }
                }
            }

            WindowEvent::CursorLeft { .. } => {
                if self.pointer.take().is_some() {
                    let mut event = InputEvent::mouse_move(0.0, 0.0, self.modifiers);
                    event.kind = InputKind::PointerLeave;
                    self.send(event);
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // A click with no known position would land wherever the
                // agent's pointer happens to be, which is worse than nothing.
                let (Some(at), Some(button)) = (self.pointer, input::button(button)) else {
                    return;
                };
                let mut event = InputEvent::mouse_move(at.0, at.1, self.modifiers);
                event.kind = match state {
                    ElementState::Pressed => InputKind::MouseDown,
                    ElementState::Released => InputKind::MouseUp,
                };
                event.button = button;
                self.send(event);
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let at = self.pointer.unwrap_or((0.5, 0.5));
                self.send(input::scroll(delta, at, self.modifiers));
            }

            WindowEvent::KeyboardInput { event, .. } => {
                // Auto-repeat is generated locally by the client's OS. The
                // agent's OS will generate its own from the held key, so
                // forwarding these would double the repeat rate.
                if event.repeat {
                    return;
                }
                if let Some(translated) = input::key(
                    event.physical_key,
                    event.state,
                    event.text.as_deref(),
                    self.modifiers,
                ) {
                    self.send(translated);
                }
            }

            WindowEvent::DroppedFile(path) => {
                let _ = self.dropped.send(path);
            }

            _ => {}
        }
    }
}

/// Own the transport: decode what arrives, send what the window produces.
async fn pump(
    ticket: SessionTicket,
    mailbox: Mailbox,
    mut input: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    mut dropped: tokio::sync::mpsc::UnboundedReceiver<std::path::PathBuf>,
    wake: impl Fn() + Send + 'static,
) {
    let connected = match crate::connect_to_agent(&ticket).await {
        Ok(connected) => connected,
        Err(error) => {
            tracing::error!(%error, "could not connect to the resource");
            return;
        }
    };
    let crate::Connected {
        session,
        mut incoming,
        // Bound, not dropped: this is what tells the gateway the client is
        // still here. Discarding it ends the session immediately.
        gateway,
    } = connected;

    let mut decoder = match video::decoder() {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::error!(%error, "no video decoder");
            return;
        }
    };

    // A session without sound is worth having; a session that refuses to
    // start because this machine has no output device is not. So playback
    // failing is a warning, not the end of the connection.
    let mut playback = match crate::audio::Playback::start(2) {
        Ok(playback) => Some(playback),
        Err(error) => {
            tracing::warn!(%error, "no audio output; this session will be silent");
            None
        }
    };

    // The same engine the agent runs, mirrored. Both sides watch and both
    // sides write; what stops that being a loop is in the engine itself.
    let mut clipboard = if ticket.policy.clipboard {
        match nebula_agent::clipboard::SystemClipboard::open() {
            Ok(board) => Some(nebula_agent::clipboard::spawn(board, true)),
            Err(error) => {
                tracing::warn!(%error, "no clipboard on this machine; nothing will be shared");
                None
            }
        }
    } else {
        None
    };

    let mut transfers = ticket
        .policy
        .file_transfer
        .then(|| nebula_agent::files::spawn(nebula_agent::files::default_downloads(), true));

    tracing::info!(session = %ticket.session_id, "connected");
    let mut seq: u32 = 0;
    let mut stats = Stats::new();
    let mut report = tokio::time::interval(Stats::EVERY);
    report.tick().await;

    loop {
        tokio::select! {
            _ = report.tick() => {
                // On a timer rather than per frame: a session receiving
                // nothing at all is exactly the one worth knowing about, and
                // it is the one that would never reach a per-frame report.
                stats.report();
            }

            message = incoming.recv() => {
                let Some(Ok(message)) = message else { break };
                if message.channel == Channel::Clipboard {
                    if let Some(worker) = clipboard.as_ref() {
                        match inbound_clipboard(message.header.kind, &message.payload) {
                            Ok(Some(message)) => worker.deliver(message),
                            Ok(None) => {}
                            Err(error) => {
                                tracing::debug!(%error, "malformed clipboard message");
                            }
                        }
                    }
                    continue;
                }
                if message.channel == Channel::File {
                    if let Some(worker) = transfers.as_ref() {
                        match inbound_file(message.header.kind, &message.payload) {
                            Ok(Some(message)) => worker.deliver(message),
                            Ok(None) => {}
                            Err(error) => {
                                tracing::debug!(%error, "malformed file transfer message");
                            }
                        }
                    }
                    continue;
                }
                if message.channel == Channel::Audio {
                    if let Some(playback) = playback.as_mut() {
                        if let Err(error) = playback.play(&message.payload) {
                            // One bad packet is a glitch; a stream this
                            // client cannot play at all is a configuration
                            // problem, and both are better heard about than
                            // silently absent.
                            tracing::debug!(%error, "an audio packet could not be played");
                        }
                    }
                    continue;
                }
                if message.channel != Channel::Video {
                    continue;
                }
                stats.frame(message.payload.len());
                match decoder.decode(&message.payload) {
                    Ok(Some(picture)) => {
                        stats.decoded(picture.width, picture.height);
                        if let Ok(mut slot) = mailbox.lock() {
                            *slot = Some(picture);
                        }
                        wake();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        // One bad frame is normal after a loss. Ask for a
                        // keyframe and carry on rather than ending a session
                        // that is otherwise healthy.
                        tracing::debug!(%error, "a frame could not be decoded; asking for a keyframe");
                        let header = MsgHeader::new(MsgKind::CapsUpdate, seq, 0);
                        seq = seq.wrapping_add(1);
                        if session.send(Channel::Control, header, b"").await.is_err() {
                            break;
                        }
                    }
                }
            }

            action = async {
                match clipboard.as_mut() {
                    Some(worker) => worker.outbound.recv().await,
                    // Nothing to wait on; returning would spin this arm as
                    // fast as the runtime allows.
                    None => std::future::pending().await,
                }
            } => {
                let Some(action) = action else {
                    clipboard = None;
                    continue;
                };
                if !send_clipboard(&session, &action, &mut seq).await {
                    break;
                }
            }

            action = async {
                match transfers.as_mut() {
                    Some(worker) => worker.outbound.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(action) = action else {
                    transfers = None;
                    continue;
                };
                if !send_file_action(&session, &action, &mut seq).await {
                    break;
                }
            }

            path = dropped.recv() => {
                let Some(path) = path else { break };
                match transfers.as_ref() {
                    Some(worker) => {
                        worker.deliver(nebula_agent::files::Inbound::Send(path));
                    }
                    None => tracing::info!(
                        path = %path.display(),
                        "ignoring a dropped file: this session may not transfer files"
                    ),
                }
            }

            event = input.recv() => {
                let Some(event) = event else { break };
                // Drain whatever else is waiting and send it as one batch.
                // Pointer moves in particular arrive far faster than they
                // need to be delivered, and one message per move would spend
                // more on framing than on the events.
                let mut batch = vec![event];
                while let Ok(next) = input.try_recv() {
                    batch.push(next);
                    if batch.len() >= 64 {
                        break;
                    }
                }
                let batch = coalesce(batch);
                let header = MsgHeader::new(MsgKind::InputBatch, seq, 0);
                seq = seq.wrapping_add(1);
                if session
                    .send(Channel::Input, header, &InputEvent::encode_batch(&batch))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    tracing::info!("the session ended");
    let _ = session
        .send(Channel::Control, MsgHeader::new(MsgKind::Bye, seq, 0), b"")
        .await;
    session.close(0, b"closed by the user");
    // Only now: while this is held the gateway believes the client is still
    // here, and saying goodbye properly matters more than releasing it early.
    drop(gateway);
}

/// A periodic account of what is actually arriving.
///
/// Without it, "the window is black" and "no frames are being sent" and "the
/// frames arrive but do not decode" all look identical from the outside, and
/// they need completely different fixes.
struct Stats {
    since: Instant,
    frames: u32,
    decoded: u32,
    bytes: usize,
    size: (u32, u32),
}

impl Stats {
    /// How often the session reports what it is receiving.
    pub const EVERY: Duration = Duration::from_secs(5);

    fn new() -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            decoded: 0,
            bytes: 0,
            size: (0, 0),
        }
    }

    fn frame(&mut self, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes;
    }

    fn decoded(&mut self, width: u32, height: u32) {
        self.decoded += 1;
        self.size = (width, height);
    }

    fn report(&mut self) {
        let seconds = self.since.elapsed().as_secs_f64();
        tracing::info!(
            fps = format!("{:.1}", f64::from(self.decoded) / seconds),
            kbps = format!("{:.0}", (self.bytes as f64 * 8.0 / 1000.0) / seconds),
            width = self.size.0,
            height = self.size.1,
            dropped = self.frames - self.decoded,
            "video"
        );
        *self = Self::new();
    }
}

/// Drop pointer moves that a later move in the same batch supersedes.
///
/// Only the last position in a batch is observable; the ones before it were
/// never rendered anywhere. Buttons and keys are never dropped, because their
/// order relative to the moves is exactly what makes a drag a drag.
fn coalesce(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for event in events {
        let redundant = event.kind == InputKind::MouseMove
            && event.button == MouseButton::None
            && out.last().is_some_and(|last| {
                last.kind == InputKind::MouseMove && last.button == MouseButton::None
            });
        if redundant {
            out.pop();
        }
        out.push(event);
    }
    out
}

/// Put one clipboard action on the wire.
async fn send_clipboard(
    session: &ndp_transport::Session,
    action: &nebula_agent::clipboard::Action,
    seq: &mut u32,
) -> bool {
    use nebula_agent::clipboard::Action;

    let (kind, payload) = match action {
        Action::Offer(offer) => (
            MsgKind::ClipboardOffer,
            serde_json::to_vec(offer).unwrap_or_default(),
        ),
        Action::Request(request) => (
            MsgKind::ClipboardRequest,
            serde_json::to_vec(request).unwrap_or_default(),
        ),
        Action::Data(header, bytes) => (MsgKind::ClipboardData, header.payload(bytes)),
    };
    let header = MsgHeader::new(kind, *seq, 0);
    *seq = seq.wrapping_add(1);
    session
        .send(Channel::Clipboard, header, &payload)
        .await
        .is_ok()
}

/// Put one file transfer action on the wire.
async fn send_file_action(
    session: &ndp_transport::Session,
    action: &nebula_agent::files::Action,
    seq: &mut u32,
) -> bool {
    use nebula_agent::files::Action;
    let (kind, payload) = match action {
        Action::Offer(offer) => (
            MsgKind::FileOffer,
            serde_json::to_vec(offer).unwrap_or_default(),
        ),
        Action::Chunk(header, data) => {
            let mut payload = Vec::with_capacity(data.len() + ndp_proto::FILE_CHUNK_HEADER_LEN);
            payload.extend_from_slice(&header.to_bytes());
            payload.extend_from_slice(data);
            (MsgKind::FileChunk, payload)
        }
        Action::Ack(ack) => (MsgKind::FileAck, ack.to_bytes().to_vec()),
    };
    *seq = seq.wrapping_add(1);
    session
        .send(Channel::File, MsgHeader::new(kind, *seq, 0), &payload)
        .await
        .is_ok()
}

/// Parse one file transfer message from the agent.
fn inbound_file(
    kind: MsgKind,
    payload: &[u8],
) -> anyhow::Result<Option<nebula_agent::files::Inbound>> {
    use nebula_agent::files::Inbound;
    Ok(match kind {
        MsgKind::FileOffer => Some(Inbound::Offer(serde_json::from_slice(payload)?)),
        MsgKind::FileChunk => {
            let (header, data) = ndp_proto::FileChunkHeader::split(payload)?;
            Some(Inbound::Chunk(header, data.to_vec()))
        }
        MsgKind::FileAck => Some(Inbound::Ack(ndp_proto::FileAck::decode(payload)?)),
        other => {
            tracing::debug!(
                kind = ?other,
                "ignoring an unexpected message on the file channel"
            );
            None
        }
    })
}

/// Parse one clipboard message from the agent.
fn inbound_clipboard(
    kind: MsgKind,
    payload: &[u8],
) -> anyhow::Result<Option<nebula_agent::clipboard::Inbound>> {
    use nebula_agent::clipboard::Inbound;

    Ok(match kind {
        MsgKind::ClipboardOffer => Some(Inbound::Offer(serde_json::from_slice(payload)?)),
        MsgKind::ClipboardRequest => Some(Inbound::Request(serde_json::from_slice(payload)?)),
        MsgKind::ClipboardData => {
            let (header, bytes) = ndp_proto::ClipboardDataHeader::split(payload)?;
            Some(Inbound::Data(header, bytes.to_vec()))
        }
        other => {
            tracing::debug!(
                ?other,
                "ignoring an unexpected message on the clipboard channel"
            );
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndp_proto::Modifiers;

    fn moved(x: f32, y: f32) -> InputEvent {
        InputEvent::mouse_move(x, y, Modifiers::NONE)
    }

    fn clicked(kind: InputKind) -> InputEvent {
        let mut event = InputEvent::mouse_move(0.5, 0.5, Modifiers::NONE);
        event.kind = kind;
        event.button = MouseButton::Left;
        event
    }

    #[test]
    fn only_the_last_of_a_run_of_moves_survives() {
        let batch = coalesce(vec![moved(0.1, 0.1), moved(0.2, 0.2), moved(0.3, 0.3)]);
        assert_eq!(batch.len(), 1);
        assert!((batch[0].x - 0.3).abs() < 0.001);
    }

    #[test]
    fn a_drag_keeps_every_position_between_press_and_release() {
        // Collapsing across the press would turn a drag into a click
        // somewhere else entirely, which is the bug this guards.
        let batch = coalesce(vec![
            moved(0.1, 0.1),
            clicked(InputKind::MouseDown),
            moved(0.5, 0.5),
            clicked(InputKind::MouseUp),
        ]);
        assert_eq!(batch.len(), 4);
        assert_eq!(batch[1].kind, InputKind::MouseDown);
        assert_eq!(batch[3].kind, InputKind::MouseUp);
    }

    #[test]
    fn keys_and_scrolls_are_never_dropped() {
        let key = InputEvent::key(InputKind::KeyDown, ndp_proto::KeyCode::A, Modifiers::NONE);
        let batch = coalesce(vec![moved(0.1, 0.1), key, moved(0.2, 0.2), key]);
        assert_eq!(batch.len(), 4);
    }

    #[test]
    fn an_empty_batch_stays_empty() {
        assert!(coalesce(Vec::new()).is_empty());
    }
}
