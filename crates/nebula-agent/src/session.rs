//! Serving one session.
//!
//! By the time this runs, authorisation is already settled: the manager
//! decided it, the gateway carried it, and the policy in the request is the
//! whole of what this session may do. The agent's remaining job is to prove
//! the peer really is the client the gateway named, and then to move media.

use std::sync::Arc;
use std::time::Duration;

use ndp_crypto::{PublicKey, Responder, StaticKeypair};
use ndp_proto::{Channel, InputEvent, MsgFlags, MsgHeader, MsgKind};
use ndp_signal::direct::{SessionHello, MULTIPATH_GREETING};
use ndp_signal::{RelayHello, RelayHelloAck, SessionRequest};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Incoming, Session, SessionReceiver,
    TransportConfig, ALPN_RELAY,
};
use tokio::sync::{mpsc, oneshot};

use crate::clipboard;
use crate::files;
use crate::media::{
    AudioConfig, AudioSink, AudioSource, EncodedAudio, EncodedFrame, FrameSink, Platform,
    VideoConfig, VideoSource,
};

/// How many encoded frames may wait for the network.
///
/// Small on purpose. A backlog of video is not useful data, it is latency
/// that has already been committed to; dropping at the source lets the
/// encoder recover with a keyframe instead of delivering a slideshow.
const FRAME_QUEUE: usize = 3;

/// How long a frame may sit in the transport before it is abandoned.
const FRAME_DEADLINE: Duration = Duration::from_millis(120);

/// How many encoded audio packets may wait for the network.
///
/// Deeper than video because a gap in sound is more noticeable than a
/// dropped frame, and still short: eight 20 ms packets is 160 ms, past which
/// nobody would rather hear the audio than skip to the present.
const AUDIO_QUEUE: usize = 8;

type VideoSend = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = ndp_transport::Result<(ndp_transport::FrameOutcome, usize)>,
            > + Send,
    >,
>;

/// The Noise prologue binding a handshake to one session.
///
/// Both sides derive it from a session id they were told separately, so a
/// handshake recorded from one session cannot be replayed into another.
#[must_use]
pub fn prologue(session: nebula_common::SessionId) -> Vec<u8> {
    format!("ndp/3 session {session}").into_bytes()
}

/// What a finished session reports back up the control tunnel.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tally {
    /// Bytes of media the agent sent to the client.
    pub sent: u64,
    /// Bytes the agent received from the client.
    pub received: u64,
}

/// Connect to the relay, complete the handshake, and serve until it ends.
pub async fn serve(
    request: SessionRequest,
    keys: StaticKeypair,
    platform: Arc<dyn Platform>,
    mut stop: oneshot::Receiver<String>,
) -> anyhow::Result<Tally> {
    let expected = PublicKey::from_slice(
        &hex::decode(&request.client_key)
            .map_err(|_| anyhow::anyhow!("the client key in the session request is not hex"))?,
    )?;

    let establish = async {
        let config = TransportConfig::default();
        let endpoint = client_endpoint("0.0.0.0:0".parse().expect("literal address"))?;
        let pin = CertificateFingerprint::from_hex(&request.relay_pin).map_err(|_| {
            anyhow::anyhow!("the relay pin in the session request is not a fingerprint")
        })?;
        let conn = connect(
            &endpoint,
            request
                .relay_addr
                .parse()
                .map_err(|_| anyhow::anyhow!("the relay address is not a socket address"))?,
            "localhost",
            pin,
            ALPN_RELAY,
            &config,
        )
        .await?;

        let (mut send, mut recv) = conn.open_bi().await?;
        ndp_signal::write_message(
            &mut send,
            &RelayHello {
                pair_token: request.pair_token.clone(),
            },
        )
        .await?;
        match ndp_signal::read_message::<RelayHelloAck>(&mut recv).await? {
            RelayHelloAck::Spliced => {}
            RelayHelloAck::Rejected { reason } => {
                anyhow::bail!("the relay refused this session: {reason}")
            }
        }

        let responder = Responder::new(&keys, &prologue(request.session))?;
        let mut multipath = false;
        let (session, receiver) = Session::accept(
            conn,
            responder,
            |ticket| {
                // The agent cannot check the ticket's signature: it holds no
                // manager keys, and giving it some would put a verification
                // dependency on the machine least able to keep one current. The
                // ticket is only evidence that the peer got this far.
                multipath = session_multipath(ticket)?;
                Ok(if multipath {
                    MULTIPATH_GREETING
                } else {
                    b"agent-ready"
                }
                .to_vec())
            },
            &config,
        )
        .await?;

        // This is the check that makes a compromised gateway or relay unable to
        // insert itself: only the holder of the private key the gateway named can
        // have completed the handshake.
        match session.peer_static() {
            Some(key) if key.as_bytes() == expected.as_bytes() => {}
            _ => {
                session.close(0x20, b"unexpected peer");
                anyhow::bail!("the peer is not the client this session was set up for");
            }
        }
        Ok::<_, anyhow::Error>((session, receiver, multipath))
    };
    let (session, receiver, multipath) = tokio::select! {
        biased;
        reason = &mut stop => {
            tracing::info!(session = %request.session, reason = ?reason, "session establishment cancelled");
            return Ok(Tally::default());
        }
        result = tokio::time::timeout(Duration::from_secs(20), establish) => result??,
    };
    let (session, receiver) = if multipath {
        session.into_multipath(receiver)
    } else {
        (session, receiver)
    };
    let _lifetime = SessionLifetime(session.clone());
    let _direct = multipath.then(|| {
        crate::direct::DirectService::spawn(request.session, keys, expected, session.clone())
    });

    tracing::info!(session = %request.session, "session established");
    let tally = pump(&request, session, receiver, platform, stop).await;
    Ok(tally)
}

struct SessionLifetime(Session);

fn session_multipath(ticket: &[u8]) -> Result<bool, String> {
    if ticket.first() == Some(&b'{') {
        let hello: SessionHello =
            serde_json::from_slice(ticket).map_err(|_| "malformed session hello".to_string())?;
        if hello.ticket.is_empty() {
            return Err("no ticket was presented".into());
        }
        Ok(hello.multipath)
    } else if ticket.is_empty() {
        Err("no ticket was presented".into())
    } else {
        Ok(false)
    }
}

impl Drop for SessionLifetime {
    fn drop(&mut self) {
        self.0.close(0, b"agent session ended");
    }
}

/// Move media until one side goes away.
async fn pump(
    request: &SessionRequest,
    session: Session,
    receiver: SessionReceiver,
    platform: Arc<dyn Platform>,
    mut stop: oneshot::Receiver<String>,
) -> Tally {
    let mut tally = Tally::default();
    let mut media = Box::pin(pump_media(
        request,
        session.clone(),
        receiver,
        platform,
        &mut tally,
    ));
    tokio::select! {
        biased;
        reason = &mut stop => {
            let reason = reason.unwrap_or_else(|_| "the session was ended".into());
            tracing::info!(session = %request.session, %reason, "ending the session");
            // Reliable writes can await credit indefinitely while probes stay
            // healthy. Revoke the transport before cancelling any native cleanup.
            session.close(0x22, reason.as_bytes());
        }
        () = &mut media => {}
    }
    drop(media);
    tracing::info!(session = %request.session, sent = tally.sent, received = tally.received, "session ended");
    tally
}

async fn pump_media(
    request: &SessionRequest,
    session: Session,
    mut receiver: SessionReceiver,
    platform: Arc<dyn Platform>,
    tally: &mut Tally,
) {
    let platform = match platform.session_scope(request.policy.input) {
        Ok(Some(scoped)) => scoped,
        Ok(None) => platform,
        Err(error) => {
            tracing::error!(%error, "could not create the session's media scope");
            session.close(0x21, b"media scope failed");
            return;
        }
    };
    let video = match platform.video() {
        Ok(video) => video,
        Err(error) => {
            tracing::error!(%error, "no video source; refusing the session");
            session.close(0x21, b"no video source");
            return;
        }
    };
    let mut input = match input_for_session(platform.as_ref(), request.policy.input) {
        Ok(input) => input,
        Err(error) => {
            // Not fatal: watching a machine you cannot control is still
            // worth something, and it is a legitimate configuration. But it
            // is never what someone expects by accident.
            tracing::warn!(%error, "input is unavailable; this session can only watch");
            None
        }
    };

    let (frames_tx, mut frames) = mpsc::channel::<EncodedFrame>(FRAME_QUEUE);
    let mut video = match start_video(video, VideoConfig::default(), frames_tx).await {
        Ok(video) => video,
        Err(error) => {
            tracing::error!(%error, "the video source refused to start");
            session.close(0x21, b"capture failed");
            return;
        }
    };

    let audio_config = AudioConfig::default();
    let audio_info = ndp_proto::AudioFrameInfo {
        codec: ndp_proto::AudioCodec::Opus,
        channels: audio_config.channels as u8,
        frame_ms: 20,
    };

    // Audio is optional in both directions: the entitlement may withhold it,
    // and a platform backend may not have it yet. Neither is a reason to
    // refuse a session, so a failure here is a session without sound.
    let (audio_tx, audio_rx) = mpsc::channel::<EncodedAudio>(AUDIO_QUEUE);
    let mut starting_audio = request
        .policy
        .audio
        .then(|| Box::pin(start_audio(platform.clone(), audio_config, audio_tx)));
    let mut pending_packets = request.policy.audio.then_some(audio_rx);
    let mut audio = None;
    let mut packets = None;

    // The clipboard runs on its own thread and only when the entitlement
    // allows it, so a session without the permission never opens the
    // machine's clipboard at all.
    let mut clipboard = if request.policy.clipboard {
        match platform.clipboard() {
            Ok(board) => Some(clipboard::spawn(board, true)),
            Err(error) => {
                tracing::warn!(%error, "no clipboard on this machine; the session will not share one");
                None
            }
        }
    } else {
        None
    };

    // File transfer, likewise, exists only where it is permitted.
    let mut transfers = request
        .policy
        .file_transfer
        .then(|| files::spawn(files::default_downloads(), true));

    let mut seq: u32 = 0;
    let mut audio_seq: u32 = 0;
    // Keep a single send alive across select iterations. Awaiting it inside
    // the frame arm would stop input and revocation behind a congested IDR.
    let mut sending: Option<VideoSend> = None;
    let mut path_changes = session.path_changes();
    loop {
        tokio::select! {
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
                    tracing::info!(session = %request.session, ?path, "session media path changed");
                }
                sending = None;
                while frames.try_recv().is_ok() {}
                video.source.request_keyframe();
            }

            started = async {
                match starting_audio.as_mut() {
                    Some(starting) => starting.await,
                    None => std::future::pending().await,
                }
            } => {
                starting_audio = None;
                match started {
                    Ok(Some(started)) => {
                        audio = Some(started);
                        packets = pending_packets.take();
                    }
                    Ok(None) => {
                        pending_packets = None;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "audio capture would not start; this session is silent");
                        pending_packets = None;
                    }
                }
            }

            frame = frames.recv(), if sending.is_none() => {
                let Some(frame) = frame else { break };
                tracing::trace!(
                    timestamp_us = frame.timestamp_us,
                    keyframe = frame.keyframe,
                    bytes = frame.data.len(),
                    "video source frame"
                );
                let flags = if frame.keyframe { MsgFlags::KEYFRAME } else { MsgFlags::DISCARDABLE };
                let header = MsgHeader::new(MsgKind::VideoFrame, seq, frame.timestamp_us)
                    .with_flags(flags);
                seq = seq.wrapping_add(1);

                // A keyframe is the one frame that must not be abandoned. It
                // is also the largest, so on a link that cannot carry it
                // inside the deadline it is the frame that always loses —
                // and the agent answers a discarded frame by producing
                // another keyframe, which loses in turn. The result is a
                // session that never shows a picture at all.
                let deadline = (!frame.keyframe)
                    .then(|| tokio::time::Instant::now() + FRAME_DEADLINE);
                let sender = session.clone();
                sending = Some(Box::pin(async move {
                    let outcome = sender.send_video_frame(header, &frame.data, deadline).await?;
                    Ok((outcome, frame.data.len()))
                }));
            }

            result = async {
                match sending.as_mut() {
                    Some(send) => send.await,
                    None => std::future::pending().await,
                }
            } => {
                sending = None;
                match result {
                    Ok((ndp_transport::FrameOutcome::Sent, bytes)) => {
                        tally.sent += bytes as u64;
                    }
                    Ok((ndp_transport::FrameOutcome::Discarded, _)) => {
                        // The frame never left, so the next one must not
                        // depend on it.
                        video.source.request_keyframe();
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the session ended while sending video");
                        break;
                    }
                }
            }

            packet = next_audio(&mut packets) => {
                // A closed audio channel is not the end of the session: the
                // encoder can stop while the picture keeps going, and a
                // silent remote desktop is still a remote desktop.
                let Some(packet) = packet else {
                    tracing::debug!("audio capture ended; continuing without audio");
                    continue;
                };
                let header = MsgHeader::new(MsgKind::AudioFrame, audio_seq, packet.timestamp_us)
                    .with_flags(MsgFlags::DISCARDABLE);
                audio_seq = audio_seq.wrapping_add(1);
                let payload = audio_info.frame_payload(&packet.data);
                // Audio rides in datagrams, which cannot be fragmented. A
                // packet too big for the path is dropped here rather than
                // failing the send and taking the session down with it.
                if session.max_audio_record().is_some_and(|max| payload.len() > max) {
                    tracing::debug!(bytes = payload.len(), "audio packet too large for the path");
                    continue;
                }
                match session.send(Channel::Audio, header, &payload).await {
                    Ok(()) => tally.sent += payload.len() as u64,
                    Err(error) => {
                        tracing::debug!(%error, "the session ended while sending audio");
                        break;
                    }
                }
            }

            action = async {
                match clipboard.as_mut() {
                    Some(worker) => worker.outbound.recv().await,
                    // Nothing to wait on, and returning would spin this
                    // arm of the select as fast as the runtime allows.
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

            message = receiver.recv() => {
                let Some(message) = message else { break };
                let Ok(message) = message else { break };
                tally.received += message.payload.len() as u64;
                if !handle(
                    request,
                    &session,
                    &mut video.source,
                    input.as_deref_mut(),
                    clipboard.as_ref(),
                    transfers.as_ref(),
                    message,
                )
                .await
                {
                    break;
                }
            }
        }
    }

    drop(sending);
    drop(video);
    drop(starting_audio);
    drop(audio);
}

struct StartedVideo {
    source: Box<dyn VideoSource>,
}

impl Drop for StartedVideo {
    fn drop(&mut self) {
        self.source.stop();
    }
}

async fn start_video(
    source: Box<dyn VideoSource>,
    config: VideoConfig,
    sink: FrameSink,
) -> anyhow::Result<StartedVideo> {
    // Portal consent and native driver setup can block. A dropped JoinHandle
    // cannot cancel a blocking call, so its result owns a stop guard: even if
    // the peer leaves during consent, eventual startup is torn down, not leaked.
    tokio::task::spawn_blocking(move || {
        let mut video = StartedVideo { source };
        video.source.start(config, sink)?;
        Ok(video)
    })
    .await?
}

struct StartedAudio {
    source: Box<dyn AudioSource>,
}

impl Drop for StartedAudio {
    fn drop(&mut self) {
        self.source.stop();
    }
}

async fn start_audio(
    platform: Arc<dyn Platform>,
    config: AudioConfig,
    sink: AudioSink,
) -> anyhow::Result<Option<StartedAudio>> {
    // Optional native audio setup must not hold up video or control. Like video,
    // the worker owns its stop guard even if the session leaves during startup.
    tokio::task::spawn_blocking(move || {
        let source = match platform.audio() {
            Ok(source) => source,
            Err(error) => {
                tracing::warn!(%error, "no audio source; this session is silent");
                return Ok(None);
            }
        };
        let mut audio = StartedAudio { source };
        audio.source.start(config, sink)?;
        Ok(Some(audio))
    })
    .await?
}

fn input_for_session(
    platform: &dyn Platform,
    allowed: bool,
) -> anyhow::Result<Option<Box<dyn crate::media::InputInjector>>> {
    if allowed {
        platform.input().map(Some)
    } else {
        Ok(None)
    }
}

async fn next_audio(packets: &mut Option<mpsc::Receiver<EncodedAudio>>) -> Option<EncodedAudio> {
    let Some(receiver) = packets.as_mut() else {
        return std::future::pending().await;
    };
    let packet = receiver.recv().await;
    if packet.is_none() {
        // A closed receiver is always ready. Disable this select branch after
        // draining it, instead of waking the media loop continuously.
        *packets = None;
    }
    packet
}

/// Act on one message from the client. Returns false when the session should
/// end.
async fn handle(
    request: &SessionRequest,
    session: &Session,
    video: &mut Box<dyn crate::media::VideoSource>,
    input: Option<&mut dyn crate::media::InputInjector>,
    clipboard: Option<&clipboard::Worker>,
    transfers: Option<&files::Worker>,
    message: Incoming,
) -> bool {
    match (message.channel, message.header.kind) {
        (Channel::Input, _) => {
            // Policy is enforced here and nowhere else. A viewer's client is
            // supposed not to send input at all, but "supposed to" is not a
            // security property.
            if !request.policy.input {
                tracing::debug!(session = %request.session, "input refused: this session is view only");
                return true;
            }
            let Some(input) = input else { return true };
            match InputEvent::decode_batch(&message.payload) {
                Ok(events) => {
                    for event in &events {
                        if let Err(error) = input.inject(event) {
                            tracing::warn!(%error, "input injection failed");
                        }
                    }
                }
                Err(error) => tracing::debug!(%error, "malformed input batch"),
            }
            true
        }

        (Channel::Control, MsgKind::Ping) => {
            let header = MsgHeader::new(
                MsgKind::Pong,
                message.header.seq,
                message.header.timestamp_us,
            );
            session
                .send(Channel::Control, header, &message.payload)
                .await
                .is_ok()
        }

        (Channel::Control, MsgKind::QosReport) => {
            // Adaptive bitrate proper arrives with the media backends; until
            // then a report is at least evidence the client is alive.
            tracing::trace!(session = %request.session, "qos report");
            true
        }

        (Channel::Control, MsgKind::CapsUpdate) => {
            tracing::debug!(session = %request.session, "the client asked for a keyframe");
            video.request_keyframe();
            true
        }

        (Channel::Clipboard, kind) => {
            // Policy is enforced by there being no worker at all when the
            // entitlement withholds the clipboard, so a peer that sends
            // anyway is talking to nothing.
            if let Some(worker) = clipboard {
                match inbound(kind, &message.payload) {
                    Ok(Some(message)) => worker.deliver(message),
                    Ok(None) => {}
                    Err(error) => tracing::debug!(%error, "malformed clipboard message"),
                }
            }
            true
        }

        (Channel::File, kind) => {
            if let Some(worker) = transfers {
                match inbound_file(kind, &message.payload) {
                    Ok(Some(message)) => worker.deliver(message),
                    Ok(None) => {}
                    Err(error) => tracing::debug!(%error, "malformed file transfer message"),
                }
            }
            true
        }

        (Channel::Control, MsgKind::Bye) => false,

        (channel, kind) => {
            tracing::debug!(?channel, ?kind, "ignoring an unhandled message");
            true
        }
    }
}

/// Put one clipboard action on the wire.
async fn send_clipboard(session: &Session, action: &clipboard::Action, seq: &mut u32) -> bool {
    let (kind, payload) = match action {
        clipboard::Action::Offer(offer) => (
            MsgKind::ClipboardOffer,
            serde_json::to_vec(offer).unwrap_or_default(),
        ),
        clipboard::Action::Request(request) => (
            MsgKind::ClipboardRequest,
            serde_json::to_vec(request).unwrap_or_default(),
        ),
        clipboard::Action::Data(header, bytes) => (MsgKind::ClipboardData, header.payload(bytes)),
    };
    let header = MsgHeader::new(kind, *seq, 0);
    *seq = seq.wrapping_add(1);
    session
        .send(Channel::Clipboard, header, &payload)
        .await
        .is_ok()
}

/// Parse one clipboard message from the peer.
fn inbound(kind: MsgKind, payload: &[u8]) -> anyhow::Result<Option<clipboard::Inbound>> {
    Ok(match kind {
        MsgKind::ClipboardOffer => {
            Some(clipboard::Inbound::Offer(serde_json::from_slice(payload)?))
        }
        MsgKind::ClipboardRequest => Some(clipboard::Inbound::Request(serde_json::from_slice(
            payload,
        )?)),
        MsgKind::ClipboardData => {
            let (header, bytes) = ndp_proto::ClipboardDataHeader::split(payload)?;
            Some(clipboard::Inbound::Data(header, bytes.to_vec()))
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

/// Put one file transfer action on the wire.
async fn send_file_action(session: &Session, action: &files::Action, seq: &mut u32) -> bool {
    let (kind, payload) = match action {
        files::Action::Offer(offer) => (
            MsgKind::FileOffer,
            serde_json::to_vec(offer).unwrap_or_default(),
        ),
        files::Action::Chunk(header, data) => {
            let mut payload = Vec::with_capacity(data.len() + ndp_proto::FILE_CHUNK_HEADER_LEN);
            payload.extend_from_slice(&header.to_bytes());
            payload.extend_from_slice(data);
            (MsgKind::FileChunk, payload)
        }
        files::Action::Ack(ack) => (MsgKind::FileAck, ack.to_bytes().to_vec()),
    };
    *seq = seq.wrapping_add(1);
    let header = MsgHeader::new(kind, *seq, 0);
    session.send(Channel::File, header, &payload).await.is_ok()
}

/// Parse one file transfer message from the peer.
fn inbound_file(kind: MsgKind, payload: &[u8]) -> anyhow::Result<Option<files::Inbound>> {
    Ok(match kind {
        MsgKind::FileOffer => Some(files::Inbound::Offer(serde_json::from_slice(payload)?)),
        MsgKind::FileChunk => {
            let (header, data) = ndp_proto::FileChunkHeader::split(payload)?;
            Some(files::Inbound::Chunk(header, data.to_vec()))
        }

        MsgKind::FileAck => Some(files::Inbound::Ack(ndp_proto::FileAck::decode(payload)?)),
        other => {
            tracing::debug!(
                kind = ?other,
                "ignoring an unexpected message on the file channel"
            );
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipath_is_negotiated_without_reinterpreting_legacy_sessions() {
        assert!(!session_multipath(b"legacy-ticket").unwrap());
        assert!(session_multipath(br#"{"ticket":"signed-ticket","multipath":true}"#).unwrap());
        assert!(!session_multipath(br#"{"ticket":"signed-ticket","multipath":false}"#).unwrap());
        assert!(session_multipath(b"").is_err());
        assert!(session_multipath(br#"{"ticket":"","multipath":true}"#).is_err());
        assert!(session_multipath(br#"{"ticket":true,"multipath":true}"#).is_err());
        assert!(session_multipath(b"{").is_err());
    }
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct PumpProbe {
        sink: mpsc::UnboundedSender<FrameSink>,
        injected: Arc<AtomicUsize>,
        requested: Arc<AtomicUsize>,
        stopped: Arc<AtomicUsize>,
        audio: std::sync::Mutex<Option<WaitingAudio>>,
        close_before_stop: Option<quinn::Connection>,
    }

    impl Platform for PumpProbe {
        fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
            Ok(Box::new(ProbeVideo {
                sink: self.sink.clone(),
                requested: self.requested.clone(),
                stopped: self.stopped.clone(),
                close_before_stop: self.close_before_stop.clone(),
            }))
        }

        fn input(&self) -> anyhow::Result<Box<dyn crate::media::InputInjector>> {
            Ok(Box::new(ProbeInput(self.injected.clone())))
        }

        fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
            self.audio
                .lock()
                .unwrap()
                .take()
                .map(|audio| Box::new(audio) as Box<dyn AudioSource>)
                .ok_or_else(|| anyhow::anyhow!("no test audio source"))
        }
    }

    struct ProbeVideo {
        sink: mpsc::UnboundedSender<FrameSink>,
        requested: Arc<AtomicUsize>,
        stopped: Arc<AtomicUsize>,
        close_before_stop: Option<quinn::Connection>,
    }

    impl VideoSource for ProbeVideo {
        fn start(&mut self, _: VideoConfig, sink: FrameSink) -> anyhow::Result<()> {
            self.sink.send(sink)?;
            Ok(())
        }
        fn request_keyframe(&mut self) {
            self.requested.fetch_add(1, Ordering::Relaxed);
        }
        fn set_bitrate(&mut self, _: u32) {}
        fn stop(&mut self) {
            if let Some(conn) = &self.close_before_stop {
                assert!(
                    conn.close_reason().is_some(),
                    "close transport before stopping capture"
                );
            }
            self.stopped.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ProbeInput(Arc<AtomicUsize>);

    impl crate::media::InputInjector for ProbeInput {
        fn inject(&mut self, _: &InputEvent) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn congested_keyframe_keeps_input_control_and_revocation_responsive() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let config = TransportConfig {
                max_concurrent_uni: 0,
                ..TransportConfig::default()
            };
            let credentials = ndp_transport::dev_credentials(&[]).unwrap();
            let server = ndp_transport::server_endpoint(
                "127.0.0.1:0".parse().unwrap(),
                &credentials,
                &[ndp_transport::ALPN_SESSION],
                &config,
            )
            .unwrap();
            let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
            let keys = StaticKeypair::generate();
            let public = keys.public();
            let listener = server.clone();
            let accept_config = config.clone();
            let accepting = tokio::spawn(async move {
                let conn = listener.accept().await.unwrap().await.unwrap();
                Session::accept(
                    conn,
                    Responder::new(&keys, b"pump regression").unwrap(),
                    |_| Ok(Vec::new()),
                    &accept_config,
                )
                .await
                .unwrap()
            });
            let conn = connect(
                &endpoint,
                server.local_addr().unwrap(),
                "localhost",
                credentials.fingerprint,
                ndp_transport::ALPN_SESSION,
                &config,
            )
            .await
            .unwrap();
            let (client, mut incoming, _) = Session::initiate(
                conn,
                ndp_crypto::Initiator::new(&StaticKeypair::generate(), &public, b"pump regression")
                    .unwrap(),
                b"ticket",
                &config,
            )
            .await
            .unwrap();
            let (agent, receiver) = accepting.await.unwrap();
            let (sink_tx, mut sink_rx) = mpsc::unbounded_channel();
            let platform = Arc::new(PumpProbe {
                sink: sink_tx,
                injected: Arc::default(),
                requested: Arc::default(),
                stopped: Arc::default(),
                audio: std::sync::Mutex::new(None),
                close_before_stop: None,
            });
            let request = SessionRequest {
                session: nebula_common::SessionId::new(),
                resource_id: uuid::Uuid::new_v4(),
                policy: nebula_common::SessionPolicy {
                    input: true,
                    ..nebula_common::SessionPolicy::view_only()
                },
                role: nebula_common::SessionRole::Controller,
                relay_addr: String::new(),
                relay_pin: String::new(),
                pair_token: String::new(),
                client_key: String::new(),
            };
            let (stop, stopped) = oneshot::channel();
            let probe = platform.clone();
            let running =
                tokio::spawn(async move { pump(&request, agent, receiver, probe, stopped).await });
            let sink = sink_rx.recv().await.unwrap();
            let frame = EncodedFrame {
                keyframe: true,
                timestamp_us: 1,
                data: b"codec independent".to_vec(),
            };
            for _ in 0..=FRAME_QUEUE {
                sink.send(frame.clone()).await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(30), sink.send(frame))
                    .await
                    .is_err(),
                "only one send may be in flight in addition to the bounded source queue"
            );
            for _ in 0..3 {
                client
                    .send(
                        Channel::Input,
                        MsgHeader::new(MsgKind::InputBatch, 0, 0),
                        &InputEvent::encode_batch(&[InputEvent::mouse_move(
                            0.5,
                            0.5,
                            ndp_proto::Modifiers::NONE,
                        )]),
                    )
                    .await
                    .unwrap();
            }
            client
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::CapsUpdate, 0, 0),
                    b"",
                )
                .await
                .unwrap();
            client
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::Ping, 0, 0),
                    b"responsive",
                )
                .await
                .unwrap();
            let pong = tokio::time::timeout(Duration::from_secs(1), incoming.recv())
                .await
                .expect("blocked video must not block control")
                .unwrap()
                .unwrap();
            assert_eq!(pong.header.kind, MsgKind::Pong);
            assert_eq!(pong.payload, b"responsive");
            assert_eq!(platform.requested.load(Ordering::Relaxed), 1);
            tokio::time::timeout(Duration::from_secs(1), async {
                while platform.injected.load(Ordering::Relaxed) != 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("blocked video must not block input");
            stop.send("revoked".into()).unwrap();
            let tally = tokio::time::timeout(Duration::from_secs(1), running)
                .await
                .expect("blocked keyframe must not delay revocation")
                .unwrap();
            assert_eq!(tally.sent, 0);
            assert_eq!(platform.stopped.load(Ordering::Relaxed), 1);
            assert!(sink.is_closed());
        })
        .await
        .expect("loopback pump test timed out");
    }

    #[tokio::test]
    async fn revocation_interrupts_blocked_reliable_pong_and_preserves_tally() {
        tokio::time::timeout(Duration::from_secs(6), async {
            let config = TransportConfig::default();
            let credentials = ndp_transport::dev_credentials(&[]).unwrap();
            let server = ndp_transport::server_endpoint(
                "127.0.0.1:0".parse().unwrap(),
                &credentials,
                &[ndp_transport::ALPN_SESSION],
                &config,
            )
            .unwrap();
            let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
            let keys = StaticKeypair::generate();
            let public = keys.public();
            let listener = server.clone();
            let accept_config = config.clone();
            let accepting = tokio::spawn(async move {
                let conn = listener.accept().await.unwrap().await.unwrap();
                let (session, receiver) = Session::accept(
                    conn.clone(),
                    Responder::new(&keys, b"blocked pong regression").unwrap(),
                    |_| Ok(Vec::new()),
                    &accept_config,
                )
                .await
                .unwrap();
                (session, receiver, conn)
            });
            let client_conn = connect(
                &endpoint,
                server.local_addr().unwrap(),
                "localhost",
                credentials.fingerprint,
                ndp_transport::ALPN_SESSION,
                &config,
            )
            .await
            .unwrap();
            let (client, incoming, _) = Session::initiate(
                client_conn.clone(),
                ndp_crypto::Initiator::new(
                    &StaticKeypair::generate(),
                    &public,
                    b"blocked pong regression",
                )
                .unwrap(),
                b"ticket",
                &config,
            )
            .await
            .unwrap();
            let (client, mut incoming) = client.into_multipath(incoming);
            let (agent, receiver, agent_conn) = accepting.await.unwrap();
            let (agent, receiver) = agent.into_multipath(receiver);
            let monitor = agent.clone();
            let _cleanup = SessionLifetime(agent.clone());
            let (sink_tx, mut sink_rx) = mpsc::unbounded_channel();
            let (entered, started) = oneshot::channel();
            let (release, gate) = std::sync::mpsc::channel();
            let (audio_stopped, finished) = oneshot::channel();
            let platform = Arc::new(PumpProbe {
                sink: sink_tx,
                injected: Arc::default(),
                requested: Arc::default(),
                stopped: Arc::default(),
                audio: std::sync::Mutex::new(Some(WaitingAudio {
                    entered: Some(entered),
                    release: gate,
                    stopped: Some(audio_stopped),
                    sink: None,
                    fail: false,
                })),
                close_before_stop: Some(agent_conn.clone()),
            });
            let request = SessionRequest {
                session: nebula_common::SessionId::new(),
                resource_id: uuid::Uuid::new_v4(),
                policy: nebula_common::SessionPolicy {
                    audio: true,
                    ..nebula_common::SessionPolicy::view_only()
                },
                role: nebula_common::SessionRole::Viewer,
                relay_addr: String::new(),
                relay_pin: String::new(),
                pair_token: String::new(),
                client_key: String::new(),
            };
            let (stop, stopped) = oneshot::channel();
            let probe = platform.clone();
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn(async move { pump(&request, agent, receiver, probe, stopped).await });
            let sink = sink_rx.recv().await.unwrap();
            let frame = EncodedFrame {
                keyframe: true,
                timestamp_us: 1,
                data: b"counted video".to_vec(),
            };
            sink.send(frame.clone()).await.unwrap();
            let video = incoming.recv().await.unwrap().unwrap();
            assert_eq!(video.header.kind, MsgKind::VideoFrame);
            let audio_sink = started.await.unwrap();
            release.send(()).unwrap();
            audio_sink
                .send(EncodedAudio {
                    timestamp_us: 2,
                    data: vec![1, 2, 3],
                })
                .await
                .unwrap();
            let audio = incoming.recv().await.unwrap().unwrap();
            assert_eq!(audio.header.kind, MsgKind::AudioFrame);
            let media_bytes = (video.payload.len() + audio.payload.len()) as u64;

            // Retain but stop draining the application receiver. The encrypted
            // path's probes keep running while reliable Pong credit runs out.
            let sent = Arc::new(AtomicUsize::new(0));
            let progress = sent.clone();
            let sender = client.clone();
            let mut flooding = tokio::task::JoinSet::new();
            flooding.spawn(async move {
                for _ in 0..512 {
                    sender
                        .send(
                            Channel::Control,
                            MsgHeader::new(MsgKind::Ping, 0, 0),
                            b"flood",
                        )
                        .await?;
                    progress.fetch_add(1, Ordering::Relaxed);
                    sender
                        .send(
                            Channel::Control,
                            MsgHeader::new(MsgKind::CapsUpdate, 0, 0),
                            b"",
                        )
                        .await?;
                }
                Ok::<_, ndp_transport::TransportError>(())
            });
            while sent.load(Ordering::Relaxed) < 128 {
                tokio::task::yield_now().await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(150), flooding.join_next())
                    .await
                    .is_err()
            );
            // The pump can no longer drain capture either: it is inside the
            // awaited Pong handler, not waiting at its ordinary select loop.
            for _ in 0..FRAME_QUEUE {
                sink.send(frame.clone()).await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), sink.send(frame.clone()),)
                    .await
                    .is_err()
            );
            let handled = platform.requested.load(Ordering::Relaxed);
            assert!(handled > 0);
            let before = monitor.path_stats()[0].udp_rx_bytes;
            tokio::time::sleep(Duration::from_millis(600)).await;
            let stats = monitor.path_stats();
            assert_eq!(stats.len(), 1);
            assert!(stats[0].end_to_end_rtt.is_some());
            assert!(stats[0].udp_rx_bytes > before, "probes must remain live");
            assert_eq!(platform.requested.load(Ordering::Relaxed), handled);
            assert!(agent_conn.close_reason().is_none());

            stop.send("gateway revoked blocked session".into()).unwrap();
            let tally = tokio::time::timeout(Duration::from_millis(300), tasks.join_next())
                .await
                .expect("gateway revocation must interrupt a blocked reliable Pong")
                .unwrap()
                .unwrap();
            assert_eq!(tally.sent, media_bytes);
            assert!(tally.received >= (handled * b"flood".len()) as u64);
            assert_eq!(platform.stopped.load(Ordering::Relaxed), 1);
            finished.await.unwrap();
            assert!(sink.is_closed());
            assert!(audio_sink.is_closed());
            client_conn.closed().await;
            let terminal = loop {
                match incoming.recv().await {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break error,
                    None => panic!("logical receiver omitted the revocation error"),
                }
            };
            let ndp_transport::TransportError::Connection(
                quinn::ConnectionError::ApplicationClosed(closed),
            ) = terminal
            else {
                panic!("revocation must retain the application's close reason: {terminal:?}");
            };
            assert_eq!(closed.error_code.into_inner() & 0xffff_ffff, 0x22);
            assert_eq!(closed.reason.as_ref(), b"gateway revoked blocked session");
            flooding.abort_all();
        })
        .await
        .expect("encrypted multipath revocation regression timed out");
    }

    #[derive(Default)]
    struct InputProbe {
        opened: AtomicUsize,
    }

    impl Platform for InputProbe {
        fn video(&self) -> anyhow::Result<Box<dyn crate::media::VideoSource>> {
            anyhow::bail!("not used by the input permission tests")
        }

        fn input(&self) -> anyhow::Result<Box<dyn crate::media::InputInjector>> {
            self.opened.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("input permission denied")
        }
    }

    #[test]
    fn viewing_does_not_open_input_or_prompt_for_input_permission() {
        let platform = InputProbe::default();
        assert!(input_for_session(&platform, false).unwrap().is_none());
        assert_eq!(platform.opened.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn input_permission_errors_are_propagated() {
        let platform = InputProbe::default();
        assert!(input_for_session(&platform, true).is_err());
        assert_eq!(platform.opened.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn closed_audio_is_drained_then_disabled() {
        let (sender, receiver) = mpsc::channel(2);
        sender
            .send(EncodedAudio {
                timestamp_us: 1,
                data: vec![1],
            })
            .await
            .unwrap();
        drop(sender);
        let mut packets = Some(receiver);
        assert_eq!(next_audio(&mut packets).await.unwrap().data, [1]);
        assert!(next_audio(&mut packets).await.is_none());
        assert!(packets.is_none());
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(20),
            next_audio(&mut packets),
        )
        .await
        .is_err());
    }

    struct WaitingAudio {
        entered: Option<oneshot::Sender<AudioSink>>,
        release: std::sync::mpsc::Receiver<()>,
        stopped: Option<oneshot::Sender<()>>,
        sink: Option<AudioSink>,
        fail: bool,
    }

    impl AudioSource for WaitingAudio {
        fn start(&mut self, _: AudioConfig, sink: AudioSink) -> anyhow::Result<()> {
            self.sink = Some(sink.clone());
            let _ = self.entered.take().unwrap().send(sink);
            self.release.recv_timeout(Duration::from_secs(5))?;
            anyhow::ensure!(!self.fail, "test audio startup failure");
            Ok(())
        }

        fn stop(&mut self) {
            self.sink = None;
            if let Some(stopped) = self.stopped.take() {
                let _ = stopped.send(());
            }
        }
    }

    enum AudioEnd {
        Revoked,
        Disconnected,
        Cancelled,
        Success,
        Failure,
    }

    async fn exercise_blocking_audio(end: AudioEnd) {
        tokio::time::timeout(Duration::from_secs(4), async {
            let config = TransportConfig::default();
            let credentials = ndp_transport::dev_credentials(&[]).unwrap();
            let server = ndp_transport::server_endpoint(
                "127.0.0.1:0".parse().unwrap(),
                &credentials,
                &[ndp_transport::ALPN_SESSION],
                &config,
            )
            .unwrap();
            let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
            let keys = StaticKeypair::generate();
            let public = keys.public();
            let listener = server.clone();
            let accept_config = config.clone();
            let accepting = tokio::spawn(async move {
                let conn = listener.accept().await.unwrap().await.unwrap();
                Session::accept(
                    conn,
                    Responder::new(&keys, b"audio pump regression").unwrap(),
                    |_| Ok(Vec::new()),
                    &accept_config,
                )
                .await
                .unwrap()
            });
            let conn = connect(
                &endpoint,
                server.local_addr().unwrap(),
                "localhost",
                credentials.fingerprint,
                ndp_transport::ALPN_SESSION,
                &config,
            )
            .await
            .unwrap();
            let (client, mut incoming, _) = Session::initiate(
                conn,
                ndp_crypto::Initiator::new(
                    &StaticKeypair::generate(),
                    &public,
                    b"audio pump regression",
                )
                .unwrap(),
                b"ticket",
                &config,
            )
            .await
            .unwrap();
            let (agent, receiver) = accepting.await.unwrap();
            let (sink_tx, mut sink_rx) = mpsc::unbounded_channel();
            let (entered, started) = oneshot::channel();
            let (release, gate) = std::sync::mpsc::channel();
            let (audio_stopped, finished) = oneshot::channel();
            let platform = Arc::new(PumpProbe {
                sink: sink_tx,
                injected: Arc::default(),
                requested: Arc::default(),
                stopped: Arc::default(),
                audio: std::sync::Mutex::new(Some(WaitingAudio {
                    entered: Some(entered),
                    release: gate,
                    stopped: Some(audio_stopped),
                    sink: None,
                    fail: matches!(end, AudioEnd::Failure),
                })),
                close_before_stop: None,
            });
            let request = SessionRequest {
                session: nebula_common::SessionId::new(),
                resource_id: uuid::Uuid::new_v4(),
                policy: nebula_common::SessionPolicy {
                    input: true,
                    audio: true,
                    ..nebula_common::SessionPolicy::view_only()
                },
                role: nebula_common::SessionRole::Controller,
                relay_addr: String::new(),
                relay_pin: String::new(),
                pair_token: String::new(),
                client_key: String::new(),
            };
            let (stop, stopped) = oneshot::channel();
            let probe = platform.clone();
            let running =
                tokio::spawn(async move { pump(&request, agent, receiver, probe, stopped).await });
            let sink = sink_rx.recv().await.unwrap();
            let audio_sink = started.await.unwrap();
            let frame = EncodedFrame {
                keyframe: true,
                timestamp_us: 1,
                data: b"video during audio startup".to_vec(),
            };
            sink.send(frame.clone()).await.unwrap();
            let video = incoming.recv().await.unwrap().unwrap();
            assert_eq!(video.header.kind, MsgKind::VideoFrame);
            assert_eq!(video.payload, frame.data);
            client
                .send(
                    Channel::Input,
                    MsgHeader::new(MsgKind::InputBatch, 0, 0),
                    &InputEvent::encode_batch(&[InputEvent::mouse_move(
                        0.5,
                        0.5,
                        ndp_proto::Modifiers::NONE,
                    )]),
                )
                .await
                .unwrap();
            client
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::Ping, 7, 9),
                    b"responsive during audio startup",
                )
                .await
                .unwrap();
            let pong = incoming.recv().await.unwrap().unwrap();
            assert_eq!(pong.header.kind, MsgKind::Pong);
            assert_eq!(pong.payload, b"responsive during audio startup");
            while platform.injected.load(Ordering::Relaxed) != 1 {
                tokio::task::yield_now().await;
            }
            assert_eq!(platform.stopped.load(Ordering::Relaxed), 0);

            match end {
                AudioEnd::Revoked => {
                    stop.send("revoked during audio startup".into()).unwrap();
                    running.await.unwrap();
                }
                AudioEnd::Disconnected => {
                    client.close(0, b"leaving during audio startup");
                    running.await.unwrap();
                }
                AudioEnd::Cancelled => {
                    running.abort();
                    assert!(running.await.unwrap_err().is_cancelled());
                }
                AudioEnd::Success => {
                    let packet = EncodedAudio {
                        timestamp_us: 42,
                        data: vec![1, 2, 3],
                    };
                    audio_sink.send(packet.clone()).await.unwrap();
                    release.send(()).unwrap();
                    let received = incoming.recv().await.unwrap().unwrap();
                    assert_eq!(received.header.kind, MsgKind::AudioFrame);
                    assert_eq!(received.header.timestamp_us, packet.timestamp_us);
                    assert_eq!(
                        received.payload,
                        ndp_proto::AudioFrameInfo {
                            codec: ndp_proto::AudioCodec::Opus,
                            channels: AudioConfig::default().channels as u8,
                            frame_ms: 20,
                        }
                        .frame_payload(&packet.data)
                    );
                    stop.send("done".into()).unwrap();
                    running.await.unwrap();
                    finished.await.unwrap();
                    assert!(audio_sink.is_closed());
                    return;
                }
                AudioEnd::Failure => {
                    release.send(()).unwrap();
                    finished.await.unwrap();
                    audio_sink.closed().await;
                    sink.send(frame.clone()).await.unwrap();
                    let received = incoming.recv().await.unwrap().unwrap();
                    assert_eq!(received.header.kind, MsgKind::VideoFrame);
                    assert_eq!(received.payload, frame.data);
                    stop.send("done".into()).unwrap();
                    running.await.unwrap();
                    return;
                }
            }
            assert_eq!(platform.stopped.load(Ordering::Relaxed), 1);
            assert!(sink.is_closed());
            assert!(audio_sink.is_closed());
            // Startup has not been released: the session must already be gone.
            release.send(()).unwrap();
            finished.await.unwrap();
        })
        .await
        .expect("blocking audio must not delay media, input, control, or teardown");
    }

    #[tokio::test]
    async fn blocking_audio_keeps_video_input_control_and_revocation_responsive() {
        exercise_blocking_audio(AudioEnd::Revoked).await;
    }

    #[tokio::test]
    async fn disconnect_during_audio_startup_stops_the_eventual_source() {
        exercise_blocking_audio(AudioEnd::Disconnected).await;
    }

    #[tokio::test]
    async fn cancelling_pump_during_audio_startup_stops_the_eventual_source() {
        exercise_blocking_audio(AudioEnd::Cancelled).await;
    }

    #[tokio::test]
    async fn successful_audio_startup_delivers_packets() {
        exercise_blocking_audio(AudioEnd::Success).await;
    }

    #[tokio::test]
    async fn failed_audio_startup_stops_source_and_keeps_video_running() {
        exercise_blocking_audio(AudioEnd::Failure).await;
    }

    struct WaitingVideo {
        entered: Option<oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
        stopped: Option<oneshot::Sender<()>>,
    }

    impl VideoSource for WaitingVideo {
        fn start(&mut self, _: VideoConfig, _: FrameSink) -> anyhow::Result<()> {
            self.entered.take().unwrap().send(()).unwrap();
            self.release.recv_timeout(Duration::from_secs(2))?;
            Ok(())
        }

        fn request_keyframe(&mut self) {}
        fn set_bitrate(&mut self, _: u32) {}

        fn stop(&mut self) {
            if let Some(stopped) = self.stopped.take() {
                let _ = stopped.send(());
            }
        }
    }

    #[tokio::test]
    async fn leaving_during_blocking_startup_stops_the_eventual_source() {
        let (entered, started) = oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (stopped, finished) = oneshot::channel();
        let video = WaitingVideo {
            entered: Some(entered),
            release: gate,
            stopped: Some(stopped),
        };
        let (sink, _frames) = mpsc::channel(1);
        let task = tokio::spawn(start_video(Box::new(video), VideoConfig::default(), sink));
        tokio::time::timeout(Duration::from_secs(1), started)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), finished)
            .await
            .unwrap()
            .unwrap();
    }
}
