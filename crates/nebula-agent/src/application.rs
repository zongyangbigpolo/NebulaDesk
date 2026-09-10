//! Application-only platform boundary and bounded native-surface lifecycle.
//!
//! Native identifiers never cross the transport. Implementations must identify
//! an exclusively launched instance; sharing an OS login is not user isolation.

use std::collections::{BTreeMap, BTreeSet};

use ndp_proto::application::{
    ApplicationMessage, SurfaceInfo, SurfaceRegistry, MAX_APPLICATION_SURFACES,
};
use ndp_proto::InputEvent;

use crate::media::VideoSource;

#[derive(Debug, thiserror::Error)]
#[error("application surface ID limit reached")]
struct SurfaceLimit;

/// A proven-owned native surface. Native handles must not be recycled here.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeSurface {
    /// Backend identifier, unique throughout this application instance.
    pub native_id: u64,
    /// Proven native relationship, not inferred from title or process alone.
    pub parent: Option<u64>,
    /// Display-only title.
    pub title: String,
    /// Isolated capture width in pixels.
    pub width: u32,
    /// Isolated capture height in pixels.
    pub height: u32,
    /// Pixels per logical point.
    pub scale: f32,
    /// Nonzero monotonic generation, including native origin changes.
    pub geometry_generation: u32,
    /// Whether this surface blocks its parent.
    pub modal: bool,
    /// Whether native capture is currently minimized.
    pub minimized: bool,
}

/// Platform implementation of one owned application instance.
pub trait ApplicationBackend: Send + 'static {
    /// Enumerate only authorized windows and children of the launched instance.
    fn snapshot(&mut self) -> anyhow::Result<Vec<NativeSurface>>;
    /// Live, previously authorized surfaces temporarily unavailable for capture.
    fn unavailable(&self) -> Vec<u64> {
        Vec::new()
    }
    /// Whether fresh ownership checks prove a revision only resumes the exact
    /// same capture geometry. Other backends conservatively recreate capture.
    fn can_resume_capture(&self, _native_id: u64, _from: u32, _to: u32) -> bool {
        false
    }
    /// Build a native window capture source; display capture is forbidden.
    fn video(&mut self, native_id: u64) -> anyhow::Result<Box<dyn VideoSource>>;
    /// Recheck live identity, geometry and focus immediately before input.
    fn input(&mut self, native_id: u64, generation: u32, event: &InputEvent) -> anyhow::Result<()>;
    /// Perform a scoped native command, including its geometry check.
    fn operate(&mut self, native_id: u64, command: &ApplicationMessage) -> anyhow::Result<()>;
    /// Release held input on rejection, focus loss and teardown.
    fn release_input(&mut self);
    /// Release capture/input; never terminate a user's pre-existing process.
    fn stop(&mut self);
}

/// Session-local mapping. IDs are consumed, never recycled.
pub(crate) struct Surfaces {
    pub(crate) registry: SurfaceRegistry,
    pub(crate) native: BTreeMap<u64, u8>,
    pub(crate) live: BTreeMap<u8, NativeSurface>,
    retired: BTreeSet<u64>,
    next: u8,
}

impl Surfaces {
    pub(crate) fn new() -> Self {
        Self {
            registry: SurfaceRegistry::new(MAX_APPLICATION_SURFACES).expect("protocol limit"),
            native: BTreeMap::new(),
            live: BTreeMap::new(),
            retired: BTreeSet::new(),
            next: 0,
        }
    }

    pub(crate) fn reconcile(
        &mut self,
        snapshot: Vec<NativeSurface>,
    ) -> anyhow::Result<Vec<ApplicationMessage>> {
        anyhow::ensure!(
            snapshot.len() <= MAX_APPLICATION_SURFACES as usize,
            SurfaceLimit
        );
        anyhow::ensure!(
            snapshot
                .iter()
                .map(|s| u64::from(s.width) * u64::from(s.height))
                .fold(0u64, u64::saturating_add)
                <= 64 * 1024 * 1024,
            "aggregate application capture dimensions exceed memory budget"
        );
        let present: BTreeSet<_> = snapshot.iter().map(|s| s.native_id).collect();
        anyhow::ensure!(present.len() == snapshot.len(), "duplicate native surface");
        anyhow::ensure!(
            present.is_disjoint(&self.retired),
            "native surface identity reused"
        );
        let mut messages = Vec::new();
        // Retire children before parents, including a parent disappearing while
        // a child is still reported. Such contradictory snapshots fail closed.
        let mut removed: BTreeSet<_> = self
            .live
            .iter()
            .filter(|(_, s)| !present.contains(&s.native_id))
            .map(|(id, _)| *id)
            .collect();
        while !removed.is_empty() {
            let leaf = removed
                .iter()
                .copied()
                .find(|id| {
                    !self
                        .live
                        .values()
                        .any(|s| s.parent.and_then(|p| self.native.get(&p)) == Some(id))
                })
                .ok_or_else(|| anyhow::anyhow!("invalid native parent removal"))?;
            self.registry.remove(leaf)?;
            let old = self.live.remove(&leaf).expect("live leaf");
            self.native.remove(&old.native_id);
            self.retired.insert(old.native_id);
            removed.remove(&leaf);
            messages.push(ApplicationMessage::SurfaceRemove { surface_id: leaf });
        }
        let mut pending = snapshot;
        while !pending.is_empty() {
            let position = pending
                .iter()
                .position(|s| s.parent.is_none_or(|p| self.native.contains_key(&p)))
                .ok_or_else(|| anyhow::anyhow!("unknown or cyclic native parent"))?;
            let mut surface = pending.remove(position);
            surface.title.retain(|c| !c.is_control());
            while surface.title.len() > ndp_proto::application::MAX_SURFACE_TITLE_BYTES {
                surface.title.pop();
            }
            let id = match self.native.get(&surface.native_id) {
                Some(id) => *id,
                None => {
                    anyhow::ensure!(self.next < MAX_APPLICATION_SURFACES, SurfaceLimit);
                    let id = self.next;
                    self.next += 1;
                    self.native.insert(surface.native_id, id);
                    id
                }
            };
            let info = SurfaceInfo {
                surface_id: id,
                geometry_generation: surface.geometry_generation,
                title: surface.title.clone(),
                width: surface.width,
                height: surface.height,
                scale: surface.scale,
                parent_surface_id: surface.parent.and_then(|p| self.native.get(&p).copied()),
                modal: surface.modal,
                minimized: surface.minimized,
            };
            self.registry.upsert(info.clone())?;
            if self.live.get(&id) != Some(&surface) {
                self.live.insert(id, surface);
                messages.push(ApplicationMessage::SurfaceUpsert { surface: info });
            }
        }
        Ok(messages)
    }

    pub(crate) fn target(&self, message: &ApplicationMessage) -> anyhow::Result<u64> {
        let info = self.registry.validate_command(message)?;
        Ok(self
            .live
            .get(&info.surface_id)
            .expect("registry mirrors live surfaces")
            .native_id)
    }
}

/// Shared desktop controller lease, including legacy desktop input sessions.
pub(crate) struct ControllerLease {
    held: bool,
    file: Option<std::fs::File>,
}
static CONTROLLER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl ControllerLease {
    pub(crate) fn acquire(shared_desktop: bool) -> anyhow::Result<Self> {
        if !shared_desktop {
            return Ok(Self {
                held: false,
                file: None,
            });
        }
        CONTROLLER
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| anyhow::anyhow!("the shared desktop already has a controller"))?;
        let mut lease = Self {
            held: true,
            file: None,
        };
        #[cfg(test)]
        let directory = std::env::current_dir()?
            .join("target")
            .join("test-controller");
        #[cfg(not(test))]
        let directory = std::env::var_os(if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        })
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("shared desktop lease directory unavailable"))?
        .join(".nebula");
        std::fs::create_dir_all(&directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("desktop-controller.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .map_err(|_| anyhow::anyhow!("the shared desktop already has a controller"))?;
        lease.file = Some(file);
        Ok(lease)
    }
}

impl Drop for ControllerLease {
    fn drop(&mut self) {
        self.file = None;
        if self.held {
            CONTROLLER.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Output from the native worker, separate from its bounded video queue.
pub(crate) enum WorkerEvent {
    Ready,
    Ended,
    Metadata(ApplicationMessage),
    Failed(ndp_proto::application::ApplicationFailureReason),
    SurfaceFailed(u8),
}

pub(crate) struct SurfaceFrame {
    pub(crate) id: u8,
    pub(crate) generation: u32,
    pub(crate) frame: crate::media::EncodedFrame,
    delivery: std::sync::Arc<std::sync::atomic::AtomicU64>,
    epoch: u64,
}

impl SurfaceFrame {
    pub(crate) fn is_current(&self) -> bool {
        self.epoch & 1 != 0
            && self.delivery.load(std::sync::atomic::Ordering::Acquire) == self.epoch
    }
}

fn forward_frames(
    id: u8,
    mut frames: tokio::sync::mpsc::Receiver<(crate::media::EncodedFrame, u64)>,
    output: tokio::sync::mpsc::Sender<SurfaceFrame>,
    delivery: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    // Native discovery and sibling startup must not block established surfaces.
    tokio::spawn(async move {
        while let Some((frame, epoch)) = frames.recv().await {
            use std::sync::atomic::Ordering;
            if epoch & 1 == 0 || delivery.load(Ordering::Acquire) != epoch {
                continue;
            }
            let Ok(permit) = output.reserve().await else {
                break;
            };
            if delivery.load(Ordering::Acquire) == epoch {
                permit.send(SurfaceFrame {
                    id,
                    generation: (epoch >> 32) as u32,
                    frame,
                    delivery: delivery.clone(),
                    epoch,
                });
            }
        }
    })
}

pub(crate) enum WorkerCommand {
    Client(ApplicationMessage),
    Keyframe(u8),
}

pub(crate) struct CommandSender(std::sync::mpsc::SyncSender<(std::time::Instant, WorkerCommand)>);

impl CommandSender {
    #[cfg(test)]
    fn send(
        &self,
        command: WorkerCommand,
    ) -> Result<(), std::sync::mpsc::SendError<WorkerCommand>> {
        self.0
            .send((std::time::Instant::now(), command))
            .map_err(|std::sync::mpsc::SendError((_, command))| std::sync::mpsc::SendError(command))
    }

    pub(crate) fn try_send(
        &self,
        command: WorkerCommand,
    ) -> Result<(), std::sync::mpsc::TrySendError<WorkerCommand>> {
        self.0
            .try_send((std::time::Instant::now(), command))
            .map_err(|error| match error {
                std::sync::mpsc::TrySendError::Full((_, command)) => {
                    std::sync::mpsc::TrySendError::Full(command)
                }
                std::sync::mpsc::TrySendError::Disconnected((_, command)) => {
                    std::sync::mpsc::TrySendError::Disconnected(command)
                }
            })
    }
}

pub(crate) struct Worker {
    pub(crate) events: tokio::sync::mpsc::Receiver<WorkerEvent>,
    pub(crate) frames: tokio::sync::mpsc::Receiver<SurfaceFrame>,
    pub(crate) commands: CommandSender,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

const WARM_RESUME_FRAME_DEADLINE: std::time::Duration = std::time::Duration::from_millis(500);

struct Capture {
    source: Box<dyn VideoSource>,
    forwarding: tokio::task::JoinHandle<()>,
    delivery_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    produced_keyframe: std::sync::Arc<std::sync::atomic::AtomicU64>,
    resume_deadline: Option<(u64, std::time::Instant)>,
    generation: u32,
}

impl Capture {
    fn pause(&self) {
        use std::sync::atomic::Ordering;
        if self.delivery_epoch.load(Ordering::Acquire) & 1 != 0 {
            self.delivery_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn resume(&mut self) {
        use std::sync::atomic::Ordering;
        if self.delivery_epoch.load(Ordering::Acquire) & 1 == 0 {
            self.source.request_keyframe();
            let epoch = self.delivery_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            self.resume_deadline = Some((
                epoch,
                std::time::Instant::now() + WARM_RESUME_FRAME_DEADLINE,
            ));
        }
    }

    fn warm_resume_stalled(&mut self, now: std::time::Instant) -> bool {
        use std::sync::atomic::Ordering;
        let Some((epoch, deadline)) = self.resume_deadline else {
            return false;
        };
        if self.delivery_epoch.load(Ordering::Acquire) != epoch
            || self.produced_keyframe.load(Ordering::Acquire) >= epoch
        {
            self.resume_deadline = None;
            return false;
        }
        now >= deadline
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.pause();
        self.forwarding.abort();
        self.source.stop();
    }
}

struct Runtime {
    backend: Box<dyn ApplicationBackend>,
    surfaces: Surfaces,
    captures: BTreeMap<u8, Capture>,
    retry: BTreeMap<u8, std::time::Instant>,
    bitrate: u32,
    frames: tokio::sync::mpsc::Sender<SurfaceFrame>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.captures.clear();
        self.backend.release_input();
        self.backend.stop();
    }
}

impl Runtime {
    fn command(&mut self, command: WorkerCommand, input_allowed: bool) -> anyhow::Result<()> {
        let command = match command {
            WorkerCommand::Keyframe(id) => {
                if let Some(capture) = self.captures.get_mut(&id) {
                    capture.source.request_keyframe();
                }
                return Ok(());
            }
            WorkerCommand::Client(command) => command,
        };
        let native = self.surfaces.target(&command)?;
        let (id, generation) = command.command_target().expect("validated command");
        match command {
            ApplicationMessage::RequestKeyframe { .. } => {
                if let Some(capture) = self.captures.get_mut(&id) {
                    capture.source.request_keyframe();
                }
                Ok(())
            }
            ApplicationMessage::Input { event, .. } if input_allowed => {
                let event = InputEvent::decode(&mut event.as_slice())?;
                self.backend.input(native, generation, &event)
            }
            _ if input_allowed => self.backend.operate(native, &command),
            _ => anyhow::bail!("application input is not authorized"),
        }
    }

    fn refresh(&mut self, events: &tokio::sync::mpsc::Sender<WorkerEvent>) -> anyhow::Result<()> {
        let snapshot = self.backend.snapshot()?;
        let changes = self.surfaces.reconcile(snapshot)?;
        let unavailable: BTreeSet<_> = self.backend.unavailable().into_iter().collect();
        for change in changes {
            match &change {
                ApplicationMessage::SurfaceRemove { surface_id } => {
                    self.captures.remove(surface_id);
                    self.retry.remove(surface_id);
                    self.backend.release_input();
                }
                ApplicationMessage::SurfaceUpsert { surface } => {
                    let id = surface.surface_id;
                    let native = self.surfaces.live[&id].native_id;
                    let restart = self.captures.get(&id).is_none_or(|capture| {
                        capture.generation != surface.geometry_generation
                            && !self.backend.can_resume_capture(
                                native,
                                capture.generation,
                                surface.geometry_generation,
                            )
                    });
                    if surface.minimized || restart {
                        self.captures.remove(&id);
                    } else if let Some(capture) = self.captures.get_mut(&id) {
                        if capture.generation != surface.geometry_generation {
                            capture.pause();
                            capture.generation = surface.geometry_generation;
                            use std::sync::atomic::Ordering;
                            let epoch = capture.delivery_epoch.load(Ordering::Acquire)
                                & u64::from(u32::MAX);
                            capture.delivery_epoch.store(
                                (u64::from(capture.generation) << 32) | epoch,
                                Ordering::Release,
                            );
                        }
                    }
                    events
                        .try_send(WorkerEvent::Metadata(change.clone()))
                        .map_err(|_| anyhow::anyhow!("application lifecycle queue stalled"))?;
                    continue;
                }
                _ => {}
            }
            events
                .try_send(WorkerEvent::Metadata(change))
                .map_err(|_| anyhow::anyhow!("application lifecycle queue stalled"))?;
        }
        let ids: Vec<_> = self.surfaces.live.keys().copied().collect();
        for id in ids {
            let native = self.surfaces.live[&id].native_id;
            if unavailable.contains(&native) {
                if let Some(capture) = self.captures.get(&id) {
                    capture.pause();
                }
                self.backend.release_input();
                if !self.retry.contains_key(&id) {
                    events
                        .try_send(WorkerEvent::SurfaceFailed(id))
                        .map_err(|_| anyhow::anyhow!("application lifecycle queue stalled"))?;
                }
                self.retry.insert(id, std::time::Instant::now());
                continue;
            }
            let surface = self
                .surfaces
                .registry
                .get(id)
                .expect("live surface")
                .clone();
            if self.retry.contains_key(&id)
                && self
                    .captures
                    .get(&id)
                    .is_some_and(|capture| !capture.source.preserves_frame_provenance())
            {
                self.captures.remove(&id);
            }
            if self
                .captures
                .get_mut(&id)
                .is_some_and(|capture| capture.warm_resume_stalled(std::time::Instant::now()))
            {
                tracing::info!(
                    surface = id,
                    "warm resume has no fresh image; restarting isolated capture"
                );
                self.captures.remove(&id);
            }
            if let Some(capture) = self.captures.get_mut(&id) {
                if self.retry.remove(&id).is_some() {
                    events
                        .try_send(WorkerEvent::Metadata(ApplicationMessage::SurfaceUpsert {
                            surface: surface.clone(),
                        }))
                        .map_err(|_| anyhow::anyhow!("application lifecycle queue stalled"))?;
                    capture.resume();
                }
                continue;
            }
            if surface.minimized
                || self
                    .retry
                    .get(&id)
                    .is_some_and(|retry| *retry > std::time::Instant::now())
            {
                continue;
            }
            match self.start_capture(id, &surface) {
                Ok(()) => {
                    if self.retry.remove(&id).is_some() {
                        events
                            .try_send(WorkerEvent::Metadata(ApplicationMessage::SurfaceUpsert {
                                surface,
                            }))
                            .map_err(|_| anyhow::anyhow!("application lifecycle queue stalled"))?;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, surface = id, "isolated surface capture unavailable");
                    if !self.retry.contains_key(&id) {
                        events
                            .try_send(WorkerEvent::SurfaceFailed(id))
                            .map_err(|_| anyhow::anyhow!("application lifecycle queue stalled"))?;
                    }
                    self.retry.insert(
                        id,
                        std::time::Instant::now() + std::time::Duration::from_secs(1),
                    );
                }
            }
        }
        let bitrate = self.surface_bitrate();
        for capture in self.captures.values_mut() {
            capture.source.set_bitrate(bitrate);
        }
        Ok(())
    }

    fn start_capture(&mut self, id: u8, surface: &SurfaceInfo) -> anyhow::Result<()> {
        let native = self.surfaces.live[&id].native_id;
        tracing::info!(
            surface = id,
            native,
            generation = surface.geometry_generation,
            "starting isolated application capture"
        );
        let source = self.backend.video(native)?;
        let (tx, frames) = tokio::sync::mpsc::channel(3);
        let output = self.frames.clone();
        let delivery_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            (u64::from(surface.geometry_generation) << 32) | 1,
        ));
        let delivery = delivery_epoch.clone();
        let produced_keyframe = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let tx = crate::media::FrameSink::application(
            tx,
            delivery_epoch.clone(),
            produced_keyframe.clone(),
        );
        let forwarding = forward_frames(id, frames, output, delivery);
        let mut capture = Capture {
            source,
            forwarding,
            delivery_epoch,
            produced_keyframe,
            resume_deadline: None,
            generation: surface.geometry_generation,
        };
        capture.source.start(
            crate::media::VideoConfig {
                width: surface.width,
                height: surface.height,
                fps: 30,
                bitrate: self.surface_bitrate(),
            },
            tx,
        )?;
        self.captures.insert(id, capture);
        tracing::info!(
            surface = id,
            generation = surface.geometry_generation,
            "isolated application capture started"
        );
        Ok(())
    }

    fn surface_bitrate(&self) -> u32 {
        (self.bitrate / self.surfaces.live.len().max(1) as u32).max(1)
    }
}

/// Confine launch, native API calls and capture lifetimes to one bounded worker.
pub(crate) fn spawn(
    platform: std::sync::Arc<dyn crate::media::Platform>,
    launch: nebula_common::ApplicationLaunch,
    input_allowed: bool,
    bitrate: u32,
    lease: ControllerLease,
) -> anyhow::Result<Worker> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    let (event_tx, events) = tokio::sync::mpsc::channel(64);
    let (frame_tx, frames) = tokio::sync::mpsc::channel(8);
    let (commands, inbox) = std::sync::mpsc::sync_channel::<(Instant, WorkerCommand)>(64);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    // Synthetic encoders also work on the native worker during unit tests.
    let handle = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("nebula-application".into())
        .spawn(move || {
            let _lease = lease;
            let _entered = handle.enter();
            let mut failure = ndp_proto::application::ApplicationFailureReason::BackendUnavailable;
            let result = (|| {
                let capability = platform.application_capability();
                if capability.reason
                    == Some(
                        nebula_common::application::ApplicationUnavailableReason::PermissionDenied,
                    )
                {
                    failure = ndp_proto::application::ApplicationFailureReason::PermissionDenied;
                }
                anyhow::ensure!(capability.is_supported(), "application backend unavailable");
                failure = ndp_proto::application::ApplicationFailureReason::LaunchFailed;
                let mut runtime = Runtime {
                    backend: platform.application(&launch)?,
                    surfaces: Surfaces::new(),
                    captures: BTreeMap::new(),
                    retry: BTreeMap::new(),
                    bitrate,
                    frames: frame_tx.clone(),
                };
                failure = ndp_proto::application::ApplicationFailureReason::IsolationUnavailable;
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                event_tx
                    .try_send(WorkerEvent::Ready)
                    .map_err(|_| anyhow::anyhow!("application peer left"))?;
                let mut refresh = Instant::now();
                let mut had_surface = false;
                while !stop.load(Ordering::Acquire) && !frame_tx.is_closed() {
                    for _ in 0..64 {
                        let (queued, command) = match inbox.try_recv() {
                            Ok(command) => command,
                            Err(std::sync::mpsc::TryRecvError::Empty) => break,
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
                        };
                        let queue_ms = queued.elapsed().as_millis();
                        if queue_ms >= 100 {
                            let kind = match &command {
                                WorkerCommand::Keyframe(_)
                                | WorkerCommand::Client(ApplicationMessage::RequestKeyframe {
                                    ..
                                }) => "keyframe",
                                WorkerCommand::Client(ApplicationMessage::Input { .. }) => "input",
                                WorkerCommand::Client(ApplicationMessage::Focus { .. }) => "focus",
                                _ => "control",
                            };
                            tracing::info!(
                                queue_ms,
                                kind,
                                "native application command queue was slow"
                            );
                        }
                        if let Err(error) = runtime.command(command, input_allowed) {
                            runtime.backend.release_input();
                            tracing::debug!(%error, "scoped application command rejected");
                        }
                    }
                    if Instant::now() >= refresh {
                        runtime.refresh(&event_tx)?;
                        if had_surface && runtime.surfaces.live.is_empty() {
                            return Ok(());
                        }
                        had_surface |= !runtime.surfaces.live.is_empty();
                        refresh = Instant::now() + Duration::from_millis(200);
                    }
                    let mut ended = Vec::new();
                    for (&id, capture) in runtime.captures.iter_mut() {
                        if capture.forwarding.is_finished() {
                            ended.push(id);
                            event_tx
                                .try_send(WorkerEvent::SurfaceFailed(id))
                                .map_err(|_| anyhow::anyhow!("application peer left"))?;
                        }
                    }
                    for id in ended {
                        runtime.captures.remove(&id);
                        runtime
                            .retry
                            .insert(id, Instant::now() + Duration::from_secs(1));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "application runtime failed closed");
                if error.downcast_ref::<SurfaceLimit>().is_some() {
                    failure = ndp_proto::application::ApplicationFailureReason::SurfaceLimitReached;
                }
                let _ = event_tx.try_send(WorkerEvent::Failed(failure));
            } else if !stop.load(Ordering::Acquire) && !frame_tx.is_closed() {
                let _ = event_tx.try_send(WorkerEvent::Ended);
            }
        })?;
    Ok(Worker {
        events,
        frames,
        commands: CommandSender(commands),
        stopped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(id: u64, parent: Option<u64>) -> NativeSurface {
        NativeSurface {
            native_id: id,
            parent,
            title: "Editor".into(),
            width: 640,
            height: 480,
            scale: 1.0,
            geometry_generation: 1,
            modal: parent.is_some(),
            minimized: false,
        }
    }

    #[test]
    fn children_are_created_after_and_removed_before_parents() {
        let mut surfaces = Surfaces::new();
        let messages = surfaces
            .reconcile(vec![surface(2, Some(1)), surface(1, None)])
            .unwrap();
        assert!(
            matches!(&messages[0], ApplicationMessage::SurfaceUpsert { surface } if surface.surface_id == 0)
        );
        assert!(
            matches!(&messages[1], ApplicationMessage::SurfaceUpsert { surface } if surface.parent_surface_id == Some(0))
        );
        assert_eq!(
            surfaces.reconcile(vec![]).unwrap(),
            vec![
                ApplicationMessage::SurfaceRemove { surface_id: 1 },
                ApplicationMessage::SurfaceRemove { surface_id: 0 }
            ]
        );
        assert!(surfaces.reconcile(vec![surface(1, None)]).is_err());
    }

    #[test]
    fn stale_unknown_and_exhausted_surface_ids_fail_closed() {
        let mut surfaces = Surfaces::new();
        for i in 0..MAX_APPLICATION_SURFACES {
            surfaces
                .reconcile(vec![surface(u64::from(i), None)])
                .unwrap();
            let command = ApplicationMessage::Focus {
                surface_id: i,
                geometry_generation: 2,
            };
            assert!(surfaces.target(&command).is_err());
        }
        assert!(surfaces.reconcile(vec![surface(999, None)]).is_err());
    }

    #[test]
    fn native_cycles_invalid_dimensions_and_duplicate_handles_are_rejected() {
        assert!(Surfaces::new()
            .reconcile(vec![surface(1, Some(2)), surface(2, Some(1))])
            .is_err());
        assert!(Surfaces::new()
            .reconcile(vec![surface(1, None), surface(1, None)])
            .is_err());
        let mut invalid = surface(1, None);
        invalid.width = 0;
        assert!(Surfaces::new().reconcile(vec![invalid]).is_err());
    }

    #[test]
    fn default_platform_never_advertises_application_capture() {
        use crate::media::Platform;
        assert!(!crate::media::TestPattern::default()
            .application_capability()
            .is_supported());
        assert!(crate::media::TestPattern::default()
            .application(&nebula_common::ApplicationLaunch {
                launch_path: "/application".into(),
                launch_args: vec![],
                working_dir: None,
            })
            .is_err());
    }

    #[derive(Default)]
    struct Probe {
        surfaces: std::sync::Mutex<Vec<NativeSurface>>,
        operations: std::sync::atomic::AtomicUsize,
        released: std::sync::atomic::AtomicUsize,
        stopped: std::sync::atomic::AtomicUsize,
        unavailable: std::sync::atomic::AtomicBool,
        blocked_video: std::sync::Mutex<Option<(u64, std::sync::mpsc::Receiver<()>)>>,
        video_blocked: std::sync::atomic::AtomicBool,
        video_starts: std::sync::atomic::AtomicUsize,
        resume_generation: std::sync::atomic::AtomicBool,
        unchanged_video: std::sync::atomic::AtomicBool,
    }

    struct SyntheticApplication(std::sync::Arc<Probe>);

    #[derive(Default)]
    struct UnchangedVideo {
        image: Option<crate::media::FrameSink>,
    }

    impl VideoSource for UnchangedVideo {
        fn preserves_frame_provenance(&self) -> bool {
            true
        }
        fn start(
            &mut self,
            _: crate::media::VideoConfig,
            sink: crate::media::FrameSink,
        ) -> anyhow::Result<()> {
            // Like an unchanged SCK window: one fresh image at stream startup,
            // then only cached pixels, regardless of subsequent IDR requests.
            self.image = Some(sink.for_frame());
            self.request_keyframe();
            Ok(())
        }
        fn request_keyframe(&mut self) {
            let _ = self
                .image
                .as_ref()
                .unwrap()
                .try_send(crate::media::EncodedFrame {
                    keyframe: true,
                    timestamp_us: 0,
                    data: vec![42],
                });
        }
        fn set_bitrate(&mut self, _: u32) {}
        fn stop(&mut self) {
            self.image = None;
        }
    }

    struct BlockedStartVideo {
        video: crate::media::SyntheticVideo,
        resume: std::sync::mpsc::Receiver<()>,
        probe: std::sync::Arc<Probe>,
    }

    impl VideoSource for BlockedStartVideo {
        fn start(
            &mut self,
            config: crate::media::VideoConfig,
            sink: crate::media::FrameSink,
        ) -> anyhow::Result<()> {
            self.probe
                .video_blocked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = self.resume.recv();
            self.video.start(config, sink)
        }
        fn request_keyframe(&mut self) {
            self.video.request_keyframe();
        }
        fn set_bitrate(&mut self, bitrate: u32) {
            self.video.set_bitrate(bitrate);
        }
        fn stop(&mut self) {
            self.video.stop();
        }
    }

    impl ApplicationBackend for SyntheticApplication {
        fn can_resume_capture(&self, _: u64, from: u32, to: u32) -> bool {
            self.0
                .resume_generation
                .load(std::sync::atomic::Ordering::SeqCst)
                && from.checked_add(1) == Some(to)
        }
        fn snapshot(&mut self) -> anyhow::Result<Vec<NativeSurface>> {
            Ok(self.0.surfaces.lock().unwrap().clone())
        }
        fn unavailable(&self) -> Vec<u64> {
            if self.0.unavailable.load(std::sync::atomic::Ordering::SeqCst) {
                self.0
                    .surfaces
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|s| s.native_id)
                    .collect()
            } else {
                Vec::new()
            }
        }
        fn video(&mut self, id: u64) -> anyhow::Result<Box<dyn VideoSource>> {
            self.0
                .video_starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self
                .0
                .unchanged_video
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(Box::new(UnchangedVideo::default()));
            }
            let mut blocked = self.0.blocked_video.lock().unwrap();
            if blocked.as_ref().is_some_and(|(target, _)| *target == id) {
                let (_, resume) = blocked.take().unwrap();
                return Ok(Box::new(BlockedStartVideo {
                    video: crate::media::SyntheticVideo::default(),
                    resume,
                    probe: self.0.clone(),
                }));
            }
            Ok(Box::new(crate::media::SyntheticVideo::default()))
        }
        fn input(&mut self, _: u64, _: u32, _: &InputEvent) -> anyhow::Result<()> {
            self.0
                .operations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn operate(&mut self, _: u64, _: &ApplicationMessage) -> anyhow::Result<()> {
            self.0
                .operations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn release_input(&mut self) {
            self.0
                .released
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn stop(&mut self) {
            self.0
                .stopped
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl crate::media::Platform for SyntheticApplication {
        fn shared_desktop(&self) -> bool {
            false
        }
        fn application_capability(&self) -> nebula_common::ApplicationCapability {
            nebula_common::ApplicationCapability {
                supported: true,
                protocol_version: 1,
                max_surfaces: 32,
                global_menu_supported: false,
                reason: None,
            }
        }
        fn application(
            &self,
            _: &nebula_common::ApplicationLaunch,
        ) -> anyhow::Result<Box<dyn ApplicationBackend>> {
            Ok(Box::new(Self(self.0.clone())))
        }
        fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
            panic!("application used display capture")
        }
        fn input(&self) -> anyhow::Result<Box<dyn crate::media::InputInjector>> {
            panic!("application used global input")
        }
        fn audio(&self) -> anyhow::Result<Box<dyn crate::media::AudioSource>> {
            panic!("application used global audio")
        }
        fn clipboard(&self) -> anyhow::Result<Box<dyn crate::clipboard::ClipboardAccess>> {
            panic!("application used global clipboard")
        }
    }

    #[tokio::test]
    async fn established_surface_flows_while_sibling_native_start_is_blocked() {
        use std::sync::{atomic::Ordering, Arc};
        let probe = Arc::new(Probe::default());
        *probe.surfaces.lock().unwrap() = vec![surface(100, None), surface(200, None)];
        let (resume, blocked) = std::sync::mpsc::channel();
        *probe.blocked_video.lock().unwrap() = Some((200, blocked));
        let mut worker = spawn(
            Arc::new(SyntheticApplication(probe.clone())),
            nebula_common::ApplicationLaunch {
                launch_path: "/synthetic".into(),
                launch_args: vec![],
                working_dir: None,
            },
            true,
            4_000_000,
            ControllerLease::acquire(false).unwrap(),
        )
        .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !probe.video_blocked.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            for _ in 0..5 {
                let frame = worker.frames.recv().await.unwrap();
                assert_eq!((frame.id, frame.generation), (0, 1));
            }
        })
        .await;
        let _ = resume.send(());
        assert!(
            result.is_ok(),
            "sibling native startup must not starve an established surface"
        );
    }

    #[tokio::test]
    async fn application_worker_routes_surfaces_rejects_stale_commands_and_stops() {
        use std::sync::{atomic::Ordering, Arc};
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let probe = Arc::new(Probe::default());
            *probe.surfaces.lock().unwrap() = vec![surface(100, None), surface(200, Some(100))];
            let mut worker = spawn(
                Arc::new(SyntheticApplication(probe.clone())),
                nebula_common::ApplicationLaunch {
                    launch_path: "/synthetic".into(),
                    launch_args: vec![],
                    working_dir: None,
                },
                true,
                4_000_000,
                ControllerLease::acquire(false).unwrap(),
            )
            .unwrap();
            assert!(matches!(
                worker.events.recv().await,
                Some(WorkerEvent::Ready)
            ));
            for id in [0, 1] {
                assert!(
                    matches!(worker.events.recv().await, Some(WorkerEvent::Metadata(
                    ApplicationMessage::SurfaceUpsert { surface })) if surface.surface_id == id)
                );
            }
            let mut seen = BTreeSet::new();
            while seen.len() != 2 {
                let frame = worker.frames.recv().await.unwrap();
                seen.insert(frame.id);
                assert_eq!(frame.generation, 1);
            }
            worker
                .commands
                .send(WorkerCommand::Client(ApplicationMessage::Focus {
                    surface_id: 1,
                    geometry_generation: 1,
                }))
                .unwrap();
            worker
                .commands
                .send(WorkerCommand::Client(ApplicationMessage::Close {
                    surface_id: 0,
                    geometry_generation: 2,
                }))
                .unwrap();
            while probe.released.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            assert_eq!(probe.operations.load(Ordering::SeqCst), 1);
            probe.surfaces.lock().unwrap().clear();
            for id in [1, 0] {
                assert!(
                    matches!(worker.events.recv().await, Some(WorkerEvent::Metadata(
                    ApplicationMessage::SurfaceRemove { surface_id })) if surface_id == id)
                );
            }
            drop(worker);
            while probe.stopped.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn application_view_only_rejects_window_operations_and_input() {
        let probe = std::sync::Arc::new(Probe::default());
        let mut runtime = Runtime {
            backend: Box::new(SyntheticApplication(probe.clone())),
            surfaces: Surfaces::new(),
            captures: BTreeMap::new(),
            retry: BTreeMap::new(),
            bitrate: 4_000_000,
            frames: tokio::sync::mpsc::channel(8).0,
        };
        runtime
            .surfaces
            .reconcile(vec![surface(100, None)])
            .unwrap();
        assert!(runtime
            .command(
                WorkerCommand::Client(ApplicationMessage::Focus {
                    surface_id: 0,
                    geometry_generation: 1
                }),
                false
            )
            .is_err());
        assert!(runtime
            .command(
                WorkerCommand::Client(ApplicationMessage::RequestKeyframe {
                    surface_id: 0,
                    geometry_generation: 1
                }),
                false
            )
            .is_ok());
        assert_eq!(
            probe.operations.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn application_and_desktop_controllers_share_an_exclusive_lease() {
        let lease = ControllerLease::acquire(true).unwrap();
        assert!(ControllerLease::acquire(true).is_err());
        assert!(ControllerLease::acquire(false).is_ok());
        drop(lease);
        assert!(ControllerLease::acquire(true).is_ok());
    }

    #[tokio::test]
    async fn unchanged_source_warm_resume_restarts_without_injecting_a_new_image() {
        use std::sync::{atomic::Ordering, Arc};
        let probe = Arc::new(Probe::default());
        probe.unchanged_video.store(true, Ordering::SeqCst);
        *probe.surfaces.lock().unwrap() = vec![surface(100, None)];
        let (frames, mut pictures) = tokio::sync::mpsc::channel(8);
        let mut runtime = Runtime {
            backend: Box::new(SyntheticApplication(probe.clone())),
            surfaces: Surfaces::new(),
            captures: BTreeMap::new(),
            retry: BTreeMap::new(),
            bitrate: 4_000_000,
            frames,
        };
        let (events, _received) = tokio::sync::mpsc::channel(64);
        runtime.refresh(&events).unwrap();
        let original = pictures.recv().await.unwrap();
        assert_eq!(original.frame.data, vec![42]);
        probe.unavailable.store(true, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        probe.resume_generation.store(true, Ordering::SeqCst);
        probe.surfaces.lock().unwrap()[0].geometry_generation = 2;
        probe.unavailable.store(false, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        assert_eq!(probe.video_starts.load(Ordering::SeqCst), 1);
        assert!(!original.is_current());
        assert!(
            pictures.try_recv().is_err(),
            "cached pixels cannot acquire a new epoch"
        );
        let capture = runtime.captures.get_mut(&0).unwrap();
        let (epoch, deadline) = capture.resume_deadline.unwrap();
        assert!(!capture.warm_resume_stalled(deadline - WARM_RESUME_FRAME_DEADLINE));
        // Expire the real watchdog deterministically, without changing source
        // pixels or manually injecting any fresh image into the resumed source.
        capture.resume_deadline = Some((epoch, std::time::Instant::now()));
        runtime.refresh(&events).unwrap();
        let fresh = tokio::time::timeout(std::time::Duration::from_secs(1), pictures.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(probe.video_starts.load(Ordering::SeqCst), 2);
        assert_eq!(fresh.generation, 2);
        assert_eq!(
            fresh.frame.data, original.frame.data,
            "source content never changed"
        );
        assert!(fresh.frame.keyframe && fresh.is_current());
        assert!(!original.is_current());
    }

    #[tokio::test]
    async fn recovery_never_promotes_backlogged_or_inflight_old_keyframes() {
        use std::sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        };
        let delivery = Arc::new(AtomicU64::new((1 << 32) | 1));
        let (source, encoded) = tokio::sync::mpsc::channel(3);
        let produced = Arc::new(AtomicU64::new(0));
        let producer = crate::media::FrameSink::application(
            source.clone(),
            delivery.clone(),
            produced.clone(),
        );
        let (output, mut pictures) = tokio::sync::mpsc::channel(1);
        let forwarding = forward_frames(0, encoded, output.clone(), delivery.clone());
        let frame = |id| crate::media::EncodedFrame {
            keyframe: true,
            timestamp_us: id,
            data: vec![id as u8],
        };
        producer.try_send(frame(1)).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while output.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        producer.try_send(frame(2)).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while source.capacity() != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // One old frame is already published, another is blocked on reserve,
        // and the full source queue contains three more old keyframes.
        for id in 3..=5 {
            producer.try_send(frame(id)).unwrap();
        }
        let inflight = producer.for_frame();
        delivery.store((1 << 32) | 2, Ordering::Release);
        let paused_image = producer.for_frame();
        delivery.store((2 << 32) | 3, Ordering::Release);
        assert!(inflight.try_send(frame(6)).is_err());
        assert!(paused_image.try_send(frame(7)).is_err());
        assert_eq!(produced.load(Ordering::Acquire), (1 << 32) | 1);
        assert!(
            producer.try_send(frame(8)).is_err(),
            "source queue is still full"
        );
        assert_eq!(
            produced.load(Ordering::Acquire),
            (2 << 32) | 3,
            "native capture progress must be recorded despite network backpressure"
        );
        let old = pictures.recv().await.unwrap();
        assert!(!old.is_current());
        producer.send(frame(8)).await.unwrap();
        let fresh = tokio::time::timeout(std::time::Duration::from_secs(1), pictures.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh.frame.data, vec![8]);
        assert_eq!(fresh.generation, 2);
        assert!(fresh.is_current());
        assert!(pictures.try_recv().is_err());
        delivery.fetch_add(1, Ordering::AcqRel);
        assert!(
            !fresh.is_current(),
            "late delivery after removal must also fail"
        );
        forwarding.abort();
    }

    #[tokio::test]
    async fn application_unavailable_surface_recovers_without_reusing_its_id() {
        use std::sync::{atomic::Ordering, Arc};
        let probe = Arc::new(Probe::default());
        *probe.surfaces.lock().unwrap() = vec![surface(100, None)];
        let (frames, mut pictures) = tokio::sync::mpsc::channel(8);
        let mut runtime = Runtime {
            backend: Box::new(SyntheticApplication(probe.clone())),
            surfaces: Surfaces::new(),
            captures: BTreeMap::new(),
            retry: BTreeMap::new(),
            bitrate: 4_000_000,
            frames,
        };
        let (events, mut received) = tokio::sync::mpsc::channel(64);
        runtime.refresh(&events).unwrap();
        assert!(matches!(
            received.try_recv(),
            Ok(WorkerEvent::Metadata(
                ApplicationMessage::SurfaceUpsert { .. }
            ))
        ));
        assert!(runtime.captures.contains_key(&0));
        let old_frame = tokio::time::timeout(std::time::Duration::from_secs(2), pictures.recv())
            .await
            .unwrap()
            .unwrap();
        probe.unavailable.store(true, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        assert!(
            !old_frame.is_current(),
            "frames already queued before suspension must be invalidated"
        );
        assert!(matches!(
            received.try_recv(),
            Ok(WorkerEvent::SurfaceFailed(0))
        ));
        assert_eq!(
            runtime.captures[&0].delivery_epoch.load(Ordering::Acquire) & 1,
            0
        );
        while pictures.try_recv().is_ok() {}
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            pictures.try_recv().is_err(),
            "unverified surfaces must not deliver frames"
        );
        assert_eq!(runtime.surfaces.native.get(&100), Some(&0));
        probe.resume_generation.store(true, Ordering::SeqCst);
        probe.surfaces.lock().unwrap()[0].geometry_generation = 2;
        probe.unavailable.store(false, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        assert!(
            matches!(received.try_recv(), Ok(WorkerEvent::Metadata(ApplicationMessage::SurfaceUpsert { surface })) if surface.surface_id == 0)
        );
        assert!(runtime.captures.contains_key(&0));
        assert_eq!(
            probe.video_starts.load(Ordering::SeqCst),
            1,
            "transient AX unavailability must not restart native capture"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let frame = pictures.recv().await.unwrap();
                assert_eq!(frame.generation, 2);
                assert!(frame.is_current());
                if frame.frame.keyframe {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(
            !old_frame.is_current(),
            "resuming must not reactivate an old queued frame"
        );
        let capture = runtime.captures.get_mut(&0).unwrap();
        let (_, deadline) = capture.resume_deadline.unwrap();
        assert!(!capture.warm_resume_stalled(deadline));
        assert!(
            capture.resume_deadline.is_none(),
            "fresh keyframes disarm the watchdog"
        );

        probe.unavailable.store(true, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        probe.resume_generation.store(false, Ordering::SeqCst);
        probe.surfaces.lock().unwrap()[0].geometry_generation = 3;
        probe.unavailable.store(false, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        assert_eq!(
            probe.video_starts.load(Ordering::SeqCst),
            2,
            "a real geometry revision still replaces capture"
        );
        assert_eq!(runtime.captures[&0].generation, 3);

        // A backend's geometry proof is insufficient if its asynchronous source
        // does not also promise immutable native-frame provenance.
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        *probe.blocked_video.lock().unwrap() = Some((100, receiver));
        probe.surfaces.lock().unwrap()[0].geometry_generation = 4;
        runtime.refresh(&events).unwrap();
        assert!(!runtime.captures[&0].source.preserves_frame_provenance());
        probe.unavailable.store(true, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        probe.resume_generation.store(true, Ordering::SeqCst);
        probe.surfaces.lock().unwrap()[0].geometry_generation = 5;
        probe.unavailable.store(false, Ordering::SeqCst);
        runtime.refresh(&events).unwrap();
        assert_eq!(probe.video_starts.load(Ordering::SeqCst), 4);
        assert_eq!(runtime.captures[&0].generation, 5);
    }

    #[tokio::test]
    async fn application_close_waits_for_native_confirmation_and_allows_save_dialog() {
        let probe = std::sync::Arc::new(Probe::default());
        *probe.surfaces.lock().unwrap() = vec![surface(100, None)];
        let mut runtime = Runtime {
            backend: Box::new(SyntheticApplication(probe.clone())),
            surfaces: Surfaces::new(),
            captures: BTreeMap::new(),
            retry: BTreeMap::new(),
            bitrate: 4_000_000,
            frames: tokio::sync::mpsc::channel(8).0,
        };
        let (events, mut received) = tokio::sync::mpsc::channel(64);
        runtime.refresh(&events).unwrap();
        received.try_recv().unwrap();
        runtime
            .command(
                WorkerCommand::Client(ApplicationMessage::Close {
                    surface_id: 0,
                    geometry_generation: 1,
                }),
                true,
            )
            .unwrap();
        assert!(runtime.surfaces.registry.get(0).is_some());
        assert!(
            received.try_recv().is_err(),
            "Close must not synthesize Remove"
        );
        probe.surfaces.lock().unwrap().push(surface(200, Some(100)));
        runtime.refresh(&events).unwrap();
        assert!(matches!(received.try_recv(), Ok(WorkerEvent::Metadata(
            ApplicationMessage::SurfaceUpsert { surface })) if surface.modal && surface.parent_surface_id == Some(0)));
        assert_eq!(runtime.captures.len(), 2);
    }
}
