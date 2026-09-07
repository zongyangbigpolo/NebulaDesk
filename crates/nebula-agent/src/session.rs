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
use ndp_signal::{RelayHello, RelayHelloAck, SessionRequest};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Incoming, Session, SessionReceiver,
    TransportConfig, ALPN_RELAY,
};
use tokio::sync::{mpsc, oneshot};

use crate::clipboard;
use crate::files;
use crate::media::{AudioConfig, EncodedAudio, EncodedFrame, Platform, VideoConfig};

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
    stop: oneshot::Receiver<String>,
) -> anyhow::Result<Tally> {
    let expected = PublicKey::from_slice(
        &hex::decode(&request.client_key)
            .map_err(|_| anyhow::anyhow!("the client key in the session request is not hex"))?,
    )?;

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
    let (session, receiver) = Session::accept(
        conn,
        responder,
        |ticket| {
            // The agent cannot check the ticket's signature: it holds no
            // manager keys, and giving it some would put a verification
            // dependency on the machine least able to keep one current. The
            // ticket is only evidence that the peer got this far.
            if ticket.is_empty() {
                return Err("no ticket was presented".into());
            }
            Ok(b"agent-ready".to_vec())
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

    tracing::info!(session = %request.session, "session established");
    let tally = pump(&request, session, receiver, platform, stop).await;
    Ok(tally)
}

/// Move media until one side goes away.
async fn pump(
    request: &SessionRequest,
    session: Session,
    mut receiver: SessionReceiver,
    platform: Arc<dyn Platform>,
    mut stop: oneshot::Receiver<String>,
) -> Tally {
    let mut tally = Tally::default();

    let platform = match platform.session_scope(request.policy.input) {
        Ok(Some(scoped)) => scoped,
        Ok(None) => platform,
        Err(error) => {
            tracing::error!(%error, "could not create the session's media scope");
            session.close(0x21, b"media scope failed");
            return tally;
        }
    };
    let mut video = match platform.video() {
        Ok(video) => video,
        Err(error) => {
            tracing::error!(%error, "no video source; refusing the session");
            session.close(0x21, b"no video source");
            return tally;
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
    if let Err(error) = video.start(VideoConfig::default(), frames_tx) {
        tracing::error!(%error, "the video source refused to start");
        session.close(0x21, b"capture failed");
        return tally;
    }

    let audio_config = AudioConfig::default();
    let audio_info = ndp_proto::AudioFrameInfo {
        codec: ndp_proto::AudioCodec::Opus,
        channels: audio_config.channels as u8,
        frame_ms: 20,
    };

    // Audio is optional in both directions: the entitlement may withhold it,
    // and a platform backend may not have it yet. Neither is a reason to
    // refuse a session, so a failure here is a session without sound.
    let (audio_tx, mut packets) = mpsc::channel::<EncodedAudio>(AUDIO_QUEUE);
    let mut audio = if request.policy.audio {
        match platform.audio() {
            Ok(mut audio) => match audio.start(audio_config, audio_tx) {
                Ok(()) => Some(audio),
                Err(error) => {
                    tracing::warn!(%error, "audio capture would not start; this session is silent");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "no audio source; this session is silent");
                None
            }
        }
    } else {
        None
    };

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
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
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
                match session.send_video_frame(header, &frame.data, deadline).await {
                    Ok(ndp_transport::FrameOutcome::Sent) => {
                        tally.sent += frame.data.len() as u64;
                    }
                    Ok(ndp_transport::FrameOutcome::Discarded) => {
                        // The frame never left, so the next one must not
                        // depend on it.
                        video.request_keyframe();
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the session ended while sending video");
                        break;
                    }
                }
            }

            packet = packets.recv() => {
                // A closed audio channel is not the end of the session: the
                // encoder can stop while the picture keeps going, and a
                // silent remote desktop is still a remote desktop.
                let Some(packet) = packet else {
                    packets.close();
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

            reason = &mut stop => {
                // The gateway revoked this session: entitlement withdrawn,
                // an administrator ending it, or the client having gone.
                // Closing with the reason means the client can say why the
                // window went away instead of showing a network error.
                let reason = reason.unwrap_or_else(|_| "the session was ended".into());
                tracing::info!(session = %request.session, %reason, "ending the session");
                session.close(0x22, reason.as_bytes());
                break;
            }

            message = receiver.recv() => {
                let Some(message) = message else { break };
                let Ok(message) = message else { break };
                tally.received += message.payload.len() as u64;
                if !handle(
                    request,
                    &session,
                    &mut video,
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

    video.stop();
    if let Some(audio) = audio.as_mut() {
        audio.stop();
    }
    tracing::info!(session = %request.session, sent = tally.sent, received = tally.received, "session ended");
    tally
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
}
