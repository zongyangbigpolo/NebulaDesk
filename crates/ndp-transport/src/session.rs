//! An end-to-end encrypted, channel-multiplexed session over one QUIC
//! connection by default, or a negotiated multipath logical session.
//!
//! A session owns the Noise handshake, the mapping from [`Channel`] to QUIC
//! carrier, and a set of background readers that decrypt inbound records and
//! deliver them on one queue. Callers see plaintext [`Incoming`] messages and
//! never touch quinn directly.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::{BufMut, BytesMut};
use ndp_crypto::{Initiator, PublicKey, RecordOpener, RecordSealer, Responder};
use ndp_proto::{Channel, MsgFlags, MsgHeader, KEYFRAME_PRIORITY};
use tokio::sync::{mpsc, Mutex};
use tokio::time::Instant;
use tracing::{debug, trace, warn};

use crate::config::TransportConfig;
use crate::multipath::{Multipath, PathKind, PathState, PathStats};
use crate::wire::{self, CHANNEL_PREFIX_LEN, LENGTH_PREFIX_LEN};
use crate::{Result, TransportError};

/// Largest handshake message accepted, before any key is established.
const MAX_HANDSHAKE_MSG: usize = 8 * 1024;

/// Depth of the inbound queue. Deep enough to absorb a burst of small
/// messages, shallow enough that a stalled consumer applies backpressure
/// instead of growing memory without bound.
const INBOUND_QUEUE: usize = 256;

/// Sent when a video frame is abandoned because it went stale in the send
/// queue. Purely informational: the peer drops the partial frame.
const CODE_STALE_FRAME: u32 = 0x10;

/// Dropping a quinn writer normally finishes it, which would expose a
/// truncated encrypted record. An interrupted video send must reset instead.
struct VideoStream {
    stream: quinn::SendStream,
    finished: bool,
}

impl Drop for VideoStream {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.stream.reset(quinn::VarInt::from_u32(CODE_STALE_FRAME));
        }
    }
}

/// Channels that ride a single long-lived ordered stream.
///
/// Everything else opens a carrier per message, which is why only these two
/// demand strictly increasing sequence numbers in the record layer.
const ORDERED_CHANNELS: [Channel; 2] = [Channel::Control, Channel::Input];

/// A decrypted inbound message.
#[derive(Debug, Clone)]
pub struct Incoming {
    /// The channel it arrived on.
    pub channel: Channel,
    /// The authenticated record header. In multipath mode `seq` is restored
    /// from the authenticated logical envelope, not the native path counter.
    pub header: MsgHeader,
    /// The decrypted payload.
    pub payload: Vec<u8>,
}

/// What happened to a video frame handed to [`Session::send_video_frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The frame was written to its stream.
    Sent,
    /// The frame went stale before congestion control let it out, so its
    /// stream was reset. Normal under load and not an error: a late frame is
    /// worth less than the bandwidth it would consume.
    Discarded,
}

/// The receiving half of a session.
#[derive(Debug)]
pub struct SessionReceiver {
    pub(crate) rx: mpsc::Receiver<Result<Incoming>>,
    pub(crate) _multipath: Option<Arc<Multipath>>,
}

impl SessionReceiver {
    /// Await the next message. Returns `None` once the session is finished.
    pub async fn recv(&mut self) -> Option<Result<Incoming>> {
        self.rx.recv().await
    }
}

/// The sending half of a session. Cheap to clone and share across tasks.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
    multipath: Option<Arc<Multipath>>,
}

struct Inner {
    conn: quinn::Connection,
    sealer: RecordSealer,
    /// Long-lived ordered carriers, one per ordered channel.
    ordered: HashMap<Channel, Mutex<quinn::SendStream>>,
    max_record: usize,
    peer_static: Option<PublicKey>,
    readers: Vec<tokio::task::AbortHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"session dropped");
        for reader in &self.readers {
            reader.abort();
        }
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("remote", &self.inner.conn.remote_address())
            .field("peer_static", &self.inner.peer_static)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Establish a session as the initiator (the client).
    ///
    /// `payload` travels inside the first Noise message, encrypted to the
    /// agent's static key before the agent has said anything: that is where
    /// the manager-signed session ticket goes. Returns the payload the agent
    /// sent back in its reply.
    pub async fn initiate(
        conn: quinn::Connection,
        mut initiator: Initiator,
        payload: &[u8],
        config: &TransportConfig,
    ) -> Result<(Self, SessionReceiver, Vec<u8>)> {
        // Open both ordered carriers up front, each tagged with its channel
        // id, so the responder classifies them without depending on the order
        // in which QUIC surfaces them.
        let mut sends = HashMap::new();
        let mut recvs = HashMap::new();
        for channel in ORDERED_CHANNELS {
            let (mut send, recv) = conn.open_bi().await?;
            let _ = send.set_priority(i32::from(channel.priority()));
            send.write_all(&wire::channel_prefix(channel, channel.priority()))
                .await?;
            sends.insert(channel, send);
            recvs.insert(channel, recv);
        }

        let msg1 = initiator.write_first(payload)?;
        write_framed(
            sends.get_mut(&Channel::Control).expect("just inserted"),
            &msg1,
            MAX_HANDSHAKE_MSG,
        )
        .await?;

        let msg2 = read_framed(
            recvs.get_mut(&Channel::Control).expect("just inserted"),
            MAX_HANDSHAKE_MSG,
        )
        .await?
        .ok_or(TransportError::Truncated {
            carrier: "handshake",
            got: 0,
        })?;

        let done = initiator.read_second(&msg2)?;
        let peer_payload = done.peer_payload;
        let (session, receiver) = Self::start(
            conn,
            done.sealer,
            done.opener,
            done.peer_static,
            sends,
            recvs,
            config,
        );
        Ok((session, receiver, peer_payload))
    }

    /// Establish a session as the responder (the agent).
    ///
    /// `authorize` receives the initiator's decrypted handshake payload — the
    /// session ticket — and either rejects the session or returns the agent's
    /// own reply payload. It runs *before* the reply is generated, so an
    /// unauthorised client learns nothing beyond "refused".
    pub async fn accept<F>(
        conn: quinn::Connection,
        mut responder: Responder,
        authorize: F,
        config: &TransportConfig,
    ) -> Result<(Self, SessionReceiver)>
    where
        F: FnOnce(&[u8]) -> std::result::Result<Vec<u8>, String>,
    {
        let mut sends = HashMap::new();
        let mut recvs = HashMap::new();
        for _ in ORDERED_CHANNELS {
            let (send, mut recv) = conn.accept_bi().await?;
            let mut prefix = [0u8; CHANNEL_PREFIX_LEN];
            recv.read_exact(&mut prefix)
                .await
                .map_err(|_| TransportError::Truncated {
                    carrier: "channel prefix",
                    got: 0,
                })?;
            let channel = wire::parse_channel(&prefix)?;
            if !ORDERED_CHANNELS.contains(&channel) {
                return Err(TransportError::WrongCarrier {
                    channel: channel.name(),
                    carrier: "long-lived bidirectional stream",
                });
            }
            sends.insert(channel, send);
            recvs.insert(channel, recv);
        }
        // A peer that tagged both carriers with the same channel would leave
        // the other channel without a writer; refuse rather than half-work.
        if sends.len() != ORDERED_CHANNELS.len() {
            return Err(TransportError::WrongCarrier {
                channel: "duplicate",
                carrier: "long-lived bidirectional stream",
            });
        }

        let msg1 = read_framed(
            recvs.get_mut(&Channel::Control).expect("checked above"),
            MAX_HANDSHAKE_MSG,
        )
        .await?
        .ok_or(TransportError::Truncated {
            carrier: "handshake",
            got: 0,
        })?;

        let ticket = responder.read_first(&msg1)?.to_vec();
        let reply_payload =
            authorize(&ticket).map_err(|reason| TransportError::Rejected { reason })?;

        let (msg2, done) = responder.write_second(&reply_payload)?;
        write_framed(
            sends.get_mut(&Channel::Control).expect("checked above"),
            &msg2,
            MAX_HANDSHAKE_MSG,
        )
        .await?;

        Ok(Self::start(
            conn,
            done.sealer,
            done.opener,
            done.peer_static,
            sends,
            recvs,
            config,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        conn: quinn::Connection,
        sealer: RecordSealer,
        opener: RecordOpener,
        peer_static: Option<PublicKey>,
        sends: HashMap<Channel, quinn::SendStream>,
        recvs: HashMap<Channel, quinn::RecvStream>,
        config: &TransportConfig,
    ) -> (Self, SessionReceiver) {
        let (tx, rx) = mpsc::channel(INBOUND_QUEUE);
        let max_record = config.max_record;
        let mut readers = Vec::new();

        for (channel, recv) in recvs {
            readers.push(
                tokio::spawn(read_ordered(
                    channel,
                    recv,
                    opener.clone(),
                    tx.clone(),
                    max_record,
                ))
                .abort_handle(),
            );
        }
        readers.push(
            tokio::spawn(accept_streams(
                conn.clone(),
                opener.clone(),
                tx.clone(),
                max_record,
            ))
            .abort_handle(),
        );
        readers.push(
            tokio::spawn(read_datagrams(conn.clone(), opener, tx, max_record)).abort_handle(),
        );

        let session = Self {
            inner: Arc::new(Inner {
                conn,
                sealer,
                ordered: sends
                    .into_iter()
                    .map(|(ch, s)| (ch, Mutex::new(s)))
                    .collect(),
                max_record,
                peer_static,
                readers,
            }),
            multipath: None,
        };
        (
            session,
            SessionReceiver {
                rx,
                _multipath: None,
            },
        )
    }

    /// Opt into `multipath/1` after both peers agreed inside authenticated
    /// handshake payloads. The existing connection is the initial relay path.
    ///
    /// Never use this with a legacy peer: all subsequent payloads are enveloped.
    /// Both paths must support QUIC datagrams for end-to-end probes and ACKs.
    /// The relay has a bounded startup grace until the first authenticated
    /// multipath envelope proves the peer installed its readers; direct-path
    /// blackhole detection does not inherit that grace.
    /// Existing clones made before this call remain native; do not use them to
    /// send application messages after conversion.
    #[must_use]
    pub fn into_multipath(mut self, receiver: SessionReceiver) -> (Self, SessionReceiver) {
        if self.multipath.is_some() {
            return (self, receiver);
        }
        let (multipath, receiver) = Multipath::start(self.clone(), receiver);
        self.multipath = Some(multipath);
        (self, receiver)
    }

    /// Attach a fresh, mutually authenticated direct Noise/QUIC connection.
    ///
    /// Its proven peer static key must match the relay session. Direct is
    /// selected only after end-to-end probes demonstrate a sustained advantage.
    /// At most one direct connection is retained; rejected candidates are closed.
    pub async fn attach_direct(&self, direct: Session, receiver: SessionReceiver) -> Result<()> {
        match &self.multipath {
            Some(multipath) => multipath.attach(direct, receiver),
            None => {
                direct.close_native(0, b"multipath not negotiated");
                Err(TransportError::Config("multipath not negotiated".into()))
            }
        }
    }

    /// Report local direct-interface loss, retiring only the attached direct
    /// connection through normal path-failure handling.
    ///
    /// This synchronous notification also removes a standby direct path. If
    /// direct was selected, retained reliable records resume on the relay and
    /// the path epoch changes. If no other path survives, the receiver reports
    /// terminal failure. Returns [`TransportError::NoDirectPath`] if no direct
    /// connection is attached. Use [`Self::close`] for logical revocation.
    pub fn disconnect_direct(&self) -> Result<()> {
        self.multipath
            .as_ref()
            .ok_or(TransportError::NoDirectPath)?
            .disconnect_direct()
    }

    /// Subscribe to route changes and peer media-epoch discontinuities.
    ///
    /// A new epoch means pending video should be cancelled and the decoder
    /// reference chain refreshed. It is not a delivery or presentation ACK.
    #[must_use]
    pub fn path_changes(&self) -> Option<tokio::sync::watch::Receiver<PathState>> {
        self.multipath.as_ref().map(|m| m.path_changes())
    }

    /// QUIC statistics for the currently selected hop (not end-to-end relay RTT).
    #[must_use]
    pub fn connection_stats(&self) -> quinn::ConnectionStats {
        self.active_connection().stats()
    }

    /// Snapshot retained paths without changing the media epoch or its watch.
    ///
    /// A native session is reported as its initial `Relay` path, without an
    /// end-to-end probe RTT. Retired multipath connections are omitted, and a
    /// closed session returns an empty vector. Counter deltas should be keyed
    /// by `path_id`, since a newly attached connection starts fresh counters.
    #[must_use]
    pub fn path_stats(&self) -> Vec<PathStats> {
        if let Some(multipath) = &self.multipath {
            return multipath.path_stats();
        }
        if self.inner.conn.close_reason().is_some() {
            return Vec::new();
        }
        let stats = self.inner.conn.stats();
        vec![PathStats {
            kind: PathKind::Relay,
            path_id: 0,
            active: true,
            udp_tx_bytes: stats.udp_tx.bytes,
            udp_rx_bytes: stats.udp_rx.bytes,
            end_to_end_rtt: None,
        }]
    }

    fn active_connection(&self) -> quinn::Connection {
        self.multipath
            .as_ref()
            .and_then(|m| m.connection())
            .unwrap_or_else(|| self.inner.conn.clone())
    }

    pub(crate) fn native_connection(&self) -> quinn::Connection {
        self.inner.conn.clone()
    }

    pub(crate) fn native_limit(&self) -> usize {
        self.inner.max_record
    }

    pub(crate) fn is_multipath(&self) -> bool {
        self.multipath.is_some()
    }

    /// The peer's cryptographically proven static public key, if the pattern
    /// revealed one.
    #[must_use]
    pub fn peer_static(&self) -> Option<PublicKey> {
        self.inner.peer_static
    }

    /// The peer's current address. Changes if QUIC migrates the path.
    #[must_use]
    pub fn remote_address(&self) -> std::net::SocketAddr {
        self.active_connection().remote_address()
    }

    /// Send one message, choosing the carrier that matches the channel.
    ///
    /// Video frames should use [`Session::send_video_frame`] instead so a
    /// stale frame can be abandoned rather than queued behind congestion.
    pub async fn send(&self, channel: Channel, header: MsgHeader, payload: &[u8]) -> Result<()> {
        if let Some(multipath) = &self.multipath {
            return multipath.send(channel, header, payload).await;
        }
        self.send_native(channel, header, payload).await
    }

    pub(crate) async fn send_native(
        &self,
        channel: Channel,
        header: MsgHeader,
        payload: &[u8],
    ) -> Result<()> {
        wire::check_len(
            payload
                .len()
                .saturating_add(ndp_proto::HEADER_LEN + ndp_crypto::AEAD_TAG_LEN),
            self.inner.max_record,
        )?;
        if let Some(stream) = self.inner.ordered.get(&channel) {
            // Allocate the native sequence only after acquiring the writer:
            // concurrent callers must not put strict-channel records on wire
            // in a different order from their Noise counters.
            let mut guard = stream.lock().await;
            let record = self.inner.sealer.seal(channel, header, payload)?;
            let mut interrupted = InterruptedOrderedWrite {
                conn: &self.inner.conn,
                channel,
                complete: false,
            };
            write_framed(&mut guard, &record, self.inner.max_record).await?;
            interrupted.complete = true;
            return Ok(());
        }
        let record = self.inner.sealer.seal(channel, header, payload)?;
        if channel.is_unreliable() {
            let mut buf = BytesMut::with_capacity(CHANNEL_PREFIX_LEN + record.len());
            buf.put_slice(&wire::channel_prefix(channel, channel.priority()));
            buf.put_slice(&record);
            self.inner.conn.send_datagram(buf.freeze())?;
            return Ok(());
        }
        let mut message = VideoStream {
            stream: self.inner.conn.open_uni().await?,
            finished: false,
        };
        let _ = message.stream.set_priority(i32::from(channel.priority()));
        message
            .stream
            .write_all(&wire::channel_prefix(channel, channel.priority()))
            .await?;
        message.stream.write_all(&record).await?;
        message
            .stream
            .finish()
            .map_err(|_| TransportError::Closed)?;
        message.finished = true;
        Ok(())
    }

    pub(crate) fn send_datagram_native(&self, header: MsgHeader, payload: &[u8]) -> Result<()> {
        wire::check_len(
            payload
                .len()
                .saturating_add(ndp_proto::HEADER_LEN + ndp_crypto::AEAD_TAG_LEN),
            self.inner.max_record,
        )?;
        let record = self.inner.sealer.seal(Channel::Audio, header, payload)?;
        let mut buf = BytesMut::with_capacity(CHANNEL_PREFIX_LEN + record.len());
        buf.put_slice(&wire::channel_prefix(
            Channel::Audio,
            Channel::Audio.priority(),
        ));
        buf.put_slice(&record);
        self.inner.conn.send_datagram(buf.freeze())?;
        Ok(())
    }

    /// Send a video frame, abandoning it if it is still queued at `deadline`.
    ///
    /// This is the reason video gets a stream per frame: under congestion the
    /// stream is reset and the bytes never leave, so the next frame starts
    /// from a clean queue instead of trailing a second of stale video.
    /// The deadline includes waiting for stream credit and writing the prefix.
    /// Cancelling this future resets any unfinished stream as well.
    pub async fn send_video_frame(
        &self,
        header: MsgHeader,
        payload: &[u8],
        deadline: Option<Instant>,
    ) -> Result<FrameOutcome> {
        if let Some(multipath) = &self.multipath {
            return multipath.send_video(header, payload, deadline).await;
        }
        self.send_video_native(header, payload, deadline).await
    }

    pub(crate) async fn send_video_native(
        &self,
        header: MsgHeader,
        payload: &[u8],
        deadline: Option<Instant>,
    ) -> Result<FrameOutcome> {
        wire::check_len(
            payload
                .len()
                .saturating_add(ndp_proto::HEADER_LEN + ndp_crypto::AEAD_TAG_LEN),
            self.inner.max_record,
        )?;
        let record = self.inner.sealer.seal(Channel::Video, header, payload)?;
        // A keyframe outranks the frames that depend on it. See
        // `Channel::priority` for why the rest of the ladder looks as it does.
        let urgency = if header.flags.contains(MsgFlags::KEYFRAME) {
            KEYFRAME_PRIORITY
        } else {
            Channel::Video.priority()
        };
        let write = async {
            let mut video = VideoStream {
                stream: self.inner.conn.open_uni().await?,
                finished: false,
            };
            let _ = video.stream.set_priority(i32::from(urgency));
            video
                .stream
                .write_all(&wire::channel_prefix(Channel::Video, urgency))
                .await?;
            video.stream.write_all(&record).await?;
            video.stream.finish().map_err(|_| TransportError::Closed)?;
            video.finished = true;
            Ok::<_, TransportError>(())
        };

        match deadline {
            None => {
                write.await?;
                Ok(FrameOutcome::Sent)
            }
            Some(at) => match tokio::time::timeout_at(at, write).await {
                Ok(Ok(())) => Ok(FrameOutcome::Sent),
                Ok(Err(e)) => Err(e),
                Err(_) => {
                    trace!("video frame discarded: send deadline passed");
                    Ok(FrameOutcome::Discarded)
                }
            },
        }
    }

    /// Close the connection, telling the peer why.
    ///
    /// In multipath mode this revokes both paths. The QUIC application-close
    /// code reserves high bits for a logical-close marker and retains `code`
    /// in its low 32 bits, distinguishing revocation from a failed direct hop.
    pub fn close(&self, code: u32, reason: &[u8]) {
        if let Some(multipath) = &self.multipath {
            multipath.close(code, reason);
            return;
        }
        self.close_native(code, reason);
    }

    pub(crate) fn close_native(&self, code: u32, reason: &[u8]) {
        self.close_native_code(quinn::VarInt::from_u32(code), reason);
    }

    pub(crate) fn close_native_code(&self, code: quinn::VarInt, reason: &[u8]) {
        self.inner.conn.close(code, reason);
        for reader in &self.inner.readers {
            reader.abort();
        }
    }

    /// Largest audio record that fits in a datagram on the current path, if
    /// the peer supports datagrams at all.
    #[must_use]
    pub fn max_audio_record(&self) -> Option<usize> {
        self.active_connection().max_datagram_size().map(|n| {
            n.saturating_sub(
                CHANNEL_PREFIX_LEN
                    + if self.multipath.is_some() {
                        crate::multipath::ENVELOPE_LEN
                    } else {
                        0
                    },
            )
        })
    }
}

struct InterruptedOrderedWrite<'a> {
    conn: &'a quinn::Connection,
    channel: Channel,
    complete: bool,
}

impl Drop for InterruptedOrderedWrite<'_> {
    fn drop(&mut self) {
        if !self.complete {
            // A cancelled length-prefixed write cannot safely reuse its stream.
            warn!(
                channel = self.channel.name(),
                "closing native path after interrupted ordered record"
            );
            self.conn
                .close(0x11u32.into(), b"interrupted ordered record");
        }
    }
}

// --- background readers ---------------------------------------------------

/// Deliver a reader's fatal error, then stop. A closed queue means the
/// consumer is gone and there is nothing left to report to.
async fn fail(tx: &mpsc::Sender<Result<Incoming>>, err: TransportError) {
    let _ = tx.send(Err(err)).await;
}

async fn read_ordered(
    channel: Channel,
    mut recv: quinn::RecvStream,
    opener: RecordOpener,
    tx: mpsc::Sender<Result<Incoming>>,
    max_record: usize,
) {
    loop {
        let record = match read_framed(&mut recv, max_record).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                debug!(channel = channel.name(), "ordered carrier closed cleanly");
                return fail(&tx, TransportError::Closed).await;
            }
            Err(e) => return fail(&tx, e).await,
        };
        match open_and_send(channel, &record, &opener, &tx).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => return fail(&tx, e).await,
        }
    }
}

async fn accept_streams(
    conn: quinn::Connection,
    opener: RecordOpener,
    tx: mpsc::Sender<Result<Incoming>>,
    max_record: usize,
) {
    let mut readers = tokio::task::JoinSet::new();
    loop {
        let recv = match tokio::select! {
            result = conn.accept_uni(), if readers.len() < INBOUND_QUEUE => result,
            Some(_) = readers.join_next(), if !readers.is_empty() => continue,
            _ = tx.closed() => return,
        } {
            Ok(r) => r,
            Err(quinn::ConnectionError::ApplicationClosed(_))
            | Err(quinn::ConnectionError::LocallyClosed) => return,
            Err(e) => return fail(&tx, e.into()).await,
        };
        // One task per stream: video frames are decrypted concurrently, which
        // is safe because the record layer guards replay state per channel.
        readers.spawn(read_message_stream(
            recv,
            opener.clone(),
            tx.clone(),
            max_record,
        ));
    }
}

async fn read_message_stream(
    mut recv: quinn::RecvStream,
    opener: RecordOpener,
    tx: mpsc::Sender<Result<Incoming>>,
    max_record: usize,
) {
    let mut prefix = [0u8; CHANNEL_PREFIX_LEN];
    if recv.read_exact(&mut prefix).await.is_err() {
        // A stream reset before its header arrived is an abandoned message.
        return;
    }
    let channel = match wire::parse_channel(&prefix) {
        Ok(c) => c,
        Err(e) => return fail(&tx, e).await,
    };
    if !channel.is_stream_per_message() {
        // Control and Input must stay on their ordered carriers; accepting
        // them here would silently bypass strict sequencing.
        return fail(
            &tx,
            TransportError::WrongCarrier {
                channel: channel.name(),
                carrier: "per-message stream",
            },
        )
        .await;
    }

    let record = match recv.read_to_end(max_record).await {
        Ok(r) => r,
        Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(_))) => {
            trace!(channel = channel.name(), "peer discarded a stale message");
            return;
        }
        Err(quinn::ReadToEndError::TooLong) => {
            return fail(
                &tx,
                TransportError::RecordTooLarge {
                    len: max_record + 1,
                    limit: max_record,
                },
            )
            .await
        }
        Err(e) => return fail(&tx, e.into()).await,
    };

    match open_and_send(channel, &record, &opener, &tx).await {
        Err(TransportError::Crypto(ndp_crypto::CryptoError::Replay { seq, .. }))
            if channel == Channel::Video =>
        {
            // A relay can deliver a superseded frame after the replay window
            // has advanced. Reject that frame without closing a healthy session.
            if let Ok((header, _)) = MsgHeader::split(&record) {
                if header.flags.contains(MsgFlags::KEYFRAME) {
                    warn!(
                        native_seq = seq,
                        timestamp_us = header.timestamp_us,
                        bytes = record.len(),
                        "discarding stale or duplicate native video record marked keyframe"
                    );
                }
            }
            trace!(seq, "discarded stale or duplicate video record");
        }
        Err(error) => fail(&tx, error).await,
        Ok(_) => {}
    }
}

async fn read_datagrams(
    conn: quinn::Connection,
    opener: RecordOpener,
    tx: mpsc::Sender<Result<Incoming>>,
    max_record: usize,
) {
    loop {
        let datagram = match conn.read_datagram().await {
            Ok(d) => d,
            Err(quinn::ConnectionError::ApplicationClosed(_))
            | Err(quinn::ConnectionError::LocallyClosed) => return,
            Err(e) => return fail(&tx, e.into()).await,
        };
        if datagram.len() <= CHANNEL_PREFIX_LEN {
            trace!("dropping runt datagram");
            continue;
        }
        let (prefix, record) = datagram.split_at(CHANNEL_PREFIX_LEN);
        let mut id = [0u8; CHANNEL_PREFIX_LEN];
        id.copy_from_slice(prefix);
        let channel = match wire::parse_channel(&id) {
            Ok(c) => c,
            // A malformed datagram is one lost audio packet, not a reason to
            // tear down the session.
            Err(_) => continue,
        };
        if !channel.is_unreliable() || wire::check_len(record.len(), max_record).is_err() {
            continue;
        }
        // Individual datagram failures are tolerated: anyone on the path can
        // inject a forged UDP payload, and that must not be able to kill a
        // session.
        match opener.open(channel, record) {
            Ok((header, payload)) => {
                if tx
                    .send(Ok(Incoming {
                        channel,
                        header,
                        payload,
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => trace!(channel = channel.name(), error = %e, "dropping datagram"),
        }
    }
}

/// Decrypt one record and hand it to the consumer.
///
/// Returns `false` when the consumer has gone away.
async fn open_and_send(
    channel: Channel,
    record: &[u8],
    opener: &RecordOpener,
    tx: &mpsc::Sender<Result<Incoming>>,
) -> Result<bool> {
    let (header, payload) = opener.open(channel, record)?;
    Ok(tx
        .send(Ok(Incoming {
            channel,
            header,
            payload,
        }))
        .await
        .is_ok())
}

// --- framing helpers ------------------------------------------------------

async fn write_framed(stream: &mut quinn::SendStream, msg: &[u8], limit: usize) -> Result<()> {
    stream.write_all(&wire::frame(msg, limit)?).await?;
    Ok(())
}

async fn read_framed(stream: &mut quinn::RecvStream, limit: usize) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; LENGTH_PREFIX_LEN];
    match stream.read_exact(&mut len_buf).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(quinn::ReadExactError::FinishedEarly(got)) => {
            return Err(TransportError::Truncated {
                carrier: "length prefix",
                got,
            })
        }
        Err(quinn::ReadExactError::ReadError(e)) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    wire::check_len(len, limit)?;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await.map_err(|e| match e {
        quinn::ReadExactError::FinishedEarly(got) => TransportError::Truncated {
            carrier: "record body",
            got,
        },
        quinn::ReadExactError::ReadError(e) => e.into(),
    })?;
    Ok(Some(buf))
}

#[cfg(test)]
mod tests;
