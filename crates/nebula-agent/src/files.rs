//! Moving files between the two machines.
//!
//! The shape mirrors the clipboard: a pure state machine that decides what
//! goes on the wire, with the blocking filesystem work done on a dedicated
//! thread. The reasoning is the same too — reading a gigabyte off a disk is
//! not something to do on the executor that is carrying video.
//!
//! Three rules make a transfer safe to accept from a peer:
//!
//! * The name is validated before anything touches the filesystem, so a
//!   sender cannot name its way out of the download directory.
//! * Bytes land in a `.part` file and are published without replacement after the
//!   hash matches. A transfer that dies halfway leaves something obviously
//!   incomplete rather than a file that looks finished and isn't.
//! * Chunks are written where the sender says they belong, and the file is
//!   only finished once every byte of it is accounted for. Each chunk rides
//!   its own QUIC stream — that is the whole point of the transport — so
//!   they arrive in whatever order the network delivers them, and a receiver
//!   that insisted on sequence would be waiting for an ordering nobody
//!   promised. Nothing is trusted about where a chunk claims to go beyond
//!   its having to fit inside the announced size.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use ndp_proto::{FileAck, FileChunkHeader, FileOffer};

/// Largest file this will send or accept.
///
/// Not a protocol limit — a refusal to let one peer fill the other's disk
/// by accident.
pub const MAX_FILE: u64 = 8 << 30;

/// Bytes per chunk.
///
/// Sized to fill a QUIC stream without making each write a visible pause
/// in the loop that also has to answer acknowledgements.
pub const CHUNK: usize = 64 * 1024;

/// Bytes the receiver lets the sender put in flight.
pub const WINDOW: u32 = 4 * 1024 * 1024;

/// Acknowledge once this much has arrived since the last one.
const ACK_EVERY: u64 = WINDOW as u64 / 2;

/// Concurrent transfers accepted in each direction.
pub const MAX_ACTIVE: usize = 4;

/// How long the worker waits for traffic before looking for work to do.
const POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Also bounds waits for older peers that silently discard a failed transfer.
const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

// FileAck rejects zero windows on the wire. This impossible byte count means
// terminal receiver failure, never success; actual files are capped at MAX_FILE.
const FAILED_RECEIVED: u64 = u64::MAX;

/// A transfer id is unique within its direction, not across both peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferDirection {
    /// From this machine to the peer.
    Send,
    /// From the peer to this machine.
    Receive,
}

/// Lifecycle state reported by the local file engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferState {
    /// An outgoing offer was prepared or an incoming offer accepted.
    Offered,
    /// Bytes are moving; even all bytes sent still requires final verification.
    Transferring,
    /// The receiver verified and published the file, and acknowledged it for sends.
    Complete,
    /// The engine rejected or could not finish the transfer.
    Failed,
}

/// Latest local engine state; completion means the receiver verified and published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferUpdate {
    /// Sender-assigned id, unique within a direction.
    pub id: u64,
    /// The offered filename, or the local filename for a pre-offer failure.
    pub name: String,
    /// Direction relative to this machine.
    pub direction: TransferDirection,
    /// Bytes read for sending, or unique bytes written while receiving.
    pub transferred: u64,
    /// Expected file size; zero when a failed source could not be inspected.
    pub total: u64,
    /// Latest engine state, not an estimate based on bytes sent.
    pub state: TransferState,
    /// Local explanation for a failure.
    pub error: Option<String>,
}

// Bound the engine's coalescing buffer independently of the UI's receiver.
const MAX_UPDATES: usize = 256;
const TELEMETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn record_update(updates: &mut VecDeque<TransferUpdate>, update: TransferUpdate) {
    updates.retain(|old| old.id != update.id || old.direction != update.direction);
    if updates.len() == MAX_UPDATES {
        updates.pop_front();
    }
    updates.push_back(update);
}

fn failure_ack(transfer_id: u64) -> Action {
    Action::Ack(FileAck {
        transfer_id,
        received: FAILED_RECEIVED,
        window: WINDOW,
    })
}

/// Something to put on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Announce a file this side wants to send.
    Offer(FileOffer),
    /// Carry part of a file.
    Chunk(FileChunkHeader, Vec<u8>),
    /// Tell the sender how much more it may send.
    Ack(FileAck),
}

/// A file being sent.
struct Sending {
    file: File,
    offset: u64,
    size: u64,
    /// Highest offset the receiver has allowed.
    allowed: u64,
    finished: bool,
    last_progress: std::time::Instant,
    name: String,
}

impl Sending {
    fn update(&self, id: u64, state: TransferState, error: Option<String>) -> TransferUpdate {
        TransferUpdate {
            id,
            name: self.name.clone(),
            direction: TransferDirection::Send,
            transferred: self.offset,
            total: self.size,
            state,
            error,
        }
    }
}

/// A file being received.
struct Receiving {
    name: String,
    file: File,
    part: PathBuf,
    final_path: PathBuf,
    /// Byte ranges written so far, merged and in order.
    have: Ranges,
    acked: u64,
    size: u64,
    expected: String,
    modified_secs: Option<i64>,
    last_progress: std::time::Instant,
}

impl Receiving {
    fn update(&self, id: u64, state: TransferState, error: Option<String>) -> TransferUpdate {
        TransferUpdate {
            id,
            name: self.name.clone(),
            direction: TransferDirection::Receive,
            transferred: self.have.0.iter().map(|(start, end)| end - start).sum(),
            total: self.size,
            state,
            error,
        }
    }
}

/// The set of byte ranges that have arrived.
///
/// Kept merged so that "how much is contiguous from zero" — the only number
/// the sender's window is allowed to depend on — is the first entry.
#[derive(Debug, Default)]
struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    /// Record `[start, end)`, merging with anything it touches.
    fn add(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let mut merged = Vec::with_capacity(self.0.len() + 1);
        let (mut start, mut end) = (start, end);
        for &(s, e) in &self.0 {
            if e < start || s > end {
                merged.push((s, e));
            } else {
                start = start.min(s);
                end = end.max(e);
            }
        }
        let at = merged.partition_point(|&(s, _)| s < start);
        merged.insert(at, (start, end));
        self.0 = merged;
    }

    /// How many bytes are present contiguously from the start of the file.
    fn contiguous(&self) -> u64 {
        match self.0.first() {
            Some(&(0, end)) => end,
            _ => 0,
        }
    }

    /// Whether every byte up to `size` has arrived.
    fn complete(&self, size: u64) -> bool {
        self.contiguous() >= size
    }
}

/// Both halves of file transfer for one session.
pub struct FileTransfers {
    enabled: bool,
    downloads: PathBuf,
    next_id: u64,
    sending: HashMap<u64, Sending>,
    receiving: HashMap<u64, Receiving>,
    updates: VecDeque<TransferUpdate>,
    last_telemetry: std::time::Instant,
}

impl FileTransfers {
    /// Build an engine that writes arrivals into `downloads`.
    #[must_use]
    pub fn new(downloads: PathBuf, enabled: bool) -> Self {
        Self {
            enabled,
            downloads,
            // Odd on one side and even on the other would avoid collisions
            // between simultaneous transfers, but the two directions are
            // tracked separately, so an id only has to be unique per sender.
            next_id: 1,
            sending: HashMap::new(),
            receiving: HashMap::new(),
            updates: VecDeque::new(),
            last_telemetry: std::time::Instant::now(),
        }
    }

    /// Where arriving files are written.
    #[must_use]
    pub fn downloads(&self) -> &Path {
        &self.downloads
    }

    /// Start sending a local file.
    ///
    /// Hashing happens here, up front, so the receiver learns what to expect
    /// before the first byte arrives rather than having to trust a hash that
    /// shows up after it has already written everything to disk. The cost is
    /// one read of the file before sending starts.
    pub fn send_file(&mut self, path: &Path) -> anyhow::Result<Action> {
        let id = self.next_id;
        self.next_id += 1;
        let result = self.prepare_send(id, path);
        if let Err(error) = &result {
            self.local_failure(id, path, format!("{error:#}"));
        }
        result
    }

    fn local_failure(&mut self, id: u64, path: &Path, error: String) {
        record_update(
            &mut self.updates,
            TransferUpdate {
                id,
                name: path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned(),
                direction: TransferDirection::Send,
                transferred: 0,
                total: std::fs::symlink_metadata(path).map_or(0, |metadata| metadata.len()),
                state: TransferState::Failed,
                error: Some(error),
            },
        );
    }

    fn reject_local(&mut self, path: &Path, error: &str) {
        let id = self.next_id;
        self.next_id += 1;
        self.local_failure(id, path, error.into());
    }

    fn prepare_send(&mut self, id: u64, path: &Path) -> anyhow::Result<Action> {
        if !self.enabled {
            bail!("file transfer is not permitted in this session");
        }
        if self.sending.len() >= MAX_ACTIVE {
            bail!("too many transfers already in flight");
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("a file to send needs a name")?
            .to_owned();

        // A Finder copy or window drop authorizes a regular file, not traversal
        // into a copied directory or following a symlink to another location.
        if !std::fs::symlink_metadata(path)?.file_type().is_file() {
            bail!(
                "{} is not a regular file (symlinks and directories are not sent)",
                path.display()
            );
        }
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        let size = metadata.len();
        if size > MAX_FILE {
            bail!(
                "{} is larger than the {MAX_FILE} byte limit",
                path.display()
            );
        }

        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; CHUNK];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        file.seek(SeekFrom::Start(0))?;

        let offer = FileOffer {
            transfer_id: id,
            name: name.clone(),
            size,
            blake3: hasher.finalize().to_hex().to_string(),
            modified_secs: metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_secs()).ok()),
        };
        if !offer.has_safe_name() {
            bail!("{name} is not a name the other side will accept");
        }

        self.sending.insert(
            id,
            Sending {
                file,
                offset: 0,
                size,
                allowed: 0,
                finished: false,
                last_progress: std::time::Instant::now(),
                name,
            },
        );
        record_update(
            &mut self.updates,
            self.sending[&id].update(id, TransferState::Offered, None),
        );
        Ok(Action::Offer(offer))
    }

    /// Decide whether to accept a file the peer is offering.
    pub fn on_offer(&mut self, offer: &FileOffer) -> Option<Action> {
        if !self.enabled {
            tracing::debug!(name = %offer.name, "declining a file: not permitted here");
            self.reject_offer(offer, "file transfer is not permitted in this session");
            return None;
        }
        if !offer.has_safe_name() {
            tracing::warn!(name = %offer.name, "declining a file with an unusable name");
            self.reject_offer(offer, "unusable file name");
            return None;
        }
        if offer.size > MAX_FILE {
            tracing::warn!(name = %offer.name, size = offer.size, "declining an oversized file");
            self.reject_offer(offer, "file exceeds the size limit");
            return None;
        }
        if self.receiving.len() >= MAX_ACTIVE || self.receiving.contains_key(&offer.transfer_id) {
            // Do not mark an existing transfer failed for a duplicate offer.
            if !self.receiving.contains_key(&offer.transfer_id) {
                self.reject_offer(offer, "too many transfers already in flight");
            }
            return None;
        }

        let final_path = match self.free_path(&offer.name) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, name = %offer.name, "nowhere to put an arriving file");
                self.reject_offer(offer, &format!("{error:#}"));
                return None;
            }
        };
        let part =
            final_path.with_file_name(format!(".nebula-{}.nebulapart", uuid::Uuid::new_v4()));
        let file = match File::options().create_new(true).write(true).open(&part) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, path = %part.display(), "could not open a file to receive into");
                self.reject_offer(offer, &format!("{error:#}"));
                return None;
            }
        };

        self.receiving.insert(
            offer.transfer_id,
            Receiving {
                name: offer.name.clone(),
                file,
                part,
                final_path,
                have: Ranges::default(),
                acked: 0,
                size: offer.size,
                expected: offer.blake3.clone(),
                modified_secs: offer.modified_secs,
                last_progress: std::time::Instant::now(),
            },
        );
        record_update(
            &mut self.updates,
            self.receiving[&offer.transfer_id].update(
                offer.transfer_id,
                TransferState::Offered,
                None,
            ),
        );

        Some(Action::Ack(FileAck {
            transfer_id: offer.transfer_id,
            received: 0,
            window: WINDOW,
        }))
    }

    fn reject_offer(&mut self, offer: &FileOffer, error: &str) {
        if self.receiving.contains_key(&offer.transfer_id) {
            return;
        }
        record_update(
            &mut self.updates,
            TransferUpdate {
                id: offer.transfer_id,
                name: offer.name.clone(),
                direction: TransferDirection::Receive,
                transferred: 0,
                total: offer.size,
                state: TransferState::Failed,
                error: Some(error.into()),
            },
        );
    }

    /// Note how much more the peer is willing to receive.
    pub fn on_ack(&mut self, ack: &FileAck) {
        if ack.received == FAILED_RECEIVED {
            if let Some(send) = self.sending.remove(&ack.transfer_id) {
                tracing::warn!(name = %send.name, "the peer rejected or could not finish receiving a file");
                record_update(
                    &mut self.updates,
                    send.update(
                        ack.transfer_id,
                        TransferState::Failed,
                        Some("the peer rejected or could not verify and publish the file".into()),
                    ),
                );
            }
            return;
        }
        if let Some(send) = self.sending.get_mut(&ack.transfer_id) {
            if ack.received > send.size {
                return;
            }
            if send.finished && ack.received == send.size {
                tracing::info!(name = %send.name, "a file finished sending");
                record_update(
                    &mut self.updates,
                    send.update(ack.transfer_id, TransferState::Complete, None),
                );
                self.sending.remove(&ack.transfer_id);
            } else {
                // Each acknowledgement has its own stream and can arrive late.
                let allowed = ack.received.saturating_add(u64::from(ack.window));
                if allowed > send.allowed {
                    send.allowed = allowed;
                    send.last_progress = std::time::Instant::now();
                }
            }
        }
    }

    /// Take delivery of part of a file.
    pub fn on_chunk(&mut self, header: FileChunkHeader, data: &[u8]) -> Option<Action> {
        let recv = self.receiving.get_mut(&header.transfer_id)?;

        let end = header.offset.saturating_add(data.len() as u64);
        if end > recv.size {
            tracing::warn!("abandoning a transfer that overran its announced size");
            self.fail_receive(
                header.transfer_id,
                "chunk overran the announced size".into(),
            );
            return Some(failure_ack(header.transfer_id));
        }

        let written = recv
            .file
            .seek(SeekFrom::Start(header.offset))
            .and_then(|_| recv.file.write_all(data));
        if let Err(error) = written {
            tracing::warn!(%error, "abandoning a transfer that could not be written");
            self.fail_receive(
                header.transfer_id,
                format!("could not write arriving file: {error}"),
            );
            return Some(failure_ack(header.transfer_id));
        }
        recv.last_progress = std::time::Instant::now();
        recv.have.add(header.offset, end);
        record_update(
            &mut self.updates,
            recv.update(header.transfer_id, TransferState::Transferring, None),
        );

        let complete = recv.have.complete(recv.size);
        let received = recv.have.contiguous();
        let due = complete || received - recv.acked >= ACK_EVERY;
        if due {
            recv.acked = received;
        }

        if complete {
            match self.finish(header.transfer_id) {
                Ok(path) => tracing::info!(path = %path.display(), "a file arrived"),
                Err(error) => {
                    tracing::warn!(%error, "a file arrived damaged and was discarded");
                    return Some(failure_ack(header.transfer_id));
                }
            }
        }

        due.then_some(Action::Ack(FileAck {
            transfer_id: header.transfer_id,
            received,
            window: WINDOW,
        }))
    }

    /// Produce the next chunk that the receiver has room for, if any.
    pub fn pump(&mut self) -> Option<Action> {
        let ready = self
            .sending
            .iter()
            .find(|(_, s)| !s.finished && s.offset < s.allowed)
            .map(|(id, _)| *id)?;

        let send = self.sending.get_mut(&ready)?;
        if send.size == 0 {
            send.finished = true;
            send.last_progress = std::time::Instant::now();
            record_update(
                &mut self.updates,
                send.update(ready, TransferState::Transferring, None),
            );
            return Some(Action::Chunk(
                FileChunkHeader {
                    transfer_id: ready,
                    offset: 0,
                },
                Vec::new(),
            ));
        }
        let room = (send.allowed - send.offset).min(send.size - send.offset);
        let want = room.min(CHUNK as u64) as usize;
        let mut buffer = vec![0u8; want];
        let mut filled = 0;
        while filled < want {
            match send.file.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(error) => {
                    tracing::warn!(%error, name = %send.name, "giving up on a file being sent");
                    record_update(
                        &mut self.updates,
                        send.update(
                            ready,
                            TransferState::Failed,
                            Some(format!("could not read file: {error}")),
                        ),
                    );
                    self.sending.remove(&ready);
                    return None;
                }
            }
        }
        if filled == 0 {
            // The file shrank under us. Nothing sensible left to send.
            record_update(
                &mut self.updates,
                send.update(
                    ready,
                    TransferState::Failed,
                    Some("source file shrank during transfer".into()),
                ),
            );
            self.sending.remove(&ready);
            return None;
        }
        buffer.truncate(filled);

        let header = FileChunkHeader {
            transfer_id: ready,
            offset: send.offset,
        };
        send.offset += filled as u64;
        send.last_progress = std::time::Instant::now();
        if send.offset >= send.size {
            // Retain the slot until the receiver confirms completion. New offers
            // otherwise overtake the final chunk on independent QUIC streams.
            send.finished = true;
        }
        record_update(
            &mut self.updates,
            send.update(ready, TransferState::Transferring, None),
        );
        Some(Action::Chunk(header, buffer))
    }

    /// Whether [`Self::pump`] has something to do right now.
    #[must_use]
    pub fn wants_to_send(&self) -> bool {
        self.sending
            .values()
            .any(|s| !s.finished && s.offset < s.allowed)
    }

    fn expire_stalled(&mut self, now: std::time::Instant) {
        self.sending.retain(|&id, send| {
            let expired = now.saturating_duration_since(send.last_progress) >= STALL_TIMEOUT;
            if expired {
                tracing::warn!(name = %send.name, "file send timed out without peer progress");
                record_update(
                    &mut self.updates,
                    send.update(
                        id,
                        TransferState::Failed,
                        Some("file send timed out without peer progress".into()),
                    ),
                );
            }
            !expired
        });
        let expired: Vec<_> = self
            .receiving
            .iter()
            .filter(|(_, recv)| now.saturating_duration_since(recv.last_progress) >= STALL_TIMEOUT)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            tracing::warn!(transfer_id = id, "incomplete file receive timed out");
            self.fail_receive(id, "incomplete file receive timed out".into());
        }
    }

    /// Verify a completed arrival and move it into place.
    fn finish(&mut self, transfer_id: u64) -> anyhow::Result<PathBuf> {
        let recv = self
            .receiving
            .remove(&transfer_id)
            .context("no such transfer")?;
        let mut update = recv.update(transfer_id, TransferState::Complete, None);
        let synced = recv.file.sync_all();
        drop(recv.file);

        let result = (|| {
            synced.context("syncing the arriving file")?;
            // Hashed by reading the finished file rather than as bytes arrive:
            // out-of-order chunks mean there is no arrival order to hash in.
            let mut hasher = blake3::Hasher::new();
            let mut reading = File::open(&recv.part)?;
            let mut buffer = vec![0u8; CHUNK];
            loop {
                let read = reading.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            drop(reading);
            let actual = hasher.finalize().to_hex().to_string();
            if actual != recv.expected {
                bail!("the contents did not match the announced hash");
            }

            if let Some(secs) = recv.modified_secs {
                set_modified(&recv.part, secs);
            }
            let name = recv
                .final_path
                .file_name()
                .and_then(|name| name.to_str())
                .context("an arriving file needs a name")?;
            let mut destination = recv.final_path.clone();
            for _ in 0..1000 {
                // Publication never overwrites a case alias, symlink or racing file.
                match publish_no_replace(&recv.part, &destination) {
                    Ok(()) => return Ok(destination),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        destination = self.free_path(name)?;
                    }
                    Err(error) => {
                        return Err(error)
                            .context("publishing the arriving file without replacement")
                    }
                }
            }
            bail!("too many collisions while publishing the arriving file")
        })();
        if let Err(error) = std::fs::remove_file(&recv.part) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, path = %recv.part.display(), "could not remove file-transfer staging file");
            }
        }
        if let Err(error) = &result {
            update.state = TransferState::Failed;
            update.error = Some(format!("{error:#}"));
        }
        record_update(&mut self.updates, update);
        result
    }

    /// Drop a transfer and remove whatever was written for it.
    fn abandon(&mut self, transfer_id: u64) {
        if let Some(recv) = self.receiving.remove(&transfer_id) {
            drop(recv.file);
            std::fs::remove_file(&recv.part).ok();
        }
    }

    fn fail_receive(&mut self, id: u64, error: String) {
        if let Some(recv) = self.receiving.get(&id) {
            record_update(
                &mut self.updates,
                recv.update(id, TransferState::Failed, Some(error)),
            );
        }
        self.abandon(id);
    }

    fn flush_updates(&mut self, telemetry: &tokio::sync::mpsc::UnboundedSender<TransferUpdate>) {
        self.flush_updates_at(telemetry, std::time::Instant::now());
    }

    fn flush_updates_at(
        &mut self,
        telemetry: &tokio::sync::mpsc::UnboundedSender<TransferUpdate>,
        now: std::time::Instant,
    ) {
        if telemetry.is_closed() {
            self.updates.clear();
            return;
        }
        let progress_due = now.saturating_duration_since(self.last_telemetry) >= TELEMETRY_INTERVAL;
        let mut deferred = VecDeque::new();
        while let Some(update) = self.updates.pop_front() {
            if update.state == TransferState::Transferring && !progress_due {
                deferred.push_back(update);
                continue;
            }
            let _ = telemetry.send(update);
        }
        self.updates = deferred;
        if progress_due {
            self.last_telemetry = now;
        }
    }

    /// Find a name in the download directory that isn't taken.
    ///
    /// Overwriting is never the right answer for a file that arrived over a
    /// network, so a second `report.pdf` becomes `report (2).pdf`.
    fn free_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.downloads)?;
        let direct = self.downloads.join(name);
        if self.path_available(&direct)? {
            return Ok(direct);
        }

        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
            _ => (name, String::new()),
        };
        for n in 2..1000 {
            let candidate = self.downloads.join(format!("{stem} ({n}){extension}"));
            if self.path_available(&candidate)? {
                return Ok(candidate);
            }
        }
        bail!("there are already too many files called {name}")
    }

    fn path_available(&self, path: &Path) -> anyhow::Result<bool> {
        if self.receiving.values().any(|r| r.final_path == path) {
            return Ok(false);
        }
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error).context("checking a file-transfer destination"),
        }
    }
}

#[cfg(target_os = "macos")]
fn publish_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::ffi::{c_char, c_int, c_uint, CString};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
    }
    // Darwin <sys/stdio.h>. Unlike hard links, exclusive rename supports exFAT.
    const RENAME_EXCL: c_uint = 0x00000004;
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "source path contains NUL")
    })?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination path contains NUL",
        )
    })?;
    // SAFETY: Both pointers reference live NUL-terminated path byte strings.
    // RENAME_EXCL atomically refuses an existing destination, including symlinks.
    if unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn publish_no_replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    // Staging and destination are on the same filesystem; the caller unlinks staging.
    std::fs::hard_link(source, destination)
}

/// Restore the sender's modification time where the platform allows it.
fn set_modified(path: &Path, secs: i64) {
    let Ok(secs) = u64::try_from(secs) else {
        return;
    };
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    if let Ok(file) = File::options().write(true).open(path) {
        file.set_modified(when).ok();
    }
}

/// The default place arriving files are written.
///
/// `NEBULA_DOWNLOADS` overrides it, which is how a test can let files
/// arrive without writing into the real account's Downloads folder.
#[must_use]
pub fn default_downloads() -> PathBuf {
    if let Some(configured) = std::env::var_os("NEBULA_DOWNLOADS") {
        return PathBuf::from(configured);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join("Downloads").join("NebulaDesk")
}

/// Something arriving from the peer, or a local request to send.
#[derive(Debug)]
pub enum Inbound {
    /// The peer wants to send a file.
    Offer(FileOffer),
    /// Part of a file the peer is sending.
    Chunk(FileChunkHeader, Vec<u8>),
    /// The peer has room for more.
    Ack(FileAck),
    /// This side wants to send a local file.
    Send(PathBuf),
}

/// A handle on the thread doing the filesystem work.
pub struct Worker {
    inbound: std::sync::mpsc::Sender<Inbound>,
    /// What to send to the peer.
    pub outbound: tokio::sync::mpsc::Receiver<Action>,
    /// Coalesced engine snapshots, keyed by `(direction, id)`.
    /// Progress is coalesced at 10 Hz; terminal states are sent immediately.
    /// Closed unless created with [`spawn_with_telemetry`]; opted-in consumers
    /// must drain or drop this unbounded receiver.
    pub telemetry: tokio::sync::mpsc::UnboundedReceiver<TransferUpdate>,
}

impl Worker {
    /// Hand something to the worker, logging a worker that has gone away.
    pub fn deliver(&self, message: Inbound) {
        if self.inbound.send(message).is_err() {
            tracing::warn!("file transfer worker is unavailable");
        }
    }

    /// A sender that can ask for local files to be sent.
    #[must_use]
    pub fn requests(&self) -> std::sync::mpsc::Sender<Inbound> {
        self.inbound.clone()
    }
}

/// Run file transfer on its own thread.
///
/// Telemetry is disabled so existing callers need not drain an unused receiver.
pub fn spawn(downloads: PathBuf, enabled: bool) -> Worker {
    spawn_inner(downloads, enabled, false)
}

/// Run file transfer with telemetry for a caller that will drain the updates.
pub fn spawn_with_telemetry(downloads: PathBuf, enabled: bool) -> Worker {
    spawn_inner(downloads, enabled, true)
}

fn spawn_inner(downloads: PathBuf, enabled: bool, telemetry_enabled: bool) -> Worker {
    let (inbound, requests) = std::sync::mpsc::channel::<Inbound>();
    // Bounded so that a session which has stopped draining stops the disk
    // reads too, instead of pulling the whole file into memory.
    let (actions, outbound) = tokio::sync::mpsc::channel::<Action>(4);
    let (telemetry_tx, mut telemetry) = tokio::sync::mpsc::unbounded_channel();
    if !telemetry_enabled {
        telemetry.close();
    }

    std::thread::Builder::new()
        .name("nebula-files".into())
        .spawn(move || {
            let mut transfers = FileTransfers::new(downloads, enabled);
            let mut pending = VecDeque::new();
            loop {
                transfers.expire_stalled(std::time::Instant::now());
                transfers.flush_updates(&telemetry_tx);
                // With chunks waiting there is no reason to sit on the
                // channel; with nothing to send there is no reason to spin.
                let can_start = transfers.sending.len() < MAX_ACTIVE && !pending.is_empty();
                let wait = if transfers.wants_to_send() || can_start {
                    std::time::Duration::ZERO
                } else {
                    POLL
                };

                let action = match requests.recv_timeout(wait) {
                    Ok(Inbound::Offer(offer)) => transfers.on_offer(&offer),
                    Ok(Inbound::Chunk(header, data)) => transfers.on_chunk(header, &data),
                    Ok(Inbound::Ack(ack)) => {
                        transfers.on_ack(&ack);
                        None
                    }
                    Ok(Inbound::Send(path)) => {
                        if !enabled {
                            tracing::warn!(path = %path.display(), "file transfer is not permitted");
                            transfers.reject_local(&path, "file transfer is not permitted in this session");
                        } else if pending.len() >= 256 {
                            tracing::warn!(path = %path.display(), "file send queue is full; copy this file again later");
                            transfers.reject_local(&path, "file send queue is full; copy this file again later");
                        } else {
                            pending.push_back(path);
                        }
                        None
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                };
                transfers.flush_updates(&telemetry_tx);

                if let Some(action) = action {
                    if actions.blocking_send(action).is_err() {
                        return;
                    }
                }
                if transfers.sending.len() < MAX_ACTIVE {
                    if let Some(path) = pending.pop_front() {
                        match transfers.send_file(&path) {
                            Ok(action) => {
                                transfers.flush_updates(&telemetry_tx);
                                if actions.blocking_send(action).is_err() {
                                    return;
                                }
                            }
                            Err(error) => {
                                tracing::warn!(%error, path = %path.display(), "cannot send that file");
                            }
                        }
                    }
                }
                let action = transfers.pump();
                transfers.flush_updates(&telemetry_tx);
                if let Some(action) = action {
                    if actions.blocking_send(action).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning a thread should not fail");

    Worker {
        inbound,
        outbound,
        telemetry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_completes_only_after_verified_publish_and_final_ack() {
        for contents in [b"verified contents".as_slice(), b"".as_slice()] {
            let source = tempfile::tempdir().unwrap();
            let target = tempfile::tempdir().unwrap();
            let path = write(source.path(), "transfer.txt", contents);
            let mut sender = FileTransfers::new(source.path().into(), true);
            let mut receiver = FileTransfers::new(target.path().into(), true);
            let Action::Offer(offer) = sender.send_file(&path).unwrap() else {
                panic!()
            };
            assert_eq!(
                sender.updates.pop_front().unwrap().state,
                TransferState::Offered
            );
            let Some(Action::Ack(ack)) = receiver.on_offer(&offer) else {
                panic!()
            };
            assert_eq!(
                receiver.updates.pop_front().unwrap().state,
                TransferState::Offered
            );
            sender.on_ack(&ack);
            assert!(
                sender.updates.is_empty(),
                "the initial zero-byte ACK is not completion"
            );
            let Some(Action::Chunk(header, bytes)) = sender.pump() else {
                panic!()
            };
            let progress = sender.updates.pop_front().unwrap();
            assert_eq!(progress.state, TransferState::Transferring);
            assert_eq!(progress.transferred, offer.size);
            assert!(sender.sending.contains_key(&offer.transfer_id));
            let Some(Action::Ack(final_ack)) = receiver.on_chunk(header, &bytes) else {
                panic!()
            };
            let received = receiver.updates.pop_front().unwrap();
            assert_eq!(received.state, TransferState::Complete);
            assert_eq!(received.direction, TransferDirection::Receive);
            assert_eq!(received.transferred, offer.size);
            assert_eq!(
                std::fs::read(target.path().join(&offer.name)).unwrap(),
                contents
            );
            assert!(
                sender.updates.is_empty(),
                "publication alone does not finish the sender"
            );
            sender.on_ack(&final_ack);
            let sent = sender.updates.pop_front().unwrap();
            assert_eq!(sent.state, TransferState::Complete);
            assert_eq!(sent.direction, TransferDirection::Send);
            assert_eq!(sent.error, None);
            assert!(!sender.sending.contains_key(&offer.transfer_id));
            sender.on_ack(&final_ack);
            assert!(
                sender.updates.is_empty(),
                "duplicate ACKs do not repeat completion"
            );
        }
    }

    #[test]
    fn telemetry_reports_hash_failure_and_peer_failure_without_completion() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = write(source.path(), "damaged.txt", b"before");
        let mut sender = FileTransfers::new(source.path().into(), true);
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let Action::Offer(offer) = sender.send_file(&path).unwrap() else {
            panic!()
        };
        let Some(Action::Ack(ack)) = receiver.on_offer(&offer) else {
            panic!()
        };
        sender.on_ack(&ack);
        std::fs::write(path, b"after!").unwrap();
        let Some(Action::Chunk(header, bytes)) = sender.pump() else {
            panic!()
        };
        let Some(Action::Ack(ack)) = receiver.on_chunk(header, &bytes) else {
            panic!()
        };
        assert_eq!(ack.received, FAILED_RECEIVED);
        sender.on_ack(&ack);
        for updates in [&sender.updates, &receiver.updates] {
            let update = updates.back().unwrap();
            assert_eq!(update.state, TransferState::Failed);
            assert!(update.error.as_ref().is_some_and(|error| !error.is_empty()));
        }
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    }

    #[test]
    fn telemetry_reports_unique_pre_offer_failures_and_receive_disk_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let mut transfers = FileTransfers::new(dir.path().into(), true);
        let missing = dir.path().join("missing.txt");
        assert!(transfers.send_file(&missing).is_err());
        assert!(transfers.send_file(&missing).is_err());
        transfers.reject_local(&missing, "queue full");
        let updates: Vec<_> = transfers.updates.drain(..).collect();
        assert_eq!(updates.len(), 3);
        assert!(updates.windows(2).all(|pair| pair[0].id != pair[1].id));
        assert!(updates
            .iter()
            .all(|update| update.state == TransferState::Failed
                && update.name == "missing.txt"
                && update.error.is_some()));

        let blocked = write(dir.path(), "not-a-directory", b"block downloads");
        let mut receiver = FileTransfers::new(blocked, true);
        assert!(receiver
            .on_offer(&FileOffer {
                transfer_id: 7,
                name: "incoming.txt".into(),
                size: 1,
                blake3: blake3::hash(b"x").to_hex().to_string(),
                modified_secs: None,
            })
            .is_none());
        let failed = receiver.updates.pop_front().unwrap();
        assert_eq!(failed.direction, TransferDirection::Receive);
        assert_eq!(failed.state, TransferState::Failed);
        assert!(failed.error.is_some());
    }

    #[test]
    fn telemetry_counts_nonoverlapping_received_bytes_and_bounds_pending_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(dir.path().into(), true);
        let offer = FileOffer {
            transfer_id: 1,
            name: "reordered.txt".into(),
            size: 6,
            blake3: blake3::hash(b"abcdef").to_hex().to_string(),
            modified_secs: None,
        };
        receiver.on_offer(&offer);
        for _ in 0..2 {
            receiver.on_chunk(
                FileChunkHeader {
                    transfer_id: 1,
                    offset: 3,
                },
                b"def",
            );
            assert_eq!(receiver.updates.back().unwrap().transferred, 3);
        }
        receiver.expire_stalled(std::time::Instant::now() + STALL_TIMEOUT);
        assert_eq!(
            receiver.updates.back().unwrap().state,
            TransferState::Failed
        );
        for _ in 0..MAX_UPDATES + 10 {
            receiver.reject_local(&dir.path().join("missing.txt"), "queue full");
        }
        assert_eq!(receiver.updates.len(), MAX_UPDATES);
    }

    #[test]
    fn telemetry_reports_source_read_failure_and_tolerates_a_dropped_consumer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "shrinking.txt", b"contents");
        let mut sender = FileTransfers::new(dir.path().into(), true);
        let Action::Offer(offer) = sender.send_file(&path).unwrap() else {
            panic!()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        sender.flush_updates(&tx);
        assert_eq!(rx.try_recv().unwrap().state, TransferState::Offered);
        sender.on_ack(&FileAck {
            transfer_id: offer.transfer_id,
            received: 0,
            window: WINDOW,
        });
        std::fs::write(path, b"").unwrap();
        assert!(sender.pump().is_none());
        sender.flush_updates(&tx);
        assert_eq!(rx.try_recv().unwrap().state, TransferState::Failed);
        assert!(sender.sending.is_empty());
        drop(rx);
        sender.reject_local(&dir.path().join("missing"), "missing source");
        sender.flush_updates(&tx);
        assert!(sender.updates.is_empty());
    }

    #[tokio::test]
    async fn worker_exposes_failed_paths_before_any_wire_offer() {
        let dir = tempfile::tempdir().unwrap();
        let mut worker = spawn_with_telemetry(dir.path().into(), true);
        worker.deliver(Inbound::Send(dir.path().join("missing.txt")));
        let update =
            tokio::time::timeout(std::time::Duration::from_secs(5), worker.telemetry.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(update.state, TransferState::Failed);
        assert_eq!(update.name, "missing.txt");
        assert!(worker.outbound.try_recv().is_err());
    }

    #[test]
    fn telemetry_progress_is_coalesced_per_transfer_for_one_hundred_milliseconds() {
        let mut transfers = FileTransfers::new(PathBuf::new(), true);
        let start = transfers.last_telemetry;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let update = |id, state, transferred| TransferUpdate {
            id,
            name: format!("file-{id}"),
            direction: TransferDirection::Send,
            transferred,
            total: 1000,
            state,
            error: None,
        };
        for id in 1..=2 {
            record_update(
                &mut transfers.updates,
                update(id, TransferState::Offered, 0),
            );
            transfers.flush_updates_at(&tx, start);
            assert_eq!(rx.try_recv().unwrap().state, TransferState::Offered);
        }
        for transferred in 1..=1000 {
            for id in 1..=2 {
                record_update(
                    &mut transfers.updates,
                    update(id, TransferState::Transferring, transferred),
                );
                transfers.flush_updates_at(&tx, start + TELEMETRY_INTERVAL / 2);
            }
        }
        assert!(
            rx.try_recv().is_err(),
            "line-rate chunks must not flood telemetry"
        );
        assert_eq!(transfers.updates.len(), 2);
        transfers.flush_updates_at(&tx, start + TELEMETRY_INTERVAL);
        for _ in 1..=2 {
            let progress = rx.try_recv().unwrap();
            assert_eq!(progress.state, TransferState::Transferring);
            assert_eq!(progress.transferred, 1000);
        }
        assert!(rx.try_recv().is_err());
        for (id, state) in [
            (1, TransferState::Complete),
            (2, TransferState::Failed),
            (3, TransferState::Offered),
        ] {
            record_update(&mut transfers.updates, update(id, state, 1000));
            transfers.flush_updates_at(&tx, start + TELEMETRY_INTERVAL);
            assert_eq!(
                rx.try_recv().unwrap().state,
                state,
                "lifecycle events bypass throttling"
            );
        }
    }

    #[tokio::test]
    async fn default_worker_never_queues_unconsumed_telemetry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "standalone.txt", b"contents");
        let mut worker = spawn(dir.path().into(), true);
        assert!(worker.telemetry.is_closed());
        worker.deliver(Inbound::Send(path));
        let action =
            tokio::time::timeout(std::time::Duration::from_secs(5), worker.outbound.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(action, Action::Offer(_)));
        assert_eq!(worker.telemetry.len(), 0);
        assert!(matches!(
            worker.telemetry.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn exclusive_native_rename_preserves_path_bytes_and_rejects_nul() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source-中 é");
        let destination = dir.path().join("target-中 é");
        std::fs::write(&source, b"native").unwrap();
        let invalid = dir
            .path()
            .join(std::ffi::OsString::from_vec(b"invalid\0suffix".to_vec()));
        assert_eq!(
            publish_no_replace(&source, &invalid).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            publish_no_replace(&invalid, &destination)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(source.exists());
        publish_no_replace(&source, &destination).unwrap();
        assert!(
            !source.exists(),
            "native publication renames rather than links"
        );
        assert_eq!(std::fs::read(destination).unwrap(), b"native");
    }

    #[cfg(unix)]
    #[test]
    fn publication_does_not_replace_or_follow_directory_symlink_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(dir.path(), "staging", b"verified");
        let directory = dir.path().join("existing-directory");
        std::fs::create_dir(&directory).unwrap();
        let link = dir.path().join("existing-link");
        std::os::unix::fs::symlink(&directory, &link).unwrap();
        let dangling = dir.path().join("dangling-link");
        std::os::unix::fs::symlink(dir.path().join("absent"), &dangling).unwrap();
        for destination in [&directory, &link, &dangling] {
            assert_eq!(
                publish_no_replace(&source, destination).unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
            assert_eq!(std::fs::read(&source).unwrap(), b"verified");
        }
        assert!(std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::symlink_metadata(dangling)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn four_changed_sources_fail_explicitly_and_queued_file_still_arrives() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let mut sender = spawn(source.path().into(), true);
        let mut receiver = spawn(target.path().into(), true);
        for i in 0..=MAX_ACTIVE {
            let path = write(source.path(), &format!("copy-{i}.txt"), b"before");
            sender.deliver(Inbound::Send(path));
        }
        let mut failures = 0;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    action = sender.outbound.recv() => match action.unwrap() {
                        Action::Offer(offer) => {
                            if offer.name != format!("copy-{MAX_ACTIVE}.txt") {
                                // The offer proves hashing finished; retain the original size.
                                std::fs::write(source.path().join(&offer.name), b"after!").unwrap();
                            }
                            receiver.deliver(Inbound::Offer(offer));
                        }
                        Action::Chunk(header, bytes) => receiver.deliver(Inbound::Chunk(header, bytes)),
                        other => panic!("unexpected sender action {other:?}"),
                    },
                    action = receiver.outbound.recv() => {
                        let Action::Ack(ack) = action.unwrap() else { panic!("expected an ack") };
                        // Exercise the actual wire decoder, which forbids a zero window.
                        let decoded = FileAck::decode(&ack.to_bytes()).unwrap();
                        if decoded.received == FAILED_RECEIVED {
                            failures += 1;
                        }
                        sender.deliver(Inbound::Ack(decoded));
                    },
                }
                if failures == MAX_ACTIVE && target.path().join(format!("copy-{MAX_ACTIVE}.txt")).exists() {
                    break;
                }
            }
        }).await.expect("failed transfers must not permanently occupy the send slots");
        assert_eq!(
            std::fs::read(target.path().join(format!("copy-{MAX_ACTIVE}.txt"))).unwrap(),
            b"before"
        );
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 1);
    }

    #[test]
    fn stalled_legacy_sends_and_incomplete_receives_release_their_slots() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = write(source.path(), "stalled.txt", b"contents");
        let mut sender = FileTransfers::new(source.path().into(), true);
        let mut receiver = FileTransfers::new(target.path().into(), true);
        for _ in 0..MAX_ACTIVE {
            let Action::Offer(offer) = sender.send_file(&path).unwrap() else {
                panic!("expected offer")
            };
            let Some(Action::Ack(ack)) = receiver.on_offer(&offer) else {
                panic!("expected ack")
            };
            sender.on_ack(&ack);
        }
        while sender.pump().is_some() {}
        assert_eq!(sender.sending.len(), MAX_ACTIVE);
        assert!(sender.send_file(&path).is_err());
        let expired_at = std::time::Instant::now() + STALL_TIMEOUT;
        sender.expire_stalled(expired_at);
        receiver.expire_stalled(expired_at);
        assert!(sender
            .updates
            .iter()
            .all(|update| update.state == TransferState::Failed));
        assert!(receiver
            .updates
            .iter()
            .all(|update| update.state == TransferState::Failed));
        assert!(sender.sending.is_empty());
        assert!(receiver.receiving.is_empty());
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
        assert!(sender.send_file(&path).is_ok());
    }

    #[test]
    fn concurrent_case_equivalent_names_never_overwrite_each_other() {
        let target = tempfile::tempdir().unwrap();
        let probe = write(target.path(), "CaseProbe", b"probe");
        let case_insensitive = target.path().join("caseprobe").exists();
        std::fs::remove_file(probe).unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);
        for (id, name, bytes) in [
            (1, "Report.txt", b"first".as_slice()),
            (2, "report.txt", b"second".as_slice()),
        ] {
            assert!(receiver
                .on_offer(&FileOffer {
                    transfer_id: id,
                    name: name.into(),
                    size: bytes.len() as u64,
                    blake3: blake3::hash(bytes).to_hex().to_string(),
                    modified_secs: None,
                })
                .is_some());
        }
        for (id, bytes) in [(1, b"first".as_slice()), (2, b"second".as_slice())] {
            let Some(Action::Ack(ack)) = receiver.on_chunk(
                FileChunkHeader {
                    transfer_id: id,
                    offset: 0,
                },
                bytes,
            ) else {
                panic!("completion must be acknowledged")
            };
            assert_eq!(ack.received, bytes.len() as u64);
        }
        assert_eq!(
            std::fs::read(target.path().join("Report.txt")).unwrap(),
            b"first"
        );
        let second = if case_insensitive {
            "report (2).txt"
        } else {
            "report.txt"
        };
        assert_eq!(
            std::fs::read(target.path().join(second)).unwrap(),
            b"second"
        );
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 2);
    }

    #[test]
    fn destination_created_during_transfer_is_preserved_and_arrival_is_numbered() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let bytes = b"received";
        assert!(receiver
            .on_offer(&FileOffer {
                transfer_id: 1,
                name: "report.txt".into(),
                size: bytes.len() as u64,
                blake3: blake3::hash(bytes).to_hex().to_string(),
                modified_secs: None,
            })
            .is_some());
        write(target.path(), "report.txt", b"external writer");
        write(target.path(), "report (2).txt", b"another external writer");
        let Some(Action::Ack(ack)) = receiver.on_chunk(
            FileChunkHeader {
                transfer_id: 1,
                offset: 0,
            },
            bytes,
        ) else {
            panic!("completion must be acknowledged")
        };
        assert_eq!(ack.received, bytes.len() as u64);
        assert_eq!(
            std::fs::read(target.path().join("report.txt")).unwrap(),
            b"external writer"
        );
        assert_eq!(
            std::fs::read(target.path().join("report (2).txt")).unwrap(),
            b"another external writer"
        );
        assert_eq!(
            std::fs::read(target.path().join("report (3).txt")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 3);
    }

    /// Drive a file from one engine to the other and hand back where it
    /// landed, exactly as the session loop would.
    fn transfer(
        sender: &mut FileTransfers,
        receiver: &mut FileTransfers,
        path: &Path,
    ) -> anyhow::Result<PathBuf> {
        let Action::Offer(offer) = sender.send_file(path)? else {
            panic!("sending a file starts with an offer");
        };
        let Some(Action::Ack(ack)) = receiver.on_offer(&offer) else {
            bail!("the receiver declined the file");
        };
        sender.on_ack(&ack);

        let mut landed = None;
        while let Some(action) = sender.pump() {
            let Action::Chunk(header, data) = action else {
                panic!("pumping produces chunks");
            };
            let reply = receiver.on_chunk(header, &data);
            if let Some(Action::Ack(ack)) = reply {
                sender.on_ack(&ack);
                if ack.received == offer.size {
                    landed = Some(receiver.downloads().join(&offer.name));
                }
            }
        }
        landed.context("the transfer never completed")
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_file_arrives_byte_for_byte() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        // Larger than one chunk, and deliberately not a multiple of it.
        let contents: Vec<u8> = (0..CHUNK * 3 + 17).map(|i| (i % 251) as u8).collect();
        let path = write(source.path(), "report.pdf", &contents);

        let mut sender = FileTransfers::new(source.path().into(), true);
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let landed = transfer(&mut sender, &mut receiver, &path).unwrap();

        assert_eq!(std::fs::read(&landed).unwrap(), contents);
    }

    #[test]
    fn a_file_larger_than_the_window_does_not_stall() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let contents: Vec<u8> = (0..WINDOW as usize * 2 + 17)
            .map(|i| (i % 251) as u8)
            .collect();
        let path = write(source.path(), "large.bin", &contents);
        let mut sender = FileTransfers::new(source.path().into(), true);
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let landed = transfer(&mut sender, &mut receiver, &path).unwrap();
        assert_eq!(
            blake3::hash(&std::fs::read(landed).unwrap()),
            blake3::hash(&contents)
        );
    }

    #[test]
    fn an_empty_file_completes_and_releases_its_slot() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = write(source.path(), "empty.txt", b"");
        let mut sender = FileTransfers::new(source.path().into(), true);
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let landed = transfer(&mut sender, &mut receiver, &path).unwrap();
        assert_eq!(std::fs::read(landed).unwrap(), b"");
        assert!(sender.sending.is_empty());
        assert!(receiver.receiving.is_empty());
        transfer(&mut sender, &mut receiver, &path).unwrap();
        assert_eq!(
            std::fs::read(target.path().join("empty (2).txt")).unwrap(),
            b""
        );
    }

    #[test]
    fn completion_waits_for_the_peer_and_old_window_acks_do_not_regress() {
        let source = tempfile::tempdir().unwrap();
        let path = write(source.path(), "ack.txt", b"file");
        let mut sender = FileTransfers::new(source.path().into(), true);
        let Action::Offer(offer) = sender.send_file(&path).unwrap() else {
            panic!("missing offer")
        };
        sender.on_ack(&FileAck {
            transfer_id: offer.transfer_id,
            received: 2,
            window: WINDOW,
        });
        sender.on_ack(&FileAck {
            transfer_id: offer.transfer_id,
            received: 0,
            window: WINDOW,
        });
        assert_eq!(
            sender.sending[&offer.transfer_id].allowed,
            u64::from(WINDOW) + 2
        );
        assert!(sender.pump().is_some());
        assert!(!sender.wants_to_send());
        assert_eq!(sender.sending.len(), 1, "the receiver still owns a slot");
        sender.on_ack(&FileAck {
            transfer_id: offer.transfer_id,
            received: 4,
            window: WINDOW,
        });
        assert!(sender.sending.is_empty());
    }

    #[test]
    fn concurrent_same_stem_and_same_name_transfers_have_distinct_files() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);
        for (id, name) in [(1, "report.pdf"), (2, "report.txt"), (3, "report.pdf")] {
            assert!(receiver
                .on_offer(&FileOffer {
                    transfer_id: id,
                    name: name.into(),
                    size: 1,
                    blake3: blake3::hash(&[id as u8]).to_hex().to_string(),
                    modified_secs: None,
                })
                .is_some());
        }
        for id in [3, 1, 2] {
            assert!(receiver
                .on_chunk(
                    FileChunkHeader {
                        transfer_id: id,
                        offset: 0,
                    },
                    &[id as u8]
                )
                .is_some());
        }
        assert_eq!(
            std::fs::read(target.path().join("report.pdf")).unwrap(),
            [1]
        );
        assert_eq!(
            std::fs::read(target.path().join("report.txt")).unwrap(),
            [2]
        );
        assert_eq!(
            std::fs::read(target.path().join("report (2).pdf")).unwrap(),
            [3]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_local_gesture_does_not_send_directories_or_follow_symlinks() {
        let source = tempfile::tempdir().unwrap();
        let path = write(source.path(), "real.txt", b"local");
        let link = source.path().join("link.txt");
        std::os::unix::fs::symlink(path, &link).unwrap();
        let mut sender = FileTransfers::new(source.path().into(), true);
        assert!(sender.send_file(source.path()).is_err());
        assert!(sender.send_file(&link).is_err());
        assert!(sender.sending.is_empty());
    }

    #[test]
    fn an_arrival_never_overwrites_what_is_already_there() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        write(target.path(), "notes.txt", b"mine");
        let path = write(source.path(), "notes.txt", b"theirs");

        let mut sender = FileTransfers::new(source.path().into(), true);
        let mut receiver = FileTransfers::new(target.path().into(), true);
        transfer(&mut sender, &mut receiver, &path).unwrap();

        assert_eq!(
            std::fs::read(target.path().join("notes.txt")).unwrap(),
            b"mine",
            "the file that was already there survived"
        );
        assert_eq!(
            std::fs::read(target.path().join("notes (2).txt")).unwrap(),
            b"theirs"
        );
    }

    #[test]
    fn a_name_that_climbs_out_of_the_directory_is_refused() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().join("downloads"), true);

        for name in ["../escape.txt", "/etc/passwd", "..", "C:\\loot.txt"] {
            let offer = FileOffer {
                transfer_id: 1,
                name: name.into(),
                size: 4,
                blake3: blake3::hash(b"oops").to_hex().to_string(),
                modified_secs: None,
            };
            assert!(
                receiver.on_offer(&offer).is_none(),
                "{name} should have been refused"
            );
        }
    }

    #[test]
    fn contents_that_do_not_match_the_hash_are_discarded() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);

        let offer = FileOffer {
            transfer_id: 7,
            name: "invoice.pdf".into(),
            size: 5,
            blake3: blake3::hash(b"right").to_hex().to_string(),
            modified_secs: None,
        };
        assert!(receiver.on_offer(&offer).is_some());
        let failure = receiver.on_chunk(
            FileChunkHeader {
                transfer_id: 7,
                offset: 0,
            },
            b"wrong",
        );
        assert_eq!(failure, Some(failure_ack(7)));

        assert!(
            !target.path().join("invoice.pdf").exists(),
            "corrupt contents must not be presented as a finished file"
        );
        assert_eq!(
            std::fs::read_dir(target.path()).unwrap().count(),
            0,
            "and the partial file must not be left lying around"
        );
    }

    #[test]
    fn chunks_that_arrive_out_of_order_still_make_a_whole_file() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let contents: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let offer = FileOffer {
            transfer_id: 3,
            name: "shuffled.bin".into(),
            size: 300,
            blake3: blake3::hash(&contents).to_hex().to_string(),
            modified_secs: None,
        };
        assert!(receiver.on_offer(&offer).is_some());

        // Each chunk rides its own stream, so this is what the wire really
        // looks like — not a pathological case.
        for offset in [200usize, 0, 100] {
            receiver.on_chunk(
                FileChunkHeader {
                    transfer_id: 3,
                    offset: offset as u64,
                },
                &contents[offset..offset + 100],
            );
        }

        assert_eq!(
            std::fs::read(target.path().join("shuffled.bin")).unwrap(),
            contents
        );
    }

    #[test]
    fn a_transfer_with_a_hole_in_it_never_finishes() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let offer = FileOffer {
            transfer_id: 4,
            name: "gappy.bin".into(),
            size: 300,
            blake3: blake3::hash(&[0u8; 300]).to_hex().to_string(),
            modified_secs: None,
        };
        assert!(receiver.on_offer(&offer).is_some());

        // The middle never comes. A sparse region reads back as zeroes, so
        // without range tracking this would hash as a complete file of the
        // right length — and the hash here is deliberately the one that
        // would match if it did.
        receiver.on_chunk(
            FileChunkHeader {
                transfer_id: 4,
                offset: 0,
            },
            &[0u8; 100],
        );
        receiver.on_chunk(
            FileChunkHeader {
                transfer_id: 4,
                offset: 200,
            },
            &[0u8; 100],
        );

        assert!(
            !target.path().join("gappy.bin").exists(),
            "an incomplete file must never be presented as finished"
        );
    }

    #[test]
    fn ranges_merge_into_one_contiguous_run() {
        let mut ranges = Ranges::default();
        ranges.add(100, 200);
        assert_eq!(ranges.contiguous(), 0, "nothing yet reaches the start");
        ranges.add(0, 50);
        assert_eq!(ranges.contiguous(), 50);
        ranges.add(50, 100);
        assert_eq!(ranges.contiguous(), 200, "the gap closed");
        assert!(ranges.complete(200));
    }

    #[test]
    fn a_sender_cannot_overrun_the_size_it_announced() {
        let target = tempfile::tempdir().unwrap();
        let mut receiver = FileTransfers::new(target.path().into(), true);
        let offer = FileOffer {
            transfer_id: 9,
            name: "small.bin".into(),
            size: 4,
            blake3: blake3::hash(b"abcd").to_hex().to_string(),
            modified_secs: None,
        };
        assert!(receiver.on_offer(&offer).is_some());
        assert_eq!(
            receiver.on_chunk(
                FileChunkHeader {
                    transfer_id: 9,
                    offset: 0,
                },
                &[0u8; 4096],
            ),
            Some(failure_ack(9))
        );
        assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_session_without_the_entitlement_moves_nothing_in_either_direction() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let path = write(source.path(), "secret.txt", b"nope");

        let mut denied = FileTransfers::new(target.path().into(), false);
        assert!(
            denied.send_file(&path).is_err(),
            "an unentitled session cannot send"
        );

        let offer = FileOffer {
            transfer_id: 1,
            name: "secret.txt".into(),
            size: 4,
            blake3: blake3::hash(b"nope").to_hex().to_string(),
            modified_secs: None,
        };
        assert!(
            denied.on_offer(&offer).is_none(),
            "nor may it receive, whatever the peer claims"
        );
    }

    #[test]
    fn nothing_is_sent_until_the_receiver_makes_room() {
        let source = tempfile::tempdir().unwrap();
        let path = write(source.path(), "waiting.bin", &[7u8; 1024]);
        let mut sender = FileTransfers::new(source.path().into(), true);
        sender.send_file(&path).unwrap();

        assert!(!sender.wants_to_send());
        assert!(
            sender.pump().is_none(),
            "a sender that ignores the window is a sender that floods the link"
        );

        sender.on_ack(&FileAck {
            transfer_id: 1,
            received: 0,
            window: 512,
        });
        assert!(sender.wants_to_send());
        let Some(Action::Chunk(_, data)) = sender.pump() else {
            panic!("the window opened");
        };
        assert_eq!(data.len(), 512, "and it sends only as much as was allowed");
        assert!(!sender.wants_to_send());
    }
}
