//! Negotiated logical sessions over independently keyed native paths.
//!
//! Reliable records are retained until a cumulative *transport receive* ACK.
//! ACKs never mean that an application processed input or presented a frame.
//! Each reliable channel has its own bounded writer; video never occupies one.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use ndp_proto::{Channel, MsgFlags, MsgHeader, MsgKind};
use tokio::sync::{mpsc, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::{FrameOutcome, Incoming, Result, Session, SessionReceiver, TransportError};

const MAGIC: &[u8; 4] = b"NMP1";
const DATA: u8 = 0;
const PROBE: u8 = 1;
const ECHO: u8 = 2;
const ACK: u8 = 3;
pub(crate) const ENVELOPE_LEN: usize = 21;
const RELIABLE: [Channel; 4] = [
    Channel::Control,
    Channel::Input,
    Channel::Clipboard,
    Channel::File,
];
// Below the native unordered replay window (64); even a slow first transfer
// cannot be overtaken by an unlimited number of later native records.
const WINDOW: usize = 32;
const BYTE_WINDOW: usize = 32 * 1024 * 1024;
const PROBE_INTERVAL: Duration = Duration::from_millis(500);
const PATH_TIMEOUT: Duration = Duration::from_millis(2500);
// Accepting the responder's Noise reply into the relay-facing QUIC hop does
// not mean the initiator has received it or installed its multipath readers.
const RELAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RELAY_TIMEOUT: Duration = Duration::from_secs(15);
const COOLDOWN: Duration = Duration::from_secs(5);
const CODE_PATH_FAILED: u32 = 0x004d_5001;
const CODE_LOGICAL_CLOSE: u64 = 0x4d50_0000_0000;

/// The carrier currently selected for new logical messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// The gateway-authorized relay connection.
    Relay,
    /// A separately authenticated client-to-agent connection.
    Direct,
}

impl PathKind {
    fn index(self) -> usize {
        match self {
            Self::Relay => 0,
            Self::Direct => 1,
        }
    }
}

/// A media discontinuity notification, not a delivery acknowledgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathState {
    /// The selected outbound route.
    pub kind: PathKind,
    /// Monotonically increasing notification generation. Also advances when
    /// the peer starts a new media epoch, so asymmetric switches refresh both
    /// decoder reference chains without restarting the logical session.
    pub epoch: u64,
}

/// A read-only diagnostic snapshot of one retained native path.
///
/// UDP counters include QUIC framing, retransmissions, and keepalives; they
/// measure this QUIC hop, not application bytes or frame presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathStats {
    /// Whether this is the relay or direct carrier.
    pub kind: PathKind,
    /// Unique path generation within this logical session. Counters restart
    /// when a replacement direct connection receives a new `path_id`.
    pub path_id: u64,
    /// Whether new outbound application records currently select this path.
    pub active: bool,
    /// Cumulative UDP bytes transmitted by the native QUIC connection.
    pub udp_tx_bytes: u64,
    /// Cumulative UDP bytes received by the native QUIC connection.
    pub udp_rx_bytes: u64,
    /// Smoothed authenticated end-to-end echo RTT. Unlike QUIC's hop RTT,
    /// this includes both relay hops. `None` until an echo is measured.
    pub end_to_end_rtt: Option<Duration>,
}

struct Path {
    id: u64,
    kind: PathKind,
    native: Session,
    peer_ready: bool,
    last_echo: Instant,
    last_probe: Option<Instant>,
    probes: VecDeque<(u64, Instant)>,
    rtt: Option<Duration>,
    samples: u32,
}

impl Path {
    fn new(id: u64, kind: PathKind, native: Session) -> Self {
        Self {
            id,
            kind,
            native,
            peer_ready: false,
            last_echo: Instant::now(),
            last_probe: None,
            probes: VecDeque::new(),
            rtt: None,
            samples: 0,
        }
    }
}

struct Routes {
    paths: [Option<Path>; 2],
    active: PathKind,
    tx_epoch: u64,
    changed_at: Instant,
    next_id: u64,
    next_probe: u64,
    direct_after: Instant,
}

#[derive(Clone)]
struct Selected {
    id: u64,
    kind: PathKind,
    epoch: u64,
    native: Session,
}

struct Shared {
    routes: Mutex<Routes>,
    changes: watch::Sender<PathState>,
    shutdown: watch::Sender<bool>,
    terminal: Mutex<Option<TransportError>>,
    acknowledged: [AtomicU64; 6],
    allocated: [AtomicU64; 6],
    received: [AtomicU64; 6],
    ack_changed: [Notify; 6],
    events: mpsc::Sender<Event>,
    max_record: usize,
}

impl Shared {
    fn selected(&self) -> Option<Selected> {
        let routes = self.routes.lock().unwrap();
        routes.paths[routes.active.index()]
            .as_ref()
            .map(|p| Selected {
                id: p.id,
                kind: p.kind,
                epoch: routes.tx_epoch,
                native: p.native.clone(),
            })
    }

    fn publish(&self, kind: PathKind) {
        self.changes.send_modify(|state| {
            state.kind = kind;
            state.epoch = state.epoch.saturating_add(1);
        });
    }

    fn switch(&self, routes: &mut Routes, kind: PathKind) {
        if routes.active == kind {
            return;
        }
        routes.active = kind;
        routes.tx_epoch += 1;
        routes.changed_at = Instant::now();
        debug!(
            ?kind,
            epoch = routes.tx_epoch,
            "logical session route changed"
        );
        self.publish(kind);
    }

    fn stop(&self, error: TransportError, reason: &[u8]) {
        self.stop_with_code(error, 0, reason);
    }

    fn stop_with_code(&self, error: TransportError, code: u32, reason: &[u8]) {
        let mut routes = self.routes.lock().unwrap();
        if *self.shutdown.borrow() {
            return;
        }
        *self.terminal.lock().unwrap() = Some(error);
        for path in &mut routes.paths {
            if let Some(path) = path.take() {
                path.native.close_native_code(
                    quinn::VarInt::from_u64(CODE_LOGICAL_CLOSE | u64::from(code)).unwrap(),
                    reason,
                );
            }
        }
        self.shutdown.send_replace(true);
    }

    fn fail_path(&self, id: u64, error: TransportError) {
        if matches!(&error, TransportError::Connection(quinn::ConnectionError::ApplicationClosed(close))
            if close.error_code.into_inner() & 0xffff_ffff_0000_0000 == CODE_LOGICAL_CLOSE)
        {
            self.stop(error, b"peer closed logical session");
            return;
        }
        // Native replay rejection remains intact. A late independently
        // streamed retransmission can fall outside its native replay window;
        // the logical sender will retry it with fresh native sealing.
        if matches!(
            &error,
            TransportError::Crypto(ndp_crypto::CryptoError::Replay {
                channel: "clipboard" | "file",
                ..
            })
        ) {
            debug!(id, %error, "discarded late native reliable attempt");
            return;
        }
        if matches!(
            error,
            TransportError::Crypto(_)
                | TransportError::Proto(_)
                | TransportError::WrongCarrier { .. }
                | TransportError::UnknownChannel(_)
                | TransportError::MultipathProtocol(_)
                | TransportError::RecordTooLarge { .. }
        ) {
            warn!(id, %error, "authenticated path protocol failure");
            self.stop(error, b"multipath protocol failure");
            return;
        }
        let mut routes = self.routes.lock().unwrap();
        let Some(index) = routes
            .paths
            .iter()
            .position(|p| p.as_ref().is_some_and(|p| p.id == id))
        else {
            return;
        };
        let path = routes.paths[index].take().unwrap();
        warn!(
            id,
            kind = ?path.kind,
            %error,
            peer_ready = path.peer_ready,
            echo_age_ms = path.last_echo.elapsed().as_millis() as u64,
            probe_samples = path.samples,
            end_to_end_rtt_ms = ?path.rtt.map(|rtt| rtt.as_secs_f64() * 1000.0),
            "retiring failed native path"
        );
        path.native
            .close_native(CODE_PATH_FAILED, b"path unavailable");
        if path.kind == PathKind::Direct {
            routes.direct_after = Instant::now() + COOLDOWN;
        }
        if routes.active == path.kind {
            let other = if path.kind == PathKind::Direct {
                PathKind::Relay
            } else {
                PathKind::Direct
            };
            if routes.paths[other.index()].is_some() {
                self.switch(&mut routes, other);
            } else {
                drop(routes);
                self.stop(error, b"all paths unavailable");
            }
        }
    }

    fn acks(&self, out: &mut Vec<u8>) {
        for channel in RELIABLE {
            out.extend_from_slice(
                &self.received[channel.id() as usize]
                    .load(Ordering::Acquire)
                    .to_le_bytes(),
            );
        }
    }

    fn read_acks(&self, bytes: &[u8]) -> Result<()> {
        if bytes.len() != RELIABLE.len() * 8 {
            return Err(TransportError::MultipathProtocol(
                "invalid acknowledgment length",
            ));
        }
        for (channel, bytes) in RELIABLE.into_iter().zip(bytes.chunks_exact(8)) {
            let next = u64::from_le_bytes(bytes.try_into().unwrap());
            let index = channel.id() as usize;
            if next > self.allocated[index].load(Ordering::Acquire) {
                return Err(TransportError::MultipathProtocol(
                    "acknowledgment beyond allocated sequence",
                ));
            }
            if self.acknowledged[index].fetch_max(next, Ordering::AcqRel) < next {
                self.ack_changed[index].notify_one();
            }
        }
        Ok(())
    }

    fn internal(&self, id: u64, kind: u8, nonce: Option<u64>) {
        let native = {
            let routes = self.routes.lock().unwrap();
            routes
                .paths
                .iter()
                .flatten()
                .find(|p| p.id == id)
                .map(|p| p.native.clone())
        };
        let Some(native) = native else { return };
        let mut payload = Vec::with_capacity(45);
        payload.extend_from_slice(MAGIC);
        payload.push(kind);
        if let Some(nonce) = nonce {
            payload.extend_from_slice(&nonce.to_le_bytes());
        }
        self.acks(&mut payload);
        if let Err(error) =
            native.send_datagram_native(MsgHeader::new(MsgKind::Ping, 0, 0), &payload)
        {
            self.fail_path(id, error);
        }
    }

    fn peer_ready(&self, id: u64) {
        let mut routes = self.routes.lock().unwrap();
        let Some(path) = routes.paths.iter_mut().flatten().find(|path| path.id == id) else {
            return;
        };
        if !path.peer_ready {
            path.peer_ready = true;
            path.last_echo = Instant::now();
            // Echoes queued while the application was starting are readiness
            // evidence, not useful RTT samples. Start fresh probes now.
            path.probes.clear();
            path.last_probe = None;
            info!(id, kind = ?path.kind, "authenticated multipath peer is ready");
        }
    }

    fn heartbeat(&self) {
        let now = Instant::now();
        let mut probes = Vec::new();
        let mut expired = Vec::new();
        {
            let mut routes = self.routes.lock().unwrap();
            for index in 0..2 {
                let nonce = routes.next_probe;
                routes.next_probe = routes.next_probe.wrapping_add(1);
                let Some(path) = &mut routes.paths[index] else {
                    continue;
                };
                let timeout = probe_timeout(path.kind, path.peer_ready, path.rtt);
                let elapsed = now.duration_since(path.last_echo);
                if elapsed >= timeout {
                    warn!(
                        id = path.id,
                        kind = ?path.kind,
                        reason = if path.peer_ready { "unanswered_echo" } else { "awaiting_peer_readiness" },
                        elapsed_ms = elapsed.as_millis() as u64,
                        deadline_ms = timeout.as_millis() as u64,
                        probe_samples = path.samples,
                        "multipath probe deadline expired"
                    );
                    expired.push(path.id);
                    continue;
                }
                if path
                    .last_probe
                    .is_none_or(|last| now.duration_since(last) >= PROBE_INTERVAL)
                {
                    path.last_probe = Some(now);
                    path.probes.push_back((nonce, now));
                    while path.probes.len() > 8 {
                        path.probes.pop_front();
                    }
                    probes.push((path.id, nonce));
                }
            }
        }
        for id in expired {
            self.fail_path(
                id,
                TransportError::Connection(quinn::ConnectionError::TimedOut),
            );
        }
        for (id, nonce) in probes {
            self.internal(id, PROBE, Some(nonce));
        }
    }

    fn echo(&self, id: u64, nonce: u64) {
        let mut routes = self.routes.lock().unwrap();
        let Some(path) = routes.paths.iter_mut().flatten().find(|p| p.id == id) else {
            return;
        };
        let Some(index) = path.probes.iter().position(|(sent, _)| *sent == nonce) else {
            return;
        };
        let (_, sent) = path.probes.remove(index).unwrap();
        let now = Instant::now();
        let sample = now.duration_since(sent);
        path.last_echo = now;
        path.rtt = Some(path.rtt.map_or(sample, |old| (old * 3 + sample) / 4));
        path.samples = path.samples.saturating_add(1);
        if let (Some(relay), Some(direct)) = (&routes.paths[0], &routes.paths[1]) {
            if relay.samples >= 3 && direct.samples >= 3 {
                let relay_rtt = relay.rtt.unwrap();
                let direct_rtt = direct.rtt.unwrap();
                let target = if faster(direct_rtt, relay_rtt) {
                    Some(PathKind::Direct)
                } else if faster(relay_rtt, direct_rtt) {
                    Some(PathKind::Relay)
                } else {
                    None
                };
                // The first direct upgrade need not wait through a cooldown.
                if let Some(target) = target {
                    if routes.tx_epoch == 0 || now.duration_since(routes.changed_at) >= COOLDOWN {
                        if routes.active != target {
                            debug!(
                                ?target,
                                relay_rtt_ms = relay_rtt.as_secs_f64() * 1000.0,
                                direct_rtt_ms = direct_rtt.as_secs_f64() * 1000.0,
                                "end-to-end probes selected a faster path"
                            );
                        }
                        self.switch(&mut routes, target);
                    }
                }
            }
        }
    }
}

fn probe_timeout(kind: PathKind, peer_ready: bool, rtt: Option<Duration>) -> Duration {
    match (kind, peer_ready, rtt) {
        (PathKind::Relay, false, _) => RELAY_STARTUP_TIMEOUT,
        (PathKind::Relay, true, Some(rtt)) => {
            (rtt.saturating_mul(4) + PROBE_INTERVAL).clamp(PATH_TIMEOUT, MAX_RELAY_TIMEOUT)
        }
        // In particular, a selected direct blackhole still expires in 2.5 s.
        _ => PATH_TIMEOUT,
    }
}

fn faster(candidate: Duration, current: Duration) -> bool {
    candidate.mul_f64(1.2) + Duration::from_millis(2) < current
}

enum Event {
    Received(u64, Incoming),
    Failed(u64, TransportError),
}

struct Request {
    header: MsgHeader,
    payload: Vec<u8>,
    written: oneshot::Sender<Result<()>>,
    _count: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

struct Outbox {
    tx: mpsc::Sender<Request>,
    count: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
}

pub(crate) struct Multipath {
    shared: Arc<Shared>,
    outboxes: [Option<Outbox>; 6],
    tasks: Mutex<Vec<AbortHandle>>,
}

impl std::fmt::Debug for Multipath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Multipath")
            .field("path", &*self.shared.changes.borrow())
            .finish()
    }
}

impl Drop for Multipath {
    fn drop(&mut self) {
        self.shared
            .stop(TransportError::Closed, b"logical session dropped");
        for task in self.tasks.get_mut().unwrap() {
            task.abort();
        }
    }
}

impl Multipath {
    pub(crate) fn start(
        native: Session,
        receiver: SessionReceiver,
    ) -> (Arc<Self>, SessionReceiver) {
        let (events, incoming) = mpsc::channel(128);
        let (tx, rx) = mpsc::channel(64);
        let (changes, _) = watch::channel(PathState {
            kind: PathKind::Relay,
            epoch: 0,
        });
        let (shutdown, _) = watch::channel(false);
        let max_record = native.native_limit();
        let shared = Arc::new(Shared {
            routes: Mutex::new(Routes {
                paths: [Some(Path::new(0, PathKind::Relay, native.clone())), None],
                active: PathKind::Relay,
                tx_epoch: 0,
                changed_at: Instant::now(),
                next_id: 1,
                next_probe: 0,
                direct_after: Instant::now(),
            }),
            changes,
            shutdown,
            terminal: Mutex::new(None),
            acknowledged: std::array::from_fn(|_| AtomicU64::new(0)),
            allocated: std::array::from_fn(|_| AtomicU64::new(0)),
            received: std::array::from_fn(|_| AtomicU64::new(0)),
            ack_changed: std::array::from_fn(|_| Notify::new()),
            events,
            max_record,
        });
        let mut tasks =
            vec![tokio::spawn(receive_loop(shared.clone(), incoming, tx)).abort_handle()];
        let outboxes = std::array::from_fn(|index| {
            let channel = Channel::ALL[index];
            if !RELIABLE.contains(&channel) {
                return None;
            }
            let (tx, rx) = mpsc::channel(WINDOW);
            tasks.push(tokio::spawn(reliable_writer(shared.clone(), channel, rx)).abort_handle());
            Some(Outbox {
                tx,
                count: Arc::new(Semaphore::new(WINDOW)),
                bytes: Arc::new(Semaphore::new(BYTE_WINDOW)),
            })
        });
        let multipath = Arc::new(Self {
            shared,
            outboxes,
            tasks: Mutex::new(tasks),
        });
        multipath.forward(0, native.clone(), receiver);
        if !supports_probes(&native) {
            multipath.shared.stop(
                TransportError::Config("multipath requires QUIC datagrams".into()),
                b"datagrams required",
            );
        }
        (
            multipath.clone(),
            SessionReceiver {
                rx,
                _multipath: Some(multipath),
            },
        )
    }

    fn forward(&self, id: u64, native: Session, mut receiver: SessionReceiver) {
        let shared = self.shared.clone();
        let mut shutdown = shared.shutdown.subscribe();
        let task = tokio::spawn(async move {
            let connection = native.native_connection();
            loop {
                let event = tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    error = connection.closed() => Event::Failed(id, error.into()),
                    message = receiver.recv() => match message {
                        Some(Ok(message)) => Event::Received(id, message),
                        Some(Err(error)) => Event::Failed(id, error),
                        None => Event::Failed(id, TransportError::Closed),
                    }
                };
                let closed = connection.close_reason().is_some();
                tokio::select! {
                    _ = shutdown.changed() => break,
                    result = shared.events.send(event) => if result.is_err() { break; },
                }
                if closed {
                    break;
                }
            }
        });
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task.abort_handle());
    }

    pub(crate) fn attach(&self, native: Session, receiver: SessionReceiver) -> Result<()> {
        let reject = |reason: &str| {
            native.close_native(CODE_PATH_FAILED, reason.as_bytes());
            TransportError::Rejected {
                reason: reason.into(),
            }
        };
        let mut routes = self.shared.routes.lock().unwrap();
        if routes.paths.iter().flatten().any(|p| {
            p.native.native_connection().stable_id() == native.native_connection().stable_id()
        }) {
            // Do not close an aliased existing path, even on a rejected attach.
            return Err(TransportError::Rejected {
                reason: "direct path reuses an existing connection".into(),
            });
        }
        if native.is_multipath() || !supports_probes(&native) {
            return Err(reject("direct path must be native and support datagrams"));
        }
        if native.native_connection().close_reason().is_some() {
            return Err(reject("direct path is already closed"));
        }
        let identity = routes
            .paths
            .iter()
            .flatten()
            .next()
            .and_then(|p| p.native.peer_static());
        if identity.is_none() || identity != native.peer_static() {
            return Err(reject(
                "direct path peer identity does not match logical session",
            ));
        }
        if *self.shared.shutdown.borrow() {
            return Err(reject("logical session closed"));
        }
        if routes.paths[1].is_some() || Instant::now() < routes.direct_after {
            return Err(reject("direct path already attached or cooling down"));
        }
        if native.native_limit() < self.shared.max_record {
            return Err(reject("direct path record limit is smaller than relay"));
        }
        let id = routes.next_id;
        routes.next_id += 1;
        routes.paths[1] = Some(Path::new(id, PathKind::Direct, native.clone()));
        drop(routes);
        self.forward(id, native, receiver);
        Ok(())
    }

    pub(crate) fn path_changes(&self) -> watch::Receiver<PathState> {
        self.shared.changes.subscribe()
    }

    pub(crate) fn disconnect_direct(&self) -> Result<()> {
        let id = self.shared.routes.lock().unwrap().paths[PathKind::Direct.index()]
            .as_ref()
            .map(|path| path.id)
            .ok_or(TransportError::NoDirectPath)?;
        self.shared.fail_path(id, TransportError::Closed);
        Ok(())
    }

    pub(crate) fn connection(&self) -> Option<quinn::Connection> {
        self.shared.selected().map(|p| p.native.native_connection())
    }

    pub(crate) fn path_stats(&self) -> Vec<PathStats> {
        let routes = self.shared.routes.lock().unwrap();
        routes
            .paths
            .iter()
            .flatten()
            .map(|path| {
                let stats = path.native.native_connection().stats();
                PathStats {
                    kind: path.kind,
                    path_id: path.id,
                    active: routes.active == path.kind,
                    udp_tx_bytes: stats.udp_tx.bytes,
                    udp_rx_bytes: stats.udp_rx.bytes,
                    end_to_end_rtt: path.rtt,
                }
            })
            .collect()
    }

    pub(crate) fn close(&self, code: u32, reason: &[u8]) {
        debug!(code, "closing logical multipath session");
        self.shared
            .stop_with_code(TransportError::Closed, code, reason);
    }

    fn check_len(&self, payload: &[u8]) -> Result<()> {
        crate::wire::check_len(
            payload
                .len()
                .saturating_add(ENVELOPE_LEN + ndp_proto::HEADER_LEN + ndp_crypto::AEAD_TAG_LEN),
            self.shared.max_record,
        )
    }

    fn lossy_sequence(&self, channel: Channel) -> Result<u64> {
        self.shared.allocated[channel.id() as usize]
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| TransportError::MultipathProtocol("logical sequence exhausted"))
    }

    pub(crate) async fn send(
        &self,
        channel: Channel,
        header: MsgHeader,
        payload: &[u8],
    ) -> Result<()> {
        self.check_len(payload)?;
        if channel == Channel::Video {
            self.send_video(header, payload, None).await?;
            return Ok(());
        }
        if channel == Channel::Audio {
            let selected = self.shared.selected().ok_or(TransportError::Closed)?;
            let sequence = self.lossy_sequence(channel)?;
            let bytes = envelope(sequence, selected.epoch, payload);
            return match selected.native.send_datagram_native(header, &bytes) {
                Ok(()) => Ok(()),
                Err(error @ TransportError::Datagram(quinn::SendDatagramError::TooLarge)) => {
                    Err(error)
                }
                Err(error) => {
                    self.shared.fail_path(selected.id, error);
                    if *self.shared.shutdown.borrow() {
                        Err(TransportError::Closed)
                    } else {
                        Ok(())
                    }
                }
            };
        }
        let outbox = self.outboxes[channel.id() as usize].as_ref().unwrap();
        crate::wire::check_len(payload.len().saturating_add(ENVELOPE_LEN), BYTE_WINDOW)?;
        let mut shutdown = self.shared.shutdown.subscribe();
        if *shutdown.borrow() {
            return Err(TransportError::Closed);
        }
        let enqueue = async {
            let count = outbox
                .count
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| TransportError::Closed)?;
            let bytes = outbox
                .bytes
                .clone()
                .acquire_many_owned((payload.len() + ENVELOPE_LEN) as u32)
                .await
                .map_err(|_| TransportError::Closed)?;
            let (written, received) = oneshot::channel();
            outbox
                .tx
                .send(Request {
                    header,
                    payload: payload.to_vec(),
                    written,
                    _count: count,
                    _bytes: bytes,
                })
                .await
                .map_err(|_| TransportError::Closed)?;
            received.await.map_err(|_| TransportError::Closed)?
        };
        tokio::select! {
            _ = shutdown.changed() => Err(TransportError::Closed),
            result = enqueue => result,
        }
    }

    pub(crate) async fn send_video(
        &self,
        header: MsgHeader,
        payload: &[u8],
        deadline: Option<Instant>,
    ) -> Result<FrameOutcome> {
        self.check_len(payload)?;
        let mut changes = self.shared.changes.subscribe();
        let mut shutdown = self.shared.shutdown.subscribe();
        let selected = self.shared.selected().ok_or(TransportError::Closed)?;
        let sequence = self.lossy_sequence(Channel::Video)?;
        let bytes = envelope(sequence, selected.epoch, payload);
        let sending = selected.native.send_video_native(header, &bytes, deadline);
        tokio::pin!(sending);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => return Err(TransportError::Closed),
                _ = changes.changed() => {
                    if self.shared.selected().is_none_or(|p| p.id != selected.id || p.epoch != selected.epoch) {
                        if header.flags.contains(MsgFlags::KEYFRAME) {
                            warn!(
                                path_id = selected.id,
                                kind = ?selected.kind,
                                logical_seq = sequence,
                                frame_epoch = selected.epoch,
                                bytes = payload.len(),
                                "discarding keyframe send after outbound route changed"
                            );
                        }
                        return Ok(FrameOutcome::Discarded);
                    }
                }
                result = &mut sending => return match result {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        self.shared.fail_path(selected.id, error);
                        if *self.shared.shutdown.borrow() { Err(TransportError::Closed) } else { Ok(FrameOutcome::Discarded) }
                    }
                },
            }
        }
    }
}

fn supports_probes(native: &Session) -> bool {
    native
        .max_audio_record()
        .is_some_and(|n| n >= 45 + ndp_proto::HEADER_LEN + ndp_crypto::AEAD_TAG_LEN)
}

fn envelope(sequence: u64, epoch: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENVELOPE_LEN + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(DATA);
    out.extend_from_slice(&sequence.to_le_bytes());
    out.extend_from_slice(&epoch.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

struct Retained {
    header: MsgHeader,
    payload: Vec<u8>,
    _count: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
}

struct Pending {
    sequence: u64,
    record: Arc<Retained>,
    written: Option<oneshot::Sender<Result<()>>>,
    attempts: [Option<(u64, u64)>; 2],
}

type WriteResult = (u64, u64, Result<()>);

async fn reliable_writer(
    shared: Arc<Shared>,
    channel: Channel,
    mut requests: mpsc::Receiver<Request>,
) {
    let index = channel.id() as usize;
    let mut pending: VecDeque<Pending> = VecDeque::new();
    let mut sending: FuturesUnordered<BoxFuture<'static, WriteResult>> = FuturesUnordered::new();
    let mut busy = Vec::new();
    let mut route_changes = shared.changes.subscribe();
    let mut shutdown = shared.shutdown.subscribe();
    loop {
        if *shutdown.borrow() {
            return;
        }
        let acknowledged = shared.acknowledged[index].load(Ordering::Acquire);
        while pending.front().is_some_and(|p| p.sequence < acknowledged) {
            if let Some(written) = pending.pop_front().unwrap().written.take() {
                let _ = written.send(Ok(()));
            }
        }
        if let Some(path) = shared.selected() {
            // A live old-path write is allowed to finish. Never cancel a
            // partial ordered record merely because the preferred route moved.
            if !busy.contains(&path.id) && busy.len() < 2 {
                if let Some(record) = pending.iter_mut().find(|p| {
                    p.attempts[path.kind.index()]
                        .is_none_or(|(id, epoch)| id != path.id || epoch != path.epoch)
                }) {
                    // QUIC already retransmits on a live path. Re-seal only
                    // after a route epoch changes; periodic probe ACKs repair
                    // lost ACK datagrams without flooding a stalled consumer.
                    record.attempts[path.kind.index()] = Some((path.id, path.epoch));
                    let retained = record.record.clone();
                    let sequence = record.sequence;
                    busy.push(path.id);
                    sending.push(Box::pin(async move {
                        let bytes = envelope(sequence, path.epoch, &retained.payload);
                        let result = path
                            .native
                            .send_native(channel, retained.header, &bytes)
                            .await;
                        (path.id, sequence, result)
                    }));
                }
            }
        }
        tokio::select! {
            biased;
            _ = shutdown.changed() => return,
            Some((path, sequence, result)) = sending.next(), if !sending.is_empty() => {
                busy.retain(|id| *id != path);
                match result {
                    Ok(()) => {
                        if let Some(record) = pending.iter_mut().find(|p| p.sequence == sequence) {
                            if let Some(written) = record.written.take() {
                                let _ = written.send(Ok(()));
                            }
                        }
                    }
                    Err(error) => shared.fail_path(path, error),
                }
            }
            _ = shared.ack_changed[index].notified() => {},
            _ = route_changes.changed() => {},
            Some(request) = requests.recv(), if pending.len() < WINDOW => {
                let sequence = shared.allocated[index].load(Ordering::Acquire);
                let Some(next) = sequence.checked_add(1) else {
                    shared.stop(TransportError::MultipathProtocol("logical sequence exhausted"), b"sequence exhausted");
                    return;
                };
                shared.allocated[index].store(next, Ordering::Release);
                pending.push_back(Pending {
                    sequence,
                    record: Arc::new(Retained {
                        header: request.header, payload: request.payload, _count: request._count, _bytes: request._bytes,
                    }),
                    written: Some(request.written),
                    attempts: [None, None],
                });
            }
        }
    }
}

#[derive(Default)]
struct LossyWindow {
    highest: Option<u64>,
    bits: u64,
}

impl LossyWindow {
    fn accept(&mut self, sequence: u64) -> bool {
        match self.highest {
            None => {
                self.highest = Some(sequence);
                self.bits = 1;
                true
            }
            Some(highest) if sequence > highest => {
                let advance = sequence - highest;
                self.bits = if advance >= 64 {
                    1
                } else {
                    (self.bits << advance) | 1
                };
                self.highest = Some(sequence);
                true
            }
            Some(highest) => {
                let behind = highest - sequence;
                if behind >= 64 || self.bits & (1 << behind) != 0 {
                    return false;
                }
                self.bits |= 1 << behind;
                true
            }
        }
    }
}

struct Inbox {
    pending: [BTreeMap<u64, Incoming>; 6],
    bytes: usize,
    lossy: [LossyWindow; 2],
    media_epoch: u64,
    cursor: usize,
}

impl Inbox {
    fn new() -> Self {
        Self {
            pending: std::array::from_fn(|_| BTreeMap::new()),
            bytes: 0,
            lossy: std::array::from_fn(|_| LossyWindow::default()),
            media_epoch: 0,
            cursor: 0,
        }
    }

    fn ready(&self, shared: &Shared) -> Option<usize> {
        (0..6)
            .map(|offset| (self.cursor + offset) % 6)
            .find(|index| {
                self.pending[*index].contains_key(&shared.received[*index].load(Ordering::Acquire))
            })
    }

    fn deliver(
        &mut self,
        index: usize,
        shared: &Shared,
        permit: mpsc::Permit<'_, Result<Incoming>>,
    ) {
        let next = shared.received[index].load(Ordering::Acquire);
        let message = self.pending[index].remove(&next).unwrap();
        self.bytes -= message.payload.len();
        permit.send(Ok(message));
        shared.received[index].store(next + 1, Ordering::Release);
        self.cursor = (index + 1) % 6;
        // Send on both live paths: loss or failure of the arrival path must
        // not prevent retiring the sender's retained logical record.
        let ids: Vec<_> = shared
            .routes
            .lock()
            .unwrap()
            .paths
            .iter()
            .flatten()
            .map(|p| p.id)
            .collect();
        for id in ids {
            shared.internal(id, ACK, None);
        }
    }

    fn receive(
        &mut self,
        shared: &Shared,
        id: u64,
        mut message: Incoming,
        output: &mpsc::Sender<Result<Incoming>>,
    ) -> Result<()> {
        let payload = &message.payload;
        if payload.len() < 5 || &payload[..4] != MAGIC {
            return Err(TransportError::MultipathProtocol(
                "missing multipath/1 envelope",
            ));
        }
        match payload[4] {
            DATA => {
                if payload.len() < ENVELOPE_LEN {
                    return Err(TransportError::MultipathProtocol("truncated data envelope"));
                }
                let sequence = u64::from_le_bytes(payload[5..13].try_into().unwrap());
                let epoch = u64::from_le_bytes(payload[13..21].try_into().unwrap());
                shared.peer_ready(id);
                message.header.seq = sequence as u32;
                message.payload.drain(..ENVELOPE_LEN);
                if RELIABLE.contains(&message.channel) {
                    let index = message.channel.id() as usize;
                    let next = shared.received[index].load(Ordering::Acquire);
                    if sequence < next {
                        shared.internal(id, ACK, None);
                        return Ok(());
                    }
                    if sequence == u64::MAX || sequence - next >= WINDOW as u64 {
                        return Err(TransportError::MultipathProtocol(
                            "logical receive window exceeded",
                        ));
                    }
                    if let Some(previous) = self.pending[index].get(&sequence) {
                        if previous.header != message.header || previous.payload != message.payload
                        {
                            return Err(TransportError::MultipathProtocol(
                                "conflicting logical retransmission",
                            ));
                        }
                    } else if self.bytes + message.payload.len() <= BYTE_WINDOW * RELIABLE.len() {
                        self.bytes += message.payload.len();
                        self.pending[index].insert(sequence, message);
                    }
                } else {
                    if epoch < self.media_epoch {
                        if message.header.flags.contains(MsgFlags::KEYFRAME) {
                            warn!(
                                path_id = id,
                                logical_seq = sequence,
                                frame_epoch = epoch,
                                receive_epoch = self.media_epoch,
                                bytes = message.payload.len(),
                                "discarding keyframe from superseded media epoch"
                            );
                        }
                        return Ok(());
                    }
                    if epoch > self.media_epoch {
                        self.media_epoch = epoch;
                        let routes = shared.routes.lock().unwrap();
                        shared.publish(routes.active);
                    }
                    let index = usize::from(message.channel == Channel::Audio);
                    if self.lossy[index].accept(sequence) {
                        // Media is never replayed or allowed to block path
                        // liveness/revocation behind a stalled application.
                        if let Err(mpsc::error::TrySendError::Full(Ok(message))) =
                            output.try_send(Ok(message))
                        {
                            if message.header.flags.contains(MsgFlags::KEYFRAME) {
                                warn!(
                                    path_id = id,
                                    logical_seq = sequence,
                                    frame_epoch = epoch,
                                    bytes = message.payload.len(),
                                    "discarding keyframe because application receive queue is full"
                                );
                            }
                        }
                    } else if message.header.flags.contains(MsgFlags::KEYFRAME) {
                        warn!(
                            path_id = id,
                            logical_seq = sequence,
                            frame_epoch = epoch,
                            bytes = message.payload.len(),
                            "discarding duplicate or stale logical keyframe"
                        );
                    }
                }
                Ok(())
            }
            kind @ (PROBE | ECHO | ACK) => {
                if message.channel != Channel::Audio || message.header.kind != MsgKind::Ping {
                    return Err(TransportError::MultipathProtocol(
                        "wrong internal-message carrier",
                    ));
                }
                let offset = if kind == ACK { 5 } else { 13 };
                if payload.len() != offset + RELIABLE.len() * 8 {
                    return Err(TransportError::MultipathProtocol(
                        "invalid internal-message length",
                    ));
                }
                shared.read_acks(&payload[offset..])?;
                shared.peer_ready(id);
                if kind != ACK {
                    let nonce = u64::from_le_bytes(payload[5..13].try_into().unwrap());
                    if kind == PROBE {
                        shared.internal(id, ECHO, Some(nonce));
                    } else {
                        shared.echo(id, nonce);
                    }
                }
                Ok(())
            }
            _ => Err(TransportError::MultipathProtocol("unknown envelope type")),
        }
    }
}

async fn receive_loop(
    shared: Arc<Shared>,
    mut events: mpsc::Receiver<Event>,
    output: mpsc::Sender<Result<Incoming>>,
) {
    let mut inbox = Inbox::new();
    let mut shutdown = shared.shutdown.subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_millis(100));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *shutdown.borrow() {
            let error = shared
                .terminal
                .lock()
                .unwrap()
                .take()
                .unwrap_or(TransportError::Closed);
            let _ = output.send(Err(error)).await;
            return;
        }
        let ready = inbox.ready(&shared);
        tokio::select! {
            biased;
            _ = shutdown.changed() => {},
            _ = output.closed() => {
                shared.stop(TransportError::Closed, b"logical receiver dropped");
                return;
            }
            _ = heartbeat.tick() => shared.heartbeat(),
            permit = output.reserve(), if ready.is_some() => {
                if let Ok(permit) = permit {
                    inbox.deliver(ready.unwrap(), &shared, permit);
                }
            }
            event = events.recv() => match event {
                Some(Event::Received(id, message)) => {
                    if let Err(error) = inbox.receive(&shared, id, message, &output) {
                        shared.fail_path(id, error);
                    }
                }
                Some(Event::Failed(id, error)) => shared.fail_path(id, error),
                None => shared.stop(TransportError::Closed, b"path readers stopped"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
