//! Independent local native windows for negotiated remote application surfaces.
//! A surface never shares decoder state, input ownership, or queued frames.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ndp_proto::application::{
    ApplicationFailureReason, ApplicationHostOs, ApplicationMessage as Message, SurfaceFrameInfo,
    SurfaceInfo, SurfaceRegistry, MAX_APPLICATION_SURFACES,
};
use ndp_proto::{Channel, ControlMessage, InputEvent, Modifiers, MsgFlags, MsgHeader};
use nebula_desktop_protocol::{Command, Event, SessionState};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::{input, shortcuts, video, SessionTicket};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_CAPACITY: usize = 256;
const MAX_SURFACE_PIXELS: u64 = 16_777_216;
const MAX_SESSION_PIXELS: u64 = 67_108_864;
type Mailbox = Arc<Mutex<FrameSlot>>;

#[derive(Debug)]
pub(crate) struct ReportedFailure;
impl std::fmt::Display for ReportedFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("native application session failed")
    }
}
impl std::error::Error for ReportedFailure {}

#[derive(Default)]
struct FrameSlot {
    generation: u32,
    retired: bool,
    picture: Option<video::Picture>,
}

impl FrameSlot {
    fn publish(&mut self, generation: u32, picture: video::Picture) -> bool {
        if self.retired || self.generation != generation {
            return false;
        }
        let wake = self.picture.is_none();
        self.picture = Some(picture);
        wake
    }

    fn reset(&mut self, generation: u32, retired: bool) {
        self.generation = generation;
        self.retired = retired;
        self.picture = None;
    }
}

enum Update {
    Upsert(SurfaceInfo, Mailbox),
    Remove(u8),
    Unavailable(u8),
    Host(ApplicationHostOs),
}

struct Shared {
    ended: Mutex<Option<Result<(), &'static str>>>,
    focus_requested: AtomicBool,
    invalid_command: AtomicBool,
}

struct NativeSurface {
    info: SurfaceInfo,
    window: Arc<Window>,
    renderer: crate::render::Renderer,
    mailbox: Mailbox,
    held: input::HeldInput,
    keyboard: shortcuts::Mapper,
    modifiers: Modifiers,
    pointer: Option<(f32, f32)>,
    close_requested: CloseRequest,
    minimized: bool,
    resize: Option<((u32, u32), Instant)>,
    first_frame: bool,
    available: bool,
    focused: bool,
}

#[derive(Default)]
struct CloseRequest(Option<Instant>);

impl CloseRequest {
    fn request(&mut self, now: Instant) -> bool {
        if self
            .0
            .is_some_and(|at| now.duration_since(at) < Duration::from_secs(1))
        {
            return false;
        }
        self.0 = Some(now);
        true
    }

    fn cancel(&mut self) {
        self.0 = None;
    }
}

impl NativeSurface {
    fn input(&self, mut event: InputEvent) -> Message {
        event.display = self.info.surface_id;
        if event.kind.is_pointer() {
            event.modifiers = self.keyboard.modifiers();
        }
        Message::Input {
            surface_id: self.info.surface_id,
            geometry_generation: self.info.geometry_generation,
            event: event.to_bytes(),
        }
    }

    fn release(&mut self) -> Vec<Message> {
        // Pointer releases retain the mapped modifiers until the following key-ups.
        let mut messages: Vec<_> = self
            .held
            .release_all()
            .into_iter()
            .map(|event| self.input(event))
            .collect();
        let keys = self.keyboard.release_all();
        messages.extend(keys.into_iter().map(|event| self.input(event)));
        self.pointer = None;
        messages
    }
}

struct App {
    surfaces: BTreeMap<u8, NativeSurface>,
    updates: std::sync::mpsc::Receiver<Update>,
    outbound: tokio::sync::mpsc::Sender<Message>,
    stop: tokio::sync::watch::Sender<bool>,
    shared: Arc<Shared>,
    events: Option<crate::desktop::Events>,
    input_allowed: bool,
    connected: bool,
    fatal: Option<String>,
    managed: bool,
    host: shortcuts::Platform,
    host_explicit: bool,
    profile: shortcuts::Profile,
    mode: shortcuts::Mode,
    last_focused: Option<u8>,
}

impl App {
    fn clear_surfaces(&mut self) {
        for surface in self.surfaces.values() {
            crate::application_native::set_blocked(&surface.window, false);
            if let Some(parent) = surface
                .info
                .parent_surface_id
                .and_then(|id| self.surfaces.get(&id))
            {
                crate::application_native::detach(&surface.window, &parent.window);
            }
        }
        self.surfaces.clear();
    }

    fn emit(&mut self, event: Event) {
        if self
            .events
            .as_ref()
            .is_some_and(|events| events.emit(event).is_err())
        {
            self.fatal = Some("Desktop event pipe is unavailable".into());
            let _ = self.stop.send(true);
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, message: &str) {
        self.fatal = Some(message.to_owned());
        self.emit(Event::State {
            state: SessionState::Failed,
            path: None,
            error: Some(message.into()),
        });
        let _ = self.stop.send(true);
        self.clear_surfaces();
        if !self.managed {
            rfd::MessageDialog::new()
                .set_title("Application disconnected")
                .set_description(message)
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
        event_loop.exit();
    }

    fn send(&mut self, event_loop: &ActiveEventLoop, message: Message) {
        if !self.input_allowed && !matches!(message, Message::RequestKeyframe { .. }) {
            return;
        }
        if self.outbound.try_send(message).is_err() {
            // Never silently drop a key release/close. Disconnect releases remote ownership.
            self.fail(
                event_loop,
                "Application input is no longer responsive. Reconnect to continue.",
            );
        }
    }

    fn blocked_by_modal(&self, id: u8) -> Option<u8> {
        self.surfaces.iter().find_map(|(&child_id, child)| {
            if !child.info.modal {
                return None;
            }
            let mut parent = child.info.parent_surface_id;
            for _ in 0..MAX_APPLICATION_SURFACES {
                let p = parent?;
                if p == id {
                    return Some(child_id);
                }
                parent = self.surfaces.get(&p)?.info.parent_surface_id;
            }
            None
        })
    }

    fn apply(&mut self, event_loop: &ActiveEventLoop, update: Update) -> anyhow::Result<()> {
        match update {
            Update::Host(host) => {
                if !self.host_explicit {
                    self.host = match host {
                        ApplicationHostOs::Macos => shortcuts::Platform::Mac,
                        ApplicationHostOs::Windows => shortcuts::Platform::Windows,
                        ApplicationHostOs::Linux => shortcuts::Platform::Linux,
                        ApplicationHostOs::Unknown => shortcuts::Platform::local(),
                    };
                }
            }
            Update::Unavailable(id) => {
                if let Some(surface) = self.surfaces.get_mut(&id) {
                    for message in surface.release() {
                        self.outbound
                            .try_send(message)
                            .map_err(|_| anyhow::anyhow!("input queue full"))?;
                    }
                    surface.available = false;
                    surface.renderer.clear_picture();
                    surface.window.set_title(&format!(
                        "{} — window temporarily unavailable",
                        surface.info.title
                    ));
                    surface.window.set_visible(true);
                    surface.window.request_redraw();
                }
            }
            Update::Remove(id) => {
                if let Some(surface) = self.surfaces.remove(&id) {
                    if let Some(parent) = surface
                        .info
                        .parent_surface_id
                        .and_then(|id| self.surfaces.get(&id))
                    {
                        crate::application_native::detach(&surface.window, &parent.window);
                        parent.window.focus_window();
                    }
                }
                for (&id, surface) in &self.surfaces {
                    crate::application_native::set_blocked(
                        &surface.window,
                        self.blocked_by_modal(id).is_some(),
                    );
                }
            }
            Update::Upsert(info, mailbox) => {
                if let Some(previous) = self.surfaces.get(&info.surface_id) {
                    if previous.info.parent_surface_id != info.parent_surface_id {
                        if let Some(parent) = previous
                            .info
                            .parent_surface_id
                            .and_then(|id| self.surfaces.get(&id))
                        {
                            crate::application_native::detach(&previous.window, &parent.window);
                        }
                        if let Some(parent) =
                            info.parent_surface_id.and_then(|id| self.surfaces.get(&id))
                        {
                            crate::application_native::attach(&previous.window, &parent.window)?;
                        }
                    }
                }
                if let Some(surface) = self.surfaces.get_mut(&info.surface_id) {
                    if surface.info.geometry_generation != info.geometry_generation {
                        // Releases use the new accepted epoch; old queued commands are discarded.
                        surface.info.geometry_generation = info.geometry_generation;
                        for message in surface.release() {
                            self.outbound
                                .try_send(message)
                                .map_err(|_| anyhow::anyhow!("input queue full"))?;
                        }
                        surface.renderer.clear_picture();
                    }
                    if surface.info.width != info.width
                        || surface.info.height != info.height
                        || surface.info.scale != info.scale
                    {
                        let _ = surface
                            .window
                            .request_inner_size(winit::dpi::LogicalSize::new(
                                info.width as f64 / info.scale as f64,
                                info.height as f64 / info.scale as f64,
                            ));
                    }
                    surface.window.set_title(&info.title);
                    if surface.minimized != info.minimized {
                        surface.window.set_minimized(info.minimized);
                    }
                    surface.minimized = info.minimized;
                    surface.info = info;
                    surface.available = true;
                    // Metadata updates can follow a cancelled native close/save sheet.
                    surface.close_requested.cancel();
                    surface.window.request_redraw();
                } else {
                    let attributes = Window::default_attributes()
                        .with_title(&info.title)
                        .with_inner_size(winit::dpi::LogicalSize::new(
                            info.width as f64 / info.scale as f64,
                            info.height as f64 / info.scale as f64,
                        ))
                        .with_visible(false);
                    let parent = info
                        .parent_surface_id
                        .and_then(|id| self.surfaces.get(&id))
                        .map(|s| &*s.window);
                    let attributes = crate::application_native::attributes(attributes, parent)?;
                    let window = Arc::new(event_loop.create_window(attributes)?);
                    if let Some(parent) = parent {
                        crate::application_native::attach(&window, parent)?;
                    }
                    window.set_ime_allowed(false);
                    let size = window.inner_size();
                    let renderer = pollster::block_on(crate::render::Renderer::new(
                        window.clone(),
                        (size.width, size.height),
                    ))?;
                    let id = info.surface_id;
                    self.surfaces.insert(
                        id,
                        NativeSurface {
                            minimized: info.minimized,
                            info,
                            window,
                            renderer,
                            mailbox,
                            held: input::HeldInput::default(),
                            keyboard: shortcuts::Mapper::new(
                                shortcuts::Platform::local(),
                                self.host,
                                self.mode,
                                self.profile,
                            ),
                            modifiers: Modifiers::NONE,
                            pointer: None,
                            close_requested: CloseRequest::default(),
                            resize: None,
                            first_frame: false,
                            available: true,
                            focused: false,
                        },
                    );
                }
                // Input gating supplements native ownership, including on Wayland.
                let blocked: Vec<_> = self
                    .surfaces
                    .keys()
                    .copied()
                    .filter(|id| self.blocked_by_modal(*id).is_some())
                    .collect();
                for (&id, surface) in &self.surfaces {
                    crate::application_native::set_blocked(&surface.window, blocked.contains(&id));
                }
                for id in blocked {
                    for message in self.surfaces.get_mut(&id).unwrap().release() {
                        self.outbound
                            .try_send(message)
                            .map_err(|_| anyhow::anyhow!("input queue full"))?;
                    }
                }
            }
        }
        Ok(())
    }

    fn drain(&mut self, event_loop: &ActiveEventLoop) {
        if self.shared.invalid_command.swap(false, Ordering::AcqRel) {
            self.fail(event_loop, "Invalid desktop control message");
            return;
        }
        let ended = self
            .shared
            .ended
            .lock()
            .ok()
            .and_then(|mut result| result.take());
        if let Some(result) = ended {
            match result {
                Ok(()) => {
                    self.emit(Event::State {
                        state: SessionState::Disconnected,
                        path: None,
                        error: None,
                    });
                    self.clear_surfaces();
                    event_loop.exit();
                }
                Err(message) => self.fail(event_loop, message),
            }
            return;
        }
        while let Ok(update) = self.updates.try_recv() {
            if self.apply(event_loop, update).is_err() {
                self.fail(
                    event_loop,
                    "Could not create the application's native window.",
                );
                return;
            }
        }
        if self.shared.focus_requested.swap(false, Ordering::AcqRel) {
            let target = self
                .last_focused
                .filter(|id| self.surfaces.contains_key(id))
                .or_else(|| self.surfaces.keys().next().copied());
            if let Some(id) = target {
                let id = self.blocked_by_modal(id).unwrap_or(id);
                self.surfaces[&id].window.set_minimized(false);
                self.surfaces[&id].window.focus_window();
            }
        }
        let mut connected_now = false;
        for surface in self.surfaces.values_mut() {
            let picture = surface.mailbox.lock().ok().and_then(|mut slot| {
                if slot.retired || slot.generation != surface.info.geometry_generation {
                    None
                } else {
                    slot.picture.take()
                }
            });
            if let Some(picture) = picture {
                surface.renderer.upload(&picture);
                if !surface.first_frame {
                    surface.first_frame = true;
                    surface.window.set_visible(true);
                    surface.window.set_minimized(surface.info.minimized);
                    if surface.info.modal {
                        surface.window.focus_window();
                    }
                }
                surface.window.request_redraw();
                connected_now = true;
            }
        }
        if connected_now && !self.connected {
            self.connected = true;
            self.emit(Event::State {
                state: SessionState::Connected,
                path: None,
                error: None,
            });
        }
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.drain(event_loop);
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        self.drain(event_loop);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.drain(event_loop);
        let mut messages = Vec::new();
        for surface in self.surfaces.values_mut() {
            if let Some(((width, height), at)) = surface.resize {
                if at.elapsed() >= Duration::from_millis(100) {
                    surface.resize = None;
                    if (width, height) != (surface.info.width, surface.info.height) {
                        messages.push(Message::Resize {
                            surface_id: surface.info.surface_id,
                            geometry_generation: surface.info.geometry_generation,
                            width,
                            height,
                        });
                    }
                }
            }
            if let Some(minimized) = surface.window.is_minimized() {
                if minimized && !surface.minimized {
                    messages.extend(surface.release());
                    messages.push(Message::Minimize {
                        surface_id: surface.info.surface_id,
                        geometry_generation: surface.info.geometry_generation,
                    });
                }
                surface.minimized = minimized;
            }
        }
        for message in messages {
            self.send(event_loop, message);
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(100),
        ));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(id) = self
            .surfaces
            .iter()
            .find_map(|(&id, s)| (s.window.id() == window_id).then_some(id))
        else {
            return;
        };
        if !self.input_allowed && matches!(event, WindowEvent::CloseRequested) {
            // A viewer may end its connection, but cannot close a remote document.
            let _ = self.stop.send(true);
            return;
        }
        if let Some(modal) = self.blocked_by_modal(id) {
            if matches!(
                event,
                WindowEvent::Focused(true)
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::KeyboardInput { .. }
                    | WindowEvent::CloseRequested
            ) {
                self.surfaces[&modal].window.focus_window();
                return;
            }
            if matches!(
                event,
                WindowEvent::CursorMoved { .. } | WindowEvent::MouseWheel { .. }
            ) {
                return;
            }
        }
        if matches!(event, WindowEvent::Focused(true)) {
            self.last_focused = Some(id);
        }
        let surface = self.surfaces.get_mut(&id).unwrap();
        if !surface.available
            && matches!(
                event,
                WindowEvent::KeyboardInput { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::CursorMoved { .. }
            )
        {
            return;
        }
        let generation = surface.info.geometry_generation;
        let mut messages = Vec::new();
        if surface.focused
            && matches!(
                event,
                WindowEvent::CursorMoved { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
            )
        {
            let mut restored = surface.keyboard.observe_modifiers(surface.modifiers);
            restored.extend(surface.keyboard.restore_modifiers());
            for restored in restored {
                messages.push(surface.input(restored));
            }
        }
        match event {
            WindowEvent::CloseRequested => {
                // Never destroy locally: the remote application may show Save or cancel.
                if surface.close_requested.request(Instant::now()) {
                    messages.extend(surface.release());
                    messages.push(Message::Close {
                        surface_id: id,
                        geometry_generation: generation,
                    });
                }
            }
            WindowEvent::Focused(focused) => {
                surface.focused = focused;
                if focused {
                    surface.close_requested.cancel();
                    messages.push(Message::Focus {
                        surface_id: id,
                        geometry_generation: generation,
                    });
                    for restored in surface.keyboard.observe_modifiers(surface.modifiers) {
                        messages.push(surface.input(restored));
                    }
                } else {
                    messages.extend(surface.release());
                    surface.modifiers = Modifiers::NONE;
                }
            }
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                surface.renderer.resize(size.width, size.height);
                let scale = surface.info.scale as f64 / surface.window.scale_factor();
                surface.resize = Some((
                    (
                        (size.width as f64 * scale).round() as u32,
                        (size.height as f64 * scale).round() as u32,
                    ),
                    Instant::now(),
                ));
                surface.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                let stale = surface
                    .mailbox
                    .lock()
                    .map(|slot| slot.retired || slot.generation != generation)
                    .unwrap_or(true);
                if !stale && surface.renderer.draw().is_err() {
                    self.fail(event_loop, "The application window could not be rendered.");
                    return;
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                surface.modifiers = input::modifiers(mods.state());
                if surface.focused && surface.available {
                    for changed in surface.keyboard.observe_modifiers(surface.modifiers) {
                        messages.push(surface.input(changed));
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let size = surface.window.inner_size();
                let viewport = input::Viewport::fit(
                    (size.width as f64, size.height as f64),
                    surface
                        .renderer
                        .picture_size()
                        .unwrap_or((surface.info.width, surface.info.height)),
                );
                surface.pointer = viewport.normalise(position.x, position.y);
                if let Some(at) = surface.pointer {
                    let event = surface.held.movement(at, surface.modifiers);
                    messages.push(surface.input(event));
                }
            }
            WindowEvent::CursorLeft { .. } => {
                surface.pointer = None;
                for event in surface.held.release_buttons(surface.modifiers) {
                    messages.push(surface.input(event));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = input::button(button) {
                    if let Some(event) =
                        surface
                            .held
                            .button(button, state, surface.pointer, surface.modifiers)
                    {
                        messages.push(surface.input(event));
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(at) = surface.pointer {
                    messages.push(surface.input(input::scroll(delta, at, surface.modifiers)));
                }
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if let Some(event_input) =
                    input::key(event.physical_key, event.state, None, surface.modifiers)
                {
                    let modifier = shortcuts::modifier(event_input.key.0);
                    if !surface.focused {
                        if modifier != Modifiers::NONE {
                            if event.state == ElementState::Pressed {
                                surface.modifiers.0 |= modifier.0;
                            } else {
                                surface.modifiers.0 &= !modifier.0;
                            }
                        }
                        return;
                    }
                    for changed in surface.keyboard.observe_modifiers(surface.modifiers) {
                        messages.push(surface.input(changed));
                    }
                    let mapped = if is_synthetic {
                        surface.keyboard.synthetic_key(event_input)
                    } else {
                        surface.keyboard.key(event_input, event.repeat)
                    };
                    if modifier != Modifiers::NONE {
                        surface.modifiers = surface.keyboard.source_modifiers();
                    }
                    for mapped in mapped {
                        messages.push(surface.input(mapped));
                    }
                }
            }
            _ => {}
        }
        for message in messages {
            self.send(event_loop, message);
        }
    }
}

/// All windows live on the process main thread; transport/decoders use the runtime.
pub fn run(ticket: SessionTicket, managed: bool) -> anyhow::Result<()> {
    run_with_keyboard(ticket, managed, shortcuts::Configuration::default())
}

pub fn run_with_keyboard(
    ticket: SessionTicket,
    managed: bool,
    keyboard: shortcuts::Configuration,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        ticket.application_windows,
        "Application session mode was not confirmed"
    );
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    let (outbound, receiver) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
    let (updates_tx, updates) = std::sync::mpsc::sync_channel(64);
    let (stop, stop_rx) = tokio::sync::watch::channel(false);
    let shared = Arc::new(Shared {
        ended: Mutex::new(None),
        focus_requested: AtomicBool::new(false),
        invalid_command: AtomicBool::new(false),
    });
    let events = managed.then(crate::desktop::Events::start).transpose()?;
    if managed {
        let stop = stop.clone();
        let shared = shared.clone();
        let proxy = event_loop.create_proxy();
        std::thread::Builder::new()
            .name("nebula-app-commands".into())
            .spawn(move || {
                let mut stdin = std::io::stdin().lock();
                loop {
                    match nebula_desktop_protocol::read_message::<Command>(&mut stdin) {
                        Ok(Some(Command::Disconnect {})) | Ok(None) => break,
                        Ok(Some(Command::Focus {})) => {
                            shared.focus_requested.store(true, Ordering::Release);
                            let _ = proxy.send_event(());
                        }
                        Ok(Some(command)) if command.validate().is_ok() => {}
                        _ => {
                            shared.invalid_command.store(true, Ordering::Release);
                            let _ = proxy.send_event(());
                            break;
                        }
                    }
                }
                let _ = stop.send(true);
            })?;
    }
    let mut app = App {
        surfaces: BTreeMap::new(),
        updates,
        outbound,
        stop,
        shared: shared.clone(),
        events,
        input_allowed: ticket.policy.input,
        connected: false,
        fatal: None,
        managed,
        host: keyboard.host.unwrap_or_else(shortcuts::Platform::local),
        host_explicit: keyboard.host.is_some(),
        profile: keyboard.profile,
        mode: keyboard.mode,
        last_focused: None,
    };
    app.emit(Event::State {
        state: SessionState::Connecting,
        path: None,
        error: None,
    });
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    let proxy = event_loop.create_proxy();
    let worker_proxy = proxy.clone();
    let worker = runtime.spawn(network(ticket, receiver, stop_rx, updates_tx, worker_proxy));
    let abort_worker = worker.abort_handle();
    let mut network = runtime.spawn(async move {
        let result = worker
            .await
            .unwrap_or(Err("The native application session stopped unexpectedly"));
        if let Ok(mut ended) = shared.ended.lock() {
            *ended = Some(result);
        }
        let _ = proxy.send_event(());
    });
    let result = event_loop.run_app(&mut app);
    let _ = app.stop.send(true);
    runtime.block_on(async {
        if tokio::time::timeout(Duration::from_secs(3), &mut network)
            .await
            .is_err()
        {
            abort_worker.abort();
            network.abort();
        }
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    if let Some(events) = app.events.take() {
        events.finish()?;
    }
    result?;
    if app.fatal.is_some() {
        return Err(ReportedFailure.into());
    }
    Ok(())
}

struct DecodeSurface {
    generation: u32,
    decoder: Box<dyn video::VideoDecoder>,
    order: video::VideoOrder,
    mailbox: Mailbox,
    pixels: u64,
    created: Instant,
    seen_frame: bool,
    available: bool,
}

async fn send_control(
    session: &ndp_transport::Session,
    seq: &mut u32,
    message: ControlMessage,
) -> Result<(), &'static str> {
    let payload = message
        .encode()
        .map_err(|_| "Invalid application control message")?;
    let header = MsgHeader::new(message.kind(), *seq, 0);
    *seq = seq.wrapping_add(1);
    tokio::time::timeout(
        Duration::from_secs(2),
        session.send(Channel::Control, header, &payload),
    )
    .await
    .map_err(|_| "Application connection stopped responding")?
    .map_err(|_| "Application connection was lost")
}

fn validate_ack(
    announced: bool,
    acknowledged: bool,
    caps: &ndp_proto::Caps,
    codec: ndp_proto::VideoCodec,
) -> Result<(), &'static str> {
    if !announced
        || acknowledged
        || !caps
            .features
            .contains(ndp_proto::FeatureFlags::APPLICATION_WINDOWS)
        || codec != ndp_proto::VideoCodec::H264
    {
        return Err(nebula_desktop_protocol::APPLICATION_BACKEND_UNAVAILABLE);
    }
    Ok(())
}

fn remote_failure(started: bool, detail: Option<&str>) -> &'static str {
    match detail {
        Some("shared_desktop_controller_busy") => {
            "Another controlling session is using this host. Disconnect it and try again."
        }
        Some("application_negotiation_required" | "application_backend_unavailable") => {
            nebula_desktop_protocol::APPLICATION_BACKEND_UNAVAILABLE
        }
        _ if !started => nebula_desktop_protocol::APPLICATION_START_FAILED,
        _ => nebula_desktop_protocol::REMOTE_SESSION_ENDED,
    }
}

fn startup_failure(reason: ApplicationFailureReason) -> &'static str {
    match reason {
        ApplicationFailureReason::PermissionDenied => {
            nebula_desktop_protocol::APPLICATION_PERMISSION_REQUIRED
        }
        ApplicationFailureReason::LaunchFailed => nebula_desktop_protocol::APPLICATION_START_FAILED,
        _ => nebula_desktop_protocol::APPLICATION_BACKEND_UNAVAILABLE,
    }
}

fn frame_matches(
    surface: &SurfaceInfo,
    video: ndp_proto::VideoFrameInfo,
    frame: SurfaceFrameInfo,
) -> bool {
    surface.surface_id == video.display
        && surface.geometry_generation == frame.geometry_generation
        && (surface.width, surface.height) == (u32::from(video.width), u32::from(video.height))
        && video.codec == ndp_proto::VideoCodec::H264
}

fn announced_surface(
    registry: Option<&SurfaceRegistry>,
    acknowledged: bool,
    id: u8,
) -> Option<&SurfaceInfo> {
    if !acknowledged {
        return None;
    }
    registry?.get(id)
}

fn order_video(
    order: &mut video::VideoOrder,
    frame: SurfaceFrameInfo,
    keyframe: bool,
    bitstream: &[u8],
) -> video::Ready {
    order.accept(frame.surface_sequence, keyframe, bitstream.to_vec())
}

async fn establish(
    ticket: &SessionTicket,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Option<crate::Connected>, &'static str> {
    tokio::select! {
        biased;
        _ = async { let _ = stop.wait_for(|v| *v).await; } => Ok(None),
        result = tokio::time::timeout(STARTUP_TIMEOUT, crate::connect_to_agent(ticket)) =>
            Ok(Some(result.map_err(|_| "Application connection timed out")?
                .map_err(|_| "Could not establish the encrypted application session")?)),
    }
}

async fn network(
    ticket: SessionTicket,
    mut outbound: tokio::sync::mpsc::Receiver<Message>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    updates: std::sync::mpsc::SyncSender<Update>,
    proxy: EventLoopProxy<()>,
) -> Result<(), &'static str> {
    let Some(connected) = establish(&ticket, &mut stop).await? else {
        return Ok(());
    };
    let crate::Connected {
        session,
        mut incoming,
        gateway: _gateway,
    } = connected;
    let mut seq = 0;
    let caps = ndp_proto::Caps {
        video_codecs: vec![ndp_proto::VideoCodec::H264],
        audio_codecs: vec![],
        displays: vec![ndp_proto::DisplayGeometry {
            width: 1920,
            height: 1080,
            scale: 1.0,
            refresh_hz: 60,
        }],
        audio: Default::default(),
        color: Default::default(),
        max_bitrate_bps: 40_000_000,
        features: ndp_proto::FeatureFlags::APPLICATION_WINDOWS,
    };
    send_control(
        &session,
        &mut seq,
        ControlMessage::Hello {
            caps,
            client_version: env!("CARGO_PKG_VERSION").into(),
            client_os: std::env::consts::OS.into(),
        },
    )
    .await?;
    let mut registry: Option<SurfaceRegistry> = None;
    let mut acknowledged = false;
    let mut surfaces = BTreeMap::<u8, DecodeSurface>::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut decoded = false;
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let result = async {
        loop {
            tokio::select! {
                biased;
                _ = async { let _ = stop.wait_for(|v| *v).await; } => return Ok(()),
                _ = tick.tick() => {
                    if !decoded && Instant::now() >= deadline {
                        return Err(nebula_desktop_protocol::APPLICATION_START_FAILED);
                    }
                    for (&id, surface) in &mut surfaces {
                        if surface.available && !surface.seen_frame && surface.created.elapsed() >= STARTUP_TIMEOUT
                            && registry.as_ref().and_then(|r| r.get(id)).is_some_and(|s| !s.minimized)
                        {
                            return Err("An application window did not produce video. Reconnect to try again.");
                        }
                        if surface.available && surface.order.recovery_deadline().is_some_and(|at| Instant::now() >= at)
                            && surface.order.recover().ask_for_keyframe
                        {
                            send_control(&session, &mut seq, ControlMessage::Application(Message::RequestKeyframe {
                                surface_id: id, geometry_generation: surface.generation })).await?;
                        }
                    }
                }
                command = outbound.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    if !acknowledged { continue; }
                    if registry.as_ref().is_some_and(|r| r.validate_command(&command).is_ok())
                        && (ticket.policy.input || matches!(command, Message::RequestKeyframe { .. }))
                    {
                        send_control(&session, &mut seq, ControlMessage::Application(command)).await?;
                    }
                }
                incoming = incoming.recv() => {
                    let message = incoming.ok_or("Application connection was lost")?
                        .map_err(|_| "Application connection was lost")?;
                    if message.channel == Channel::Control {
                        let control = ControlMessage::decode(&message.payload).map_err(|_| "Invalid application control response")?;
                        if control.kind() != message.header.kind { return Err("Application control header mismatch"); }
                        match control {
                            ControlMessage::Application(Message::StartupFailed { reason }) => return Err(startup_failure(reason)),
                            ControlMessage::Application(Message::SurfaceUnavailable { surface_id, .. }) if acknowledged => {
                                let surface = surfaces.get_mut(&surface_id).ok_or("Unknown unavailable application surface")?;
                                surface.available = false;
                                surface.mailbox.lock().map_err(|_| "Application frame state failed")?.reset(surface.generation, false);
                                updates.try_send(Update::Unavailable(surface_id)).map_err(|_| "Application windows are not responding")?;
                                proxy.send_event(()).map_err(|_| "Application window loop stopped")?;
                            }
                            ControlMessage::Application(hello @ Message::Hello { max_surfaces, host_os, .. }) => {
                                hello.validate().map_err(|_| "Incompatible application protocol")?;
                                if registry.is_some() || acknowledged { return Err("Unexpected application negotiation"); }
                                registry = Some(SurfaceRegistry::new(max_surfaces).map_err(|_| "Invalid application surface limit")?);
                                updates.try_send(Update::Host(host_os)).map_err(|_| "Application windows are not responding")?;
                                proxy.send_event(()).map_err(|_| "Application window loop stopped")?;
                            }
                            ControlMessage::HelloAck { caps, active_codec, .. } => {
                                validate_ack(registry.is_some(), acknowledged, &caps, active_codec)?;
                                acknowledged = true;
                            }
                            ControlMessage::Application(Message::SurfaceUpsert { surface }) if acknowledged => {
                                if u64::from(surface.width) * u64::from(surface.height) > MAX_SURFACE_PIXELS {
                                    return Err("Application window exceeds the supported capture size");
                                }
                                let id = surface.surface_id;
                                let pixels = u64::from(surface.width) * u64::from(surface.height);
                                if surfaces.iter().filter(|(key, _)| **key != id).map(|(_, s)| s.pixels).sum::<u64>()
                                    + pixels > MAX_SESSION_PIXELS
                                { return Err("Application windows exceed the session video memory budget"); }
                                registry.as_mut().unwrap().upsert(surface.clone()).map_err(|_| "Invalid application surface lifecycle")?;
                                let generation = surface.geometry_generation;
                                let decode = if let Some(decode) = surfaces.get_mut(&id) {
                                    if decode.generation != generation || !decode.available {
                                        decode.generation = generation;
                                        decode.seen_frame = false;
                                        decode.created = Instant::now();
                                        decode.decoder = video::decoder().map_err(|_| "Native video decoder unavailable")?;
                                        decode.order = video::VideoOrder::new();
                                        decode.mailbox.lock().map_err(|_| "Application frame state failed")?.reset(generation, false);
                                    }
                                    decode.pixels = pixels;
                                    decode.available = true;
                                    decode
                                } else {
                                    surfaces.entry(id).or_insert(DecodeSurface {
                                        generation, decoder: video::decoder().map_err(|_| "Native video decoder unavailable")?,
                                        order: video::VideoOrder::new(),
                                        mailbox: Arc::new(Mutex::new(FrameSlot { generation, ..Default::default() })),
                                        pixels, created: Instant::now(), seen_frame: false, available: true,
                                    })
                                };
                                updates.try_send(Update::Upsert(surface, decode.mailbox.clone())).map_err(|_| "Application windows are not responding")?;
                                proxy.send_event(()).map_err(|_| "Application window loop stopped")?;
                                send_control(&session, &mut seq, ControlMessage::Application(Message::RequestKeyframe {
                                    surface_id: id, geometry_generation: generation })).await?;
                            }
                            ControlMessage::Application(Message::SurfaceRemove { surface_id }) if acknowledged => {
                                registry.as_mut().unwrap().remove(surface_id).map_err(|_| "Invalid application surface removal")?;
                                if let Some(surface) = surfaces.remove(&surface_id) {
                                    surface.mailbox.lock().map_err(|_| "Application frame state failed")?.reset(surface.generation, true);
                                }
                                updates.try_send(Update::Remove(surface_id)).map_err(|_| "Application windows are not responding")?;
                                proxy.send_event(()).map_err(|_| "Application window loop stopped")?;
                            }
                            ControlMessage::Bye { reason: ndp_proto::ByeReason::UserClosed, .. } => return Ok(()),
                            ControlMessage::Bye { detail, .. } => return Err(remote_failure(decoded, detail.as_deref())),
                            ControlMessage::Ping { echo_us } => send_control(&session, &mut seq, ControlMessage::Pong { echo_us }).await?,
                            ControlMessage::Application(_) => return Err("Unexpected application control message"),
                            _ => {}
                        }
                    } else if message.channel == Channel::Video {
                        // Media streams can beat the reliable HelloAck/lifecycle stream.
                        // Never render speculative surfaces; their accepted upsert asks for a refresh.
                        if !acknowledged { continue; }
                        let (info, bitstream) = ndp_proto::VideoFrameInfo::split(&message.payload).map_err(|_| "Invalid application frame")?;
                        if info.codec != ndp_proto::VideoCodec::H264 { return Err("Application sent an unsupported video codec"); }
                        let (surface_info, bitstream) = SurfaceFrameInfo::split(bitstream).map_err(|_| "Invalid application frame generation")?;
                        let Some(surface) = surfaces.get_mut(&info.display) else { continue; };
                        if !surface.available { continue; }
                        let Some(metadata) = announced_surface(registry.as_ref(), acknowledged, info.display) else { continue; };
                        if !frame_matches(metadata, info, surface_info) { continue; }
                        let ready = order_video(&mut surface.order, surface_info,
                            message.header.flags.contains(MsgFlags::KEYFRAME), bitstream);
                        let mut refresh = ready.ask_for_keyframe;
                        for payload in ready.frames {
                            match surface.decoder.decode(&payload) {
                                Ok(Some(picture)) => {
                                    if (picture.width, picture.height) != (metadata.width, metadata.height) { continue; }
                                    surface.seen_frame = true;
                                    decoded = true;
                                    if surface.mailbox.lock().map_err(|_| "Application frame state failed")?
                                        .publish(surface.generation, picture)
                                    { let _ = proxy.send_event(()); }
                                }
                                Ok(None) => {}
                                Err(_) => { refresh |= surface.order.decode_failed().ask_for_keyframe; break; }
                            }
                        }
                        if refresh {
                            send_control(&session, &mut seq, ControlMessage::Application(Message::RequestKeyframe {
                                surface_id: info.display, geometry_generation: surface.generation })).await?;
                        }
                    }
                }
            }
        }
    }.await;
    for surface in surfaces.values() {
        if let Ok(mut mailbox) = surface.mailbox.lock() {
            mailbox.reset(surface.generation, true);
        }
    }
    let _ = send_control(
        &session,
        &mut seq,
        ControlMessage::Bye {
            reason: ndp_proto::ByeReason::UserClosed,
            detail: None,
        },
    )
    .await;
    session.close(0, b"application session ended");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_surface_cannot_publish_or_retain_a_frame() {
        let mut slot = FrameSlot {
            generation: 1,
            ..Default::default()
        };
        assert!(slot.publish(1, video::Picture::default()));
        assert!(
            !slot.publish(1, video::Picture::default()),
            "mailbox wake is coalesced"
        );
        slot.reset(1, true);
        assert!(slot.picture.is_none());
        assert!(!slot.publish(1, video::Picture::default()));
    }
    #[test]
    fn geometry_transition_discards_stale_pictures() {
        let mut slot = FrameSlot {
            generation: 1,
            ..Default::default()
        };
        slot.publish(1, video::Picture::default());
        slot.reset(2, false);
        assert!(slot.picture.is_none());
        assert!(!slot.publish(1, video::Picture::default()));
        assert!(slot.publish(2, video::Picture::default()));
    }

    #[test]
    fn wire_epoch_rejects_stale_frames_even_at_identical_resolution() {
        let mut live = surface(0);
        live.geometry_generation = 2;
        let info = ndp_proto::VideoFrameInfo {
            display: 0,
            codec: ndp_proto::VideoCodec::H264,
            width: 640,
            height: 480,
            duration_us: 16_667,
        };
        let payload = info.frame_payload(
            &SurfaceFrameInfo {
                geometry_generation: 1,
                surface_sequence: 3,
            }
            .frame_payload(b"codec"),
        );
        let (video, scoped) = ndp_proto::VideoFrameInfo::split(&payload).unwrap();
        let (frame, elementary) = SurfaceFrameInfo::split(scoped).unwrap();
        assert_eq!(elementary, b"codec");
        assert!(!frame_matches(&live, video, frame));
        assert!(frame_matches(
            &live,
            video,
            SurfaceFrameInfo {
                geometry_generation: 2,
                surface_sequence: 4,
            }
        ));
    }

    #[test]
    fn interleaved_surface_streams_do_not_invent_sequence_gaps() {
        let mut orders = [video::VideoOrder::new(), video::VideoOrder::new()];
        for (id, sequence) in [(0, 0), (1, 0), (0, 1), (1, 1), (1, 2), (0, 2)] {
            let frame = SurfaceFrameInfo {
                geometry_generation: 1,
                surface_sequence: sequence,
            };
            let ready = order_video(&mut orders[id], frame, sequence == 0, &[id as u8]);
            assert_eq!(ready.frames, vec![vec![id as u8]]);
            assert!(
                !ready.ask_for_keyframe,
                "another window is not a missing frame"
            );
        }
    }

    #[test]
    fn video_before_announcement_or_after_removal_has_no_surface_target() {
        let mut registry = SurfaceRegistry::new(32).unwrap();
        assert!(announced_surface(None, false, 0).is_none());
        assert!(announced_surface(Some(&registry), true, 0).is_none());
        registry.upsert(surface(0)).unwrap();
        assert!(announced_surface(Some(&registry), false, 0).is_none());
        assert!(announced_surface(Some(&registry), true, 0).is_some());
        registry.remove(0).unwrap();
        assert!(announced_surface(Some(&registry), true, 0).is_none());
    }

    fn surface(id: u8) -> SurfaceInfo {
        SurfaceInfo {
            surface_id: id,
            geometry_generation: 1,
            title: "Application".into(),
            width: 640,
            height: 480,
            scale: 1.0,
            parent_surface_id: None,
            modal: false,
            minimized: false,
        }
    }

    #[test]
    fn multiple_surfaces_reject_stale_commands_without_harming_siblings() {
        let mut registry = SurfaceRegistry::new(32).unwrap();
        registry.upsert(surface(0)).unwrap();
        registry.upsert(surface(1)).unwrap();
        let mut resized = surface(0);
        resized.geometry_generation = 2;
        resized.width = 800;
        registry.upsert(resized).unwrap();
        assert!(registry
            .validate_command(&Message::Focus {
                surface_id: 0,
                geometry_generation: 1
            })
            .is_err());
        assert!(registry
            .validate_command(&Message::Focus {
                surface_id: 1,
                geometry_generation: 1
            })
            .is_ok());
        registry.remove(0).unwrap();
        assert!(registry.upsert(surface(0)).is_err());
        assert!(registry
            .validate_command(&Message::Close {
                surface_id: 0,
                geometry_generation: 2
            })
            .is_err());
        assert!(registry.get(1).is_some());
    }

    #[test]
    fn cancelling_close_keeps_the_surface_and_allows_another_normal_close() {
        let mut registry = SurfaceRegistry::new(32).unwrap();
        registry.upsert(surface(0)).unwrap();
        let mut close = CloseRequest::default();
        let now = Instant::now();
        assert!(close.request(now));
        assert!(!close.request(now));
        close.cancel();
        assert!(close.request(now));
        assert!(
            registry.get(0).is_some(),
            "close intent is not surface destruction"
        );
        assert!(
            close.request(now + Duration::from_secs(2)),
            "unanswered close may be retried"
        );
    }

    #[test]
    fn old_server_or_unannounced_application_is_never_connected() {
        let mut caps = ndp_proto::Caps {
            video_codecs: vec![ndp_proto::VideoCodec::H264],
            audio_codecs: vec![],
            displays: vec![],
            audio: Default::default(),
            color: Default::default(),
            max_bitrate_bps: 1_000_000,
            features: ndp_proto::FeatureFlags::NONE,
        };
        assert!(validate_ack(true, false, &caps, ndp_proto::VideoCodec::H264).is_err());
        caps.features = ndp_proto::FeatureFlags::APPLICATION_WINDOWS;
        assert!(validate_ack(false, false, &caps, ndp_proto::VideoCodec::H264).is_err());
        assert!(validate_ack(true, true, &caps, ndp_proto::VideoCodec::H264).is_err());
        assert!(validate_ack(true, false, &caps, ndp_proto::VideoCodec::H264).is_ok());
    }

    #[tokio::test]
    async fn startup_failure_is_sanitized_and_cancellation_is_bounded() {
        let ticket = SessionTicket {
            session_id: uuid::Uuid::nil(),
            ticket: "SECRET_BEARER".into(),
            gateway_addr: "invalid".into(),
            gateway_pin: "invalid".into(),
            agent_key: "invalid".into(),
            policy: nebula_common::SessionPolicy::view_only(),
            application_windows: true,
        };
        let (_sender, mut stop) = tokio::sync::watch::channel(false);
        let error = establish(&ticket, &mut stop).await.err().unwrap();
        assert_eq!(
            error,
            "Could not establish the encrypted application session"
        );
        assert!(!error.contains("SECRET"));
        let (_sender, mut stopped) = tokio::sync::watch::channel(true);
        assert!(establish(&ticket, &mut stopped).await.unwrap().is_none());
    }

    #[test]
    fn application_startup_errors_are_safe_for_desktop_presentation() {
        for reason in [
            ApplicationFailureReason::LaunchFailed,
            ApplicationFailureReason::PermissionDenied,
            ApplicationFailureReason::BackendUnavailable,
            ApplicationFailureReason::IsolationUnavailable,
            ApplicationFailureReason::SurfaceLimitReached,
            ApplicationFailureReason::SurfaceUnavailable,
            ApplicationFailureReason::ProtocolMismatch,
        ] {
            assert!(nebula_desktop_protocol::is_safe_session_error(
                startup_failure(reason)
            ));
        }
        assert!(!remote_failure(false, Some("SECRET_BEARER /private/app/path")).contains("SECRET"));
    }
}
