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
//! * Bytes land in a `.part` file and are renamed into place only after the
//!   hash matches. A transfer that dies halfway leaves something obviously
//!   incomplete rather than a file that looks finished and isn't.
//! * Chunks are written where the sender says they belong, and the file is
//!   only finished once every byte of it is accounted for. Each chunk rides
//!   its own QUIC stream — that is the whole point of the transport — so
//!   they arrive in whatever order the network delivers them, and a receiver
//!   that insisted on sequence would be waiting for an ordering nobody
//!   promised. Nothing is trusted about where a chunk claims to go beyond
//!   its having to fit inside the announced size.

use std::collections::HashMap;
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
    name: String,
}

/// A file being received.
struct Receiving {
    file: File,
    part: PathBuf,
    final_path: PathBuf,
    /// Byte ranges written so far, merged and in order.
    have: Ranges,
    acked: u64,
    size: u64,
    expected: String,
    modified_secs: Option<i64>,
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
            transfer_id: self.next_id,
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
            self.next_id,
            Sending {
                file,
                offset: 0,
                size,
                allowed: 0,
                name,
            },
        );
        self.next_id += 1;
        Ok(Action::Offer(offer))
    }

    /// Decide whether to accept a file the peer is offering.
    pub fn on_offer(&mut self, offer: &FileOffer) -> Option<Action> {
        if !self.enabled {
            tracing::debug!(name = %offer.name, "declining a file: not permitted here");
            return None;
        }
        if !offer.has_safe_name() {
            tracing::warn!(name = %offer.name, "declining a file with an unusable name");
            return None;
        }
        if offer.size > MAX_FILE {
            tracing::warn!(name = %offer.name, size = offer.size, "declining an oversized file");
            return None;
        }
        if self.receiving.len() >= MAX_ACTIVE || self.receiving.contains_key(&offer.transfer_id) {
            return None;
        }

        let final_path = match self.free_path(&offer.name) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, name = %offer.name, "nowhere to put an arriving file");
                return None;
            }
        };
        let part = final_path.with_extension("nebulapart");
        let file = match File::options()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&part)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, path = %part.display(), "could not open a file to receive into");
                return None;
            }
        };

        self.receiving.insert(
            offer.transfer_id,
            Receiving {
                file,
                part,
                final_path,
                have: Ranges::default(),
                acked: 0,
                size: offer.size,
                expected: offer.blake3.clone(),
                modified_secs: offer.modified_secs,
            },
        );

        Some(Action::Ack(FileAck {
            transfer_id: offer.transfer_id,
            received: 0,
            window: WINDOW,
        }))
    }

    /// Note how much more the peer is willing to receive.
    pub fn on_ack(&mut self, ack: &FileAck) {
        if let Some(send) = self.sending.get_mut(&ack.transfer_id) {
            send.allowed = ack.received.saturating_add(u64::from(ack.window));
        }
    }

    /// Take delivery of part of a file.
    pub fn on_chunk(&mut self, header: FileChunkHeader, data: &[u8]) -> Option<Action> {
        let recv = self.receiving.get_mut(&header.transfer_id)?;

        let end = header.offset.saturating_add(data.len() as u64);
        if end > recv.size {
            tracing::warn!("abandoning a transfer that overran its announced size");
            self.abandon(header.transfer_id);
            return None;
        }

        let written = recv
            .file
            .seek(SeekFrom::Start(header.offset))
            .and_then(|_| recv.file.write_all(data));
        if let Err(error) = written {
            tracing::warn!(%error, "abandoning a transfer that could not be written");
            self.abandon(header.transfer_id);
            return None;
        }
        recv.have.add(header.offset, end);

        let complete = recv.have.complete(recv.size);
        let received = recv.have.contiguous();
        let due = complete || received - recv.acked >= ACK_EVERY;
        recv.acked = received;

        if complete {
            match self.finish(header.transfer_id) {
                Ok(path) => tracing::info!(path = %path.display(), "a file arrived"),
                Err(error) => tracing::warn!(%error, "a file arrived damaged and was discarded"),
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
            .find(|(_, s)| s.offset < s.size && s.offset < s.allowed)
            .map(|(id, _)| *id)?;

        let send = self.sending.get_mut(&ready)?;
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
                    self.sending.remove(&ready);
                    return None;
                }
            }
        }
        if filled == 0 {
            // The file shrank under us. Nothing sensible left to send.
            self.sending.remove(&ready);
            return None;
        }
        buffer.truncate(filled);

        let header = FileChunkHeader {
            transfer_id: ready,
            offset: send.offset,
        };
        send.offset += filled as u64;
        if send.offset >= send.size {
            tracing::info!(name = %send.name, "a file finished sending");
            self.sending.remove(&ready);
        }
        Some(Action::Chunk(header, buffer))
    }

    /// Whether [`Self::pump`] has something to do right now.
    #[must_use]
    pub fn wants_to_send(&self) -> bool {
        self.sending
            .values()
            .any(|s| s.offset < s.size && s.offset < s.allowed)
    }

    /// Verify a completed arrival and move it into place.
    fn finish(&mut self, transfer_id: u64) -> anyhow::Result<PathBuf> {
        let recv = self
            .receiving
            .remove(&transfer_id)
            .context("no such transfer")?;
        recv.file.sync_all().ok();
        drop(recv.file);

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
            std::fs::remove_file(&recv.part).ok();
            bail!("the contents did not match the announced hash");
        }

        std::fs::rename(&recv.part, &recv.final_path).with_context(|| {
            format!(
                "moving {} into place at {}",
                recv.part.display(),
                recv.final_path.display()
            )
        })?;
        if let Some(secs) = recv.modified_secs {
            set_modified(&recv.final_path, secs);
        }
        Ok(recv.final_path)
    }

    /// Drop a transfer and remove whatever was written for it.
    fn abandon(&mut self, transfer_id: u64) {
        if let Some(recv) = self.receiving.remove(&transfer_id) {
            drop(recv.file);
            std::fs::remove_file(&recv.part).ok();
        }
    }

    /// Find a name in the download directory that isn't taken.
    ///
    /// Overwriting is never the right answer for a file that arrived over a
    /// network, so a second `report.pdf` becomes `report (2).pdf`.
    fn free_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.downloads)?;
        let direct = self.downloads.join(name);
        if !direct.exists() {
            return Ok(direct);
        }

        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
            _ => (name, String::new()),
        };
        for n in 2..1000 {
            let candidate = self.downloads.join(format!("{stem} ({n}){extension}"));
            if !candidate.exists() {
                return Ok(candidate);
            }
        }
        bail!("there are already too many files called {name}")
    }
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
}

impl Worker {
    /// Hand something to the worker, ignoring a worker that has gone away.
    pub fn deliver(&self, message: Inbound) {
        let _ = self.inbound.send(message);
    }

    /// A sender that can ask for local files to be sent.
    #[must_use]
    pub fn requests(&self) -> std::sync::mpsc::Sender<Inbound> {
        self.inbound.clone()
    }
}

/// Run file transfer on its own thread.
pub fn spawn(downloads: PathBuf, enabled: bool) -> Worker {
    let (inbound, requests) = std::sync::mpsc::channel::<Inbound>();
    // Bounded so that a session which has stopped draining stops the disk
    // reads too, instead of pulling the whole file into memory.
    let (actions, outbound) = tokio::sync::mpsc::channel::<Action>(4);

    std::thread::Builder::new()
        .name("nebula-files".into())
        .spawn(move || {
            let mut transfers = FileTransfers::new(downloads, enabled);
            loop {
                // With chunks waiting there is no reason to sit on the
                // channel; with nothing to send there is no reason to spin.
                let wait = if transfers.wants_to_send() {
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
                    Ok(Inbound::Send(path)) => match transfers.send_file(&path) {
                        Ok(action) => Some(action),
                        Err(error) => {
                            tracing::warn!(%error, path = %path.display(), "cannot send that file");
                            None
                        }
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => transfers.pump(),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                };

                if let Some(action) = action {
                    if actions.blocking_send(action).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawning a thread should not fail");

    Worker { inbound, outbound }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        receiver.on_chunk(
            FileChunkHeader {
                transfer_id: 7,
                offset: 0,
            },
            b"wrong",
        );

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
        assert!(receiver
            .on_chunk(
                FileChunkHeader {
                    transfer_id: 9,
                    offset: 0,
                },
                &[0u8; 4096],
            )
            .is_none());
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
