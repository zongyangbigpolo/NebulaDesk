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

use ndp_proto::{Channel, InputEvent, InputKind, MouseButton, MsgFlags, MsgHeader, MsgKind};
use nebula_desktop_protocol::{Command, ConnectionPath, Event, SessionState};
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

enum SessionEvent {
    Frame,
    Path(ndp_transport::PathKind),
    Ended,
    Commands,
    Telemetry(Event),
    Settings { audio: bool, clipboard: bool },
    Notice(String),
    PickerFinished(Option<Vec<std::path::PathBuf>>),
}

/// Connect, open a window, and run until the user closes it.
///
/// This takes over the calling thread: both macOS and Windows require the
/// event loop to run on the thread the process started on, so the network
/// side is what gets moved onto a runtime of its own.
pub fn run(ticket: SessionTicket, resource_name: &str) -> anyhow::Result<()> {
    run_inner(ticket, resource_name, false)
}

pub fn run_managed(ticket: SessionTicket, resource_name: &str) -> anyhow::Result<()> {
    run_inner(ticket, resource_name, true)
}

fn run_inner(ticket: SessionTicket, resource_name: &str, managed: bool) -> anyhow::Result<()> {
    let event_loop = EventLoop::with_user_event().build()?;
    let mailbox: Mailbox = Arc::new(Mutex::new(None));
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
    // Only local drops/pickers (including the supervising host's picker)
    // authorize file reads; the remote protocol cannot name a local path.
    let (drop_tx, drop_rx) = tokio::sync::mpsc::channel::<std::path::PathBuf>(64);
    let (commands_tx, commands_rx) = std::sync::mpsc::sync_channel(64);
    let (control_tx, control_rx) = tokio::sync::mpsc::channel(64);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let events = managed.then(crate::desktop::Events::start).transpose()?;
    let policy = ticket.policy;
    if managed {
        let proxy = event_loop.create_proxy();
        let stop = stop_tx.clone();
        std::thread::Builder::new()
            .name("nebula-desktop-commands".into())
            .spawn(move || {
                let mut stdin = std::io::stdin().lock();
                loop {
                    match nebula_desktop_protocol::read_message::<Command>(&mut stdin) {
                        Ok(Some(command)) if command.validate().is_ok() => {
                            if matches!(command, Command::Disconnect {}) {
                                let _ = stop.send(true);
                            }
                            if commands_tx.try_send(command).is_err()
                                || proxy.send_event(SessionEvent::Commands).is_err()
                            {
                                break;
                            }
                        }
                        Ok(None) => break,
                        _ => {
                            tracing::warn!("invalid desktop command; ending managed session");
                            let _ = proxy.send_event(SessionEvent::Telemetry(Event::State {
                                state: SessionState::Failed,
                                path: None,
                                error: Some("Invalid desktop control message".into()),
                            }));
                            break;
                        }
                    }
                }
                let _ = stop.send(true);
                let _ = commands_tx.try_send(Command::Disconnect {});
                let _ = proxy.send_event(SessionEvent::Commands);
            })?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("nebula-net")
        .build()?;

    // Waking the event loop from the network side is what keeps the window
    // redrawing without a spin loop: nothing is drawn until a frame arrives.
    let waker = event_loop.create_proxy();
    let network_task = runtime.spawn(pump(
        ticket,
        Arc::clone(&mailbox),
        input_rx,
        drop_rx,
        control_rx,
        stop_rx,
        move |event| {
            let _ = waker.send_event(event);
        },
    ));
    let abort_network = network_task.abort_handle();
    let observer = event_loop.create_proxy();
    let mut network = runtime.spawn(async move {
        if network_task.await.is_err() {
            let _ = observer.send_event(SessionEvent::Telemetry(Event::State {
                state: SessionState::Failed,
                path: None,
                error: Some("Native session task failed".into()),
            }));
            let _ = observer.send_event(SessionEvent::Ended);
        }
    });

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
        cursor: None,
        held: input::HeldInput::default(),
        status: "Connecting",
        chrome: crate::chrome::Chrome::new(resource_name.to_owned(), policy),
        policy,
        control: control_tx,
        stop: stop_tx,
        events,
        closing: false,
        managed,
        proxy: event_loop.create_proxy(),
        picker_open: false,
        fatal: None,
        network_ended: false,
        commands: commands_rx,
        next_ui_tick: Instant::now(),
        closing_deadline: None,
    };
    let result = event_loop.run_app(&mut app);

    // The window is gone; stop the session rather than leaving the agent
    // capturing a screen nobody is watching.
    let _ = app.stop.send(true);
    runtime.block_on(async {
        match tokio::time::timeout(Duration::from_secs(5), &mut network).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => app.fatal = Some(format!("session task failed: {error}")),
            Err(_) => {
                abort_network.abort();
                network.abort();
                app.fatal = Some("session shutdown timed out".into());
            }
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    if let Some(events) = app.events.take() {
        events.finish()?;
    }
    result?;
    if let Some(error) = app.fatal {
        anyhow::bail!(error);
    }
    Ok(())
}

/// The window, the GPU, and the input side.
struct App {
    title: String,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    mailbox: Mailbox,
    input: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    dropped: tokio::sync::mpsc::Sender<std::path::PathBuf>,
    modifiers: ndp_proto::Modifiers,
    viewport: Viewport,
    pointer: Option<(f32, f32)>,
    cursor: Option<(f64, f64)>,
    held: input::HeldInput,
    status: &'static str,
    chrome: crate::chrome::Chrome,
    policy: nebula_common::SessionPolicy,
    control: tokio::sync::mpsc::Sender<Command>,
    stop: tokio::sync::watch::Sender<bool>,
    events: Option<crate::desktop::Events>,
    closing: bool,
    managed: bool,
    proxy: winit::event_loop::EventLoopProxy<SessionEvent>,
    picker_open: bool,
    fatal: Option<String>,
    network_ended: bool,
    commands: std::sync::mpsc::Receiver<Command>,
    next_ui_tick: Instant,
    closing_deadline: Option<Instant>,
}

impl App {
    /// Send one event, ignoring a closed session: the window will be told
    /// about that separately and there is nothing useful to do here.
    fn send(&self, event: InputEvent) {
        if !self.policy.input {
            return;
        }
        tracing::trace!(
            target: "nebula_client::input_trace",
            kind = ?event.kind,
            x = event.x,
            y = event.y,
            hid = event.key.0,
            button = ?event.button,
            modifiers = event.modifiers.0,
            "forwarding input"
        );
        let _ = self.input.send(event);
    }

    fn release_input(&mut self) {
        for event in self.held.release_all() {
            self.send(event);
        }
        self.modifiers = ndp_proto::Modifiers::NONE;
        self.pointer = None;
    }

    fn leave_pointer(&mut self) {
        for event in self.held.release_buttons(self.modifiers) {
            self.send(event);
        }
        if self.pointer.take().is_some() {
            let mut event = InputEvent::mouse_move(0.0, 0.0, self.modifiers);
            event.kind = InputKind::PointerLeave;
            self.send(event);
        }
    }

    /// Recompute where the picture sits after a resize or a new resolution.
    fn refit(&mut self) {
        let (Some(window), Some(renderer)) = (&self.window, &self.renderer) else {
            return;
        };
        let size = window.inner_size();
        if let Some(picture) = renderer.picture_size() {
            self.viewport =
                self.chrome
                    .viewport((size.width, size.height), window.scale_factor(), picture);
        }
        if let Some((x, y)) = self.cursor {
            let at = (!self.chrome.blocks_pointer(x, y, window.scale_factor()))
                .then(|| self.viewport.normalise(x, y))
                .flatten();
            if at.is_none() {
                self.leave_pointer();
            } else {
                self.pointer = at;
            }
        }
    }

    fn emit(&mut self, mut event: Event) {
        if let Event::State { state, error, .. } = &mut event {
            if *state == SessionState::Disconnected && self.fatal.is_some() {
                *state = SessionState::Failed;
                error.clone_from(&self.fatal);
            }
        }
        if let Event::State {
            state: SessionState::Failed,
            error,
            ..
        } = &event
        {
            self.fatal = Some(
                error
                    .clone()
                    .unwrap_or_else(|| "Native session failed".into()),
            );
        }
        self.chrome.event(&event);
        if let Some(events) = &self.events {
            if let Err(error) = events.emit(event) {
                tracing::error!(%error, "desktop supervision failed");
                self.disconnect();
            }
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn disconnect(&mut self) {
        self.release_input();
        self.closing = true;
        self.closing_deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(3));
        let _ = self.stop.send(true);
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Focus {} => {
                if let Some(window) = &self.window {
                    window.set_visible(true);
                    window.set_minimized(false);
                    window.request_user_attention(Some(
                        winit::window::UserAttentionType::Informational,
                    ));
                    window.focus_window();
                }
            }
            Command::Disconnect {} => self.disconnect(),
            Command::SendFiles { paths } => {
                for path in paths {
                    self.send_path(path.into());
                }
            }
            other => {
                if let Err(error) = self.control.try_send(other) {
                    tracing::warn!(%error, "session command unavailable");
                    self.chrome.notice("Session command unavailable".into());
                }
            }
        }
    }

    fn send_path(&mut self, path: std::path::PathBuf) {
        if !self.policy.file_transfer {
            self.chrome.notice("File transfer is not permitted".into());
        } else if self.dropped.try_send(path).is_err() {
            self.chrome
                .notice("File queue is full or session is disconnected".into());
        }
    }

    fn action(&mut self, action: crate::chrome::Action) {
        use crate::chrome::Action;
        match action {
            Action::Audio(enabled) => self.command(Command::SetAudio { enabled }),
            Action::Clipboard(enabled) => self.command(Command::SetClipboard { enabled }),
            Action::Disconnect => self.disconnect(),
            Action::Fullscreen => {
                if let Some(window) = &self.window {
                    window.set_fullscreen(if window.fullscreen().is_some() {
                        None
                    } else {
                        Some(winit::window::Fullscreen::Borderless(None))
                    });
                }
            }
            Action::PickFiles => {
                if self.picker_open || !self.policy.file_transfer {
                    return;
                }
                self.release_input();
                self.picker_open = true;
                let proxy = self.proxy.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("nebula-file-picker".into())
                    .spawn(move || {
                        let files = pollster::block_on(rfd::AsyncFileDialog::new().pick_files())
                            .map(|files| files.into_iter().map(|f| f.path().to_owned()).collect());
                        let _ = proxy.send_event(SessionEvent::PickerFinished(files));
                    })
                {
                    self.picker_open = false;
                    self.chrome.notice(format!("File picker failed: {error}"));
                }
            }
        }
    }
}

impl ApplicationHandler<SessionEvent> for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .closing_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            event_loop.exit();
            return;
        }
        if Instant::now() >= self.next_ui_tick {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            self.next_ui_tick = Instant::now() + Duration::from_secs(1);
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(self.next_ui_tick));
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // Resuming happens more than once on mobile-style lifecycles;
            // rebuilding the surface here would throw away a working one.
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!("{} [{}]", self.title, self.status))
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 820.0))
            .with_min_inner_size(winit::dpi::LogicalSize::new(800.0, 500.0));
        #[cfg(target_os = "macos")]
        let attributes = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attributes
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
        };
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                tracing::error!(%error, "could not open a window");
                self.emit(Event::State {
                    state: SessionState::Failed,
                    path: None,
                    error: Some("Could not open native session window".into()),
                });
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        match pollster::block_on(Renderer::new(window.clone(), (size.width, size.height))) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(error) => {
                tracing::error!(%error, "could not set up the GPU");
                self.emit(Event::State {
                    state: SessionState::Failed,
                    path: None,
                    error: Some("Could not initialize native renderer".into()),
                });
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
        self.emit(Event::State {
            state: SessionState::Connecting,
            path: None,
            error: None,
        });
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: SessionEvent) {
        match event {
            SessionEvent::Frame => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
            SessionEvent::Path(path) => {
                self.status = match path {
                    ndp_transport::PathKind::Relay => "Relay",
                    ndp_transport::PathKind::Direct => "Direct",
                };
                self.emit(Event::State {
                    state: SessionState::Connected,
                    path: Some(match path {
                        ndp_transport::PathKind::Direct => ConnectionPath::Direct,
                        ndp_transport::PathKind::Relay => ConnectionPath::Relay,
                    }),
                    error: None,
                });
            }
            SessionEvent::Ended => {
                self.network_ended = true;
                self.release_input();
                self.status = "Disconnected";
                if self.managed || self.closing {
                    event_loop.exit();
                }
            }
            SessionEvent::Commands => {
                while let Ok(command) = self.commands.try_recv() {
                    self.command(command);
                }
                if *self.stop.borrow() {
                    self.disconnect();
                }
                if self.closing && self.network_ended {
                    event_loop.exit();
                }
            }
            SessionEvent::Telemetry(event) => self.emit(event),
            SessionEvent::Settings { audio, clipboard } => self.chrome.settings(audio, clipboard),
            SessionEvent::Notice(message) => self.chrome.notice(message),
            SessionEvent::PickerFinished(files) => {
                self.picker_open = false;
                if let Some(files) = files {
                    for path in files {
                        self.send_path(path);
                    }
                }
            }
        }
        if let Some(window) = &self.window {
            window.set_title(format!("{} [{}]", self.title, self.status).as_str());
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let consumed = self
            .window
            .as_ref()
            .is_some_and(|window| self.chrome.on_window_event(window, &event));
        if consumed
            && matches!(
                event,
                WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::KeyboardInput { .. }
                    | WindowEvent::Ime(_)
            )
        {
            self.release_input();
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.disconnect();
                if self.network_ended {
                    event_loop.exit();
                }
            }

            WindowEvent::Focused(false) => self.release_input(),

            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                self.refit();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { .. } => self.refit(),

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
                if let (Some(renderer), Some(window)) = (&mut self.renderer, &self.window) {
                    match renderer.draw_chrome(window, &mut self.chrome) {
                        Ok(actions) => {
                            for action in actions {
                                self.action(action);
                            }
                        }
                        Err(error) => tracing::warn!(%error, "a frame could not be drawn"),
                    }
                    self.refit();
                }
            }

            WindowEvent::ModifiersChanged(state) => {
                self.modifiers = input::modifiers(state.state());
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some((position.x, position.y));
                if self.picker_open
                    || self.window.as_ref().is_some_and(|window| {
                        self.chrome
                            .blocks_pointer(position.x, position.y, window.scale_factor())
                    })
                {
                    self.leave_pointer();
                    return;
                }
                tracing::trace!(
                    target: "nebula_client::input_trace",
                    x = position.x,
                    y = position.y,
                    viewport_x = self.viewport.x,
                    viewport_y = self.viewport.y,
                    viewport_width = self.viewport.width,
                    viewport_height = self.viewport.height,
                    "local cursor moved (physical pixels)"
                );
                match self.viewport.normalise(position.x, position.y) {
                    Some(at) => {
                        self.pointer = Some(at);
                        let event = self.held.movement(at, self.modifiers);
                        self.send(event);
                    }
                    None => {
                        // The pointer moved into the letterbox bars. Tell the
                        // agent it left rather than pinning it to an edge.
                        self.leave_pointer();
                    }
                }
            }

            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.leave_pointer();
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // A click with no known position would land wherever the
                // agent's pointer happens to be, which is worse than nothing.
                let Some(button) = input::button(button) else {
                    return;
                };
                if let Some(event) = self
                    .held
                    .button(button, state, self.pointer, self.modifiers)
                {
                    self.send(event);
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(at) = self.pointer {
                    self.send(input::scroll(delta, at, self.modifiers));
                }
            }

            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if self.picker_open || self.chrome.wants_keyboard_input() {
                    return;
                }
                tracing::trace!(
                    target: "nebula_client::input_trace",
                    physical_key = ?event.physical_key,
                    state = ?event.state,
                    repeat = event.repeat,
                    is_synthetic,
                    "local physical key transition"
                );
                // Do not restore keys that were already down when focus returned.
                if is_synthetic && event.state == ElementState::Pressed {
                    return;
                }
                if let Some(translated) = input::key(
                    event.physical_key,
                    event.state,
                    event.text.as_deref(),
                    self.modifiers,
                ) {
                    if let Some(translated) = self.held.keyboard(translated, event.repeat) {
                        self.send(translated);
                    }
                }
            }

            WindowEvent::DroppedFile(path) => {
                self.send_path(path);
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
    mut dropped: tokio::sync::mpsc::Receiver<std::path::PathBuf>,
    mut control: tokio::sync::mpsc::Receiver<Command>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    wake: impl Fn(SessionEvent) + Send + 'static,
) {
    let connected = tokio::select! {
        biased;
        _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => {
            wake(SessionEvent::Telemetry(Event::State { state: SessionState::Disconnected, path: None, error: None }));
            wake(SessionEvent::Ended);
            return;
        }
        result = crate::connect_to_agent(&ticket) => result,
    };
    let connected = match connected {
        Ok(connected) => connected,
        Err(_) => {
            tracing::error!("could not connect to the resource");
            wake(SessionEvent::Telemetry(Event::State {
                state: SessionState::Failed,
                path: None,
                error: Some("Could not establish the encrypted session".into()),
            }));
            wake(SessionEvent::Ended);
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

    let mut order = video::VideoOrder::new();
    let mut decoder = match video::decoder() {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::error!(%error, "no video decoder");
            session.close(1, b"video decoder unavailable");
            drop(gateway);
            wake(SessionEvent::Telemetry(Event::State {
                state: SessionState::Failed,
                path: None,
                error: Some("Native video decoder unavailable".into()),
            }));
            wake(SessionEvent::Ended);
            return;
        }
    };

    // A session without sound is worth having; a session that refuses to
    // start because this machine has no output device is not. So playback
    // failing is a warning, not the end of the connection.
    let mut playback = if ticket.policy.audio {
        match crate::audio::Playback::start(2) {
            Ok(playback) => Some(playback),
            Err(error) => {
                tracing::warn!(%error, "no audio output; this session will be silent");
                wake(SessionEvent::Notice("Audio output is unavailable".into()));
                None
            }
        }
    } else {
        None
    };
    let mut clipboard_enabled = ticket.policy.clipboard;

    // The same engine the agent runs, mirrored. Both sides watch and both
    // sides write; what stops that being a loop is in the engine itself.
    let mut clipboard = if ticket.policy.clipboard || ticket.policy.file_transfer {
        match nebula_agent::clipboard::SystemClipboard::open() {
            Ok(board) => Some(nebula_agent::clipboard::spawn_with_files(
                board,
                ticket.policy.clipboard,
                ticket.policy.file_transfer,
            )),
            Err(error) => {
                tracing::warn!(%error, "no clipboard on this machine; nothing will be shared");
                wake(SessionEvent::Notice(
                    "Local clipboard is unavailable".into(),
                ));
                None
            }
        }
    } else {
        None
    };

    let mut transfers = ticket.policy.file_transfer.then(|| {
        nebula_agent::files::spawn_with_telemetry(nebula_agent::files::default_downloads(), true)
    });
    let mut active_transfers = std::collections::HashMap::<String, Event>::new();
    let mut transfer_updates = transfers.as_mut().map(|worker| {
        std::mem::replace(
            &mut worker.telemetry,
            tokio::sync::mpsc::unbounded_channel().1,
        )
    });
    wake(SessionEvent::Settings {
        audio: playback.is_some(),
        clipboard: clipboard_enabled && clipboard.is_some(),
    });

    tracing::info!(session = %ticket.session_id, "connected");
    let mut seq: u32 = 0;
    let mut stats = Stats::new();
    let mut report = tokio::time::interval(Stats::EVERY);
    report.tick().await;
    let mut path_changes = session.path_changes();
    wake(SessionEvent::Path(
        path_changes
            .as_ref()
            .map(|changes| changes.borrow().kind)
            .unwrap_or(ndp_transport::PathKind::Relay),
    ));

    loop {
        let recovery_at = order.recovery_deadline();
        tokio::select! {
            _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => break,
            command = control.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::SetAudio { enabled } => {
                        // Dropping the stream discards both buffered PCM and decoder history.
                        playback = None;
                        if enabled && ticket.policy.audio {
                            match crate::audio::Playback::start(2) {
                                Ok(output) => playback = Some(output),
                                Err(error) => {
                                    tracing::warn!(%error, "could not start audio playback");
                                    wake(SessionEvent::Notice("Audio output is unavailable".into()));
                                }
                            }
                        } else if enabled {
                            wake(SessionEvent::Notice("Audio is not permitted by this session".into()));
                        }
                    }
                    Command::SetClipboard { enabled } => {
                        clipboard_enabled = enabled && ticket.policy.clipboard;
                        if let Some(worker) = &mut clipboard {
                            worker.set_enabled(clipboard_enabled);
                        }
                        if enabled && (!ticket.policy.clipboard || clipboard.is_none()) {
                            wake(SessionEvent::Notice("Clipboard sharing is unavailable".into()));
                        }
                    }
                    _ => {}
                }
                wake(SessionEvent::Settings {
                    audio: playback.is_some(), clipboard: clipboard_enabled && clipboard.is_some(),
                });
            }
            update = async {
                match transfer_updates.as_mut() {
                    Some(updates) => updates.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(update) = update {
                    let event = transfer_event(update);
                    remember_transfer(&mut active_transfers, &event);
                    wake(SessionEvent::Telemetry(event));
                } else {
                    transfer_updates = None;
                }
            }
            changed = async {
                match path_changes.as_mut() {
                    Some(changes) => changes.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() {
                    path_changes = None;
                    continue;
                }
                if let Some(changes) = &path_changes {
                    let path = *changes.borrow();
                    tracing::info!(session = %ticket.session_id, ?path, "session media path changed");
                    wake(SessionEvent::Path(path.kind));
                    wake(SessionEvent::Telemetry(metrics_event(&session, stats.size)));
                }
                if order.decode_failed().ask_for_keyframe
                    && !request_keyframe(&session, &mut seq).await
                {
                    break;
                }
            }

            _ = async {
                match recovery_at {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            } => {
                if order.recover().ask_for_keyframe
                    && !request_keyframe(&session, &mut seq).await
                {
                    break;
                }
            }

            _ = report.tick() => {
                // On a timer rather than per frame: a session receiving
                // nothing at all is exactly the one worth knowing about, and
                // it is the one that would never reach a per-frame report.
                wake(SessionEvent::Telemetry(metrics_event(&session, stats.size)));
                stats.report();
                tracing::debug!(paths = ?session.path_stats(), "session path counters");
            }

            message = incoming.recv() => {
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        tracing::warn!(%error, "the session transport failed");
                        break;
                    }
                    None => break,
                };
                if message.channel == Channel::Clipboard {
                    if let Some(worker) = clipboard.as_ref().filter(|_| clipboard_enabled) {
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
                let keyframe = message.header.flags.contains(MsgFlags::KEYFRAME);
                let ready = order.accept(message.header.seq, keyframe, message.payload);
                if ready.ask_for_keyframe && !request_keyframe(&session, &mut seq).await {
                    break;
                }
                let ask = decode_frames(decoder.as_mut(), &mut order, ready.frames, |picture| {
                    let resized = stats.size != (picture.width, picture.height);
                    stats.decoded(picture.width, picture.height);
                    if resized {
                        wake(SessionEvent::Telemetry(metrics_event(&session, stats.size)));
                    }
                    if let Ok(mut slot) = mailbox.lock() {
                        *slot = Some(picture);
                    }
                    wake(SessionEvent::Frame);
                });
                if ask && !request_keyframe(&session, &mut seq).await {
                    break;
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
                    wake(SessionEvent::Settings { audio: playback.is_some(), clipboard: false });
                    wake(SessionEvent::Notice("Clipboard worker stopped".into()));
                    continue;
                };
                if let nebula_agent::clipboard::Action::Files(paths) = action {
                    if let Some(worker) = transfers.as_ref() {
                        for path in paths {
                            worker.deliver(nebula_agent::files::Inbound::Send(path));
                        }
                        if !clipboard_enabled { continue; }
                    } else {
                        tracing::warn!("copied files cannot be sent: file transfer is unavailable");
                    }
                    continue;
                }
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
                if !ticket.policy.input { continue; }
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
    if !matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            session.send(Channel::Control, MsgHeader::new(MsgKind::Bye, seq, 0), b"")
        )
        .await,
        Ok(Ok(()))
    ) {
        tracing::debug!("session goodbye could not be delivered");
    }
    session.close(0, b"closed by the user");
    // Only now: while this is held the gateway believes the client is still
    // here, and saying goodbye properly matters more than releasing it early.
    drop(gateway);
    if let Some(updates) = &mut transfer_updates {
        while let Ok(update) = updates.try_recv() {
            let event = transfer_event(update);
            remember_transfer(&mut active_transfers, &event);
            wake(SessionEvent::Telemetry(event));
        }
    }
    for (_, mut event) in active_transfers {
        if let Event::Transfer { state, error, .. } = &mut event {
            *state = nebula_desktop_protocol::TransferState::Failed;
            *error = Some("Session ended before transfer was confirmed".into());
        }
        wake(SessionEvent::Telemetry(event));
    }
    wake(SessionEvent::Telemetry(Event::State {
        state: SessionState::Disconnected,
        path: None,
        error: None,
    }));
    wake(SessionEvent::Ended);
}

fn transfer_event(update: nebula_agent::files::TransferUpdate) -> Event {
    use nebula_agent::files::{TransferDirection as Direction, TransferState as State};
    let direction = match update.direction {
        Direction::Send => nebula_desktop_protocol::TransferDirection::Send,
        Direction::Receive => nebula_desktop_protocol::TransferDirection::Receive,
    };
    Event::Transfer {
        id: format!(
            "{}:{}",
            if update.direction == Direction::Send {
                "send"
            } else {
                "receive"
            },
            update.id
        ),
        name: update.name,
        direction,
        transferred: update.transferred,
        total: update.total,
        state: match update.state {
            State::Offered => nebula_desktop_protocol::TransferState::Offered,
            State::Transferring => nebula_desktop_protocol::TransferState::Transferring,
            State::Complete => nebula_desktop_protocol::TransferState::Complete,
            State::Failed => nebula_desktop_protocol::TransferState::Failed,
        },
        error: update.error,
    }
}

fn metrics_event(session: &ndp_transport::Session, size: (u32, u32)) -> Event {
    let rtt_ms = session
        .path_stats()
        .iter()
        .find(|path| path.active)
        .and_then(|path| path.end_to_end_rtt)
        .map(|rtt| rtt.as_secs_f64() * 1000.0);
    Event::Metrics {
        rtt_ms,
        width: size.0,
        height: size.1,
    }
}

fn remember_transfer(active: &mut std::collections::HashMap<String, Event>, event: &Event) {
    if let Event::Transfer { id, state, .. } = event {
        if matches!(
            state,
            nebula_desktop_protocol::TransferState::Complete
                | nebula_desktop_protocol::TransferState::Failed
        ) {
            active.remove(id);
        } else {
            active.insert(id.clone(), event.clone());
        }
    }
}

async fn request_keyframe(session: &ndp_transport::Session, seq: &mut u32) -> bool {
    let header = MsgHeader::new(MsgKind::CapsUpdate, *seq, 0);
    *seq = seq.wrapping_add(1);
    session.send(Channel::Control, header, b"").await.is_ok()
}

/// Stop the entire ready run at the first decode failure: every following
/// delta may reference it, even if it was already released by the reorderer.
fn decode_frames(
    decoder: &mut dyn video::VideoDecoder,
    order: &mut video::VideoOrder,
    frames: Vec<Vec<u8>>,
    mut display: impl FnMut(Picture),
) -> bool {
    for payload in frames {
        match decoder.decode(&payload) {
            Ok(Some(picture)) => display(picture),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "a frame could not be decoded; waiting for a keyframe");
                return order.decode_failed().ask_for_keyframe;
            }
        }
    }
    false
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
            // A frame held back for reordering is counted as it arrives and
            // decoded in a later interval, so within one interval more can
            // decode than arrived.
            dropped = self.frames.saturating_sub(self.decoded),
            "video"
        );
        let size = self.size;
        *self = Self::new();
        self.size = size;
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
        Action::Files(_) => {
            tracing::error!("local file-copy action incorrectly routed to clipboard wire sender");
            return false;
        }
    };
    let header = MsgHeader::new(kind, *seq, 0);
    *seq = seq.wrapping_add(1);
    let sent = session
        .send(Channel::Clipboard, header, &payload)
        .await
        .is_ok();
    if sent {
        match action {
            Action::Offer(offer) => tracing::debug!(
                offer_id = offer.offer_id,
                format = ?offer.formats,
                bytes = offer.size_hint,
                "clipboard offer sent on Nebula channel"
            ),
            Action::Data(header, bytes) => tracing::debug!(
                offer_id = header.offer_id,
                format = ?header.format,
                bytes = bytes.len(),
                "clipboard data sent on Nebula channel"
            ),
            _ => {}
        }
    }
    sent
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

    async fn pump_before_handshake(stopped: bool) -> Vec<SessionEvent> {
        let ticket = SessionTicket {
            session_id: uuid::Uuid::nil(),
            ticket: "SECRET_BEARER".into(),
            gateway_addr: "localhost:1".into(),
            gateway_pin: String::new(),
            agent_key: "not-hex".into(),
            policy: nebula_common::SessionPolicy::view_only(),
        };
        let (_input, input) = tokio::sync::mpsc::unbounded_channel();
        let (_files, files) = tokio::sync::mpsc::channel(1);
        let (_controls, controls) = tokio::sync::mpsc::channel(1);
        let (_stop, stop) = tokio::sync::watch::channel(stopped);
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&events);
        pump(
            ticket,
            Arc::new(Mutex::new(None)),
            input,
            files,
            controls,
            stop,
            move |event| received.lock().unwrap().push(event),
        )
        .await;
        Arc::try_unwrap(events).ok().unwrap().into_inner().unwrap()
    }

    #[tokio::test]
    async fn host_eof_cancels_before_handshake_without_connecting() {
        let events = pump_before_handshake(true).await;
        assert!(matches!(
            events.as_slice(),
            [
                SessionEvent::Telemetry(Event::State {
                    state: SessionState::Disconnected,
                    ..
                }),
                SessionEvent::Ended,
            ]
        ));
    }

    #[tokio::test]
    async fn failed_handshake_never_emits_connected_or_the_ticket() {
        let events = pump_before_handshake(false).await;
        assert!(matches!(
            events.as_slice(),
            [
                SessionEvent::Telemetry(Event::State {
                    state: SessionState::Failed,
                    ..
                }),
                SessionEvent::Ended,
            ]
        ));
        for event in events {
            if let SessionEvent::Telemetry(event) = event {
                assert!(!serde_json::to_string(&event)
                    .unwrap()
                    .contains("SECRET_BEARER"));
            }
        }
    }

    #[test]
    fn transfer_mapping_keeps_directions_distinct_and_only_terminal_events_finish() {
        use nebula_agent::files::{TransferDirection, TransferState, TransferUpdate};
        let update = |direction, state| {
            transfer_event(TransferUpdate {
                id: 1,
                name: "file".into(),
                direction,
                transferred: 12,
                total: 12,
                state,
                error: None,
            })
        };
        let mut active = std::collections::HashMap::new();
        remember_transfer(
            &mut active,
            &update(TransferDirection::Send, TransferState::Transferring),
        );
        remember_transfer(
            &mut active,
            &update(TransferDirection::Receive, TransferState::Offered),
        );
        assert_eq!(
            active.len(),
            2,
            "all bytes reported is not a completion event"
        );
        remember_transfer(
            &mut active,
            &update(TransferDirection::Send, TransferState::Complete),
        );
        assert!(active.contains_key("receive:1"));
        assert!(!active.contains_key("send:1"));
    }

    #[test]
    fn decoder_failure_stops_the_ready_run_and_rate_limits_recovery() {
        struct RejectingDecoder(usize);
        impl video::VideoDecoder for RejectingDecoder {
            fn decode(&mut self, _: &[u8]) -> anyhow::Result<Option<Picture>> {
                self.0 += 1;
                anyhow::bail!("synthetic decoder failure")
            }
        }
        let mut decoder = RejectingDecoder(0);
        let mut order = video::VideoOrder::new();
        order.accept(0, true, vec![0]);
        order.accept(2, false, vec![2]);
        let ready = order.accept(1, false, vec![1]);
        assert_eq!(ready.frames.len(), 2);
        assert!(decode_frames(
            &mut decoder,
            &mut order,
            ready.frames,
            |_| panic!("failed frames must not display"),
        ));
        assert_eq!(decoder.0, 1, "a dependent frame must not reach the decoder");
        let ready = order.accept(3, false, vec![3]);
        assert!(ready.frames.is_empty());
        assert!(!ready.ask_for_keyframe);
        let ready = order.accept(4, true, vec![4]);
        assert!(!decode_frames(
            &mut decoder,
            &mut order,
            ready.frames,
            |_| panic!("failed frames must not display"),
        ));
        assert_eq!(decoder.0, 2);
        assert!(order.recovery_deadline().is_some());
    }

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
