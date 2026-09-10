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
                bind_session_ticket(&request, ticket)?;
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

fn bind_session_ticket(request: &SessionRequest, payload: &[u8]) -> Result<(), String> {
    use base64::Engine;
    let presented = if payload.first() == Some(&b'{') {
        serde_json::from_slice::<SessionHello>(payload)
            .map_err(|_| "malformed session hello".to_string())?
            .ticket
    } else {
        String::from_utf8(payload.to_vec()).map_err(|_| "malformed session ticket".to_string())?
    };
    if let Some(admitted) = request.signed_ticket.as_deref() {
        if presented != admitted {
            return Err("session ticket does not match admission".into());
        }
    } else if request.launch_target.is_application() {
        return Err("application authority is missing".into());
    }
    let parts: Vec<_> = presented.split('.').collect();
    if parts.len() == 3 {
        // This parse only DENIES downgrade attempts. It never grants authority:
        // signatures/bindings were checked with trusted Manager keys at admission.
        // Legacy gateways may omit the envelope JWT, but an original APP JWT
        // inside authenticated Noise must never select the legacy desktop path.
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[1])
            .map_err(|_| "malformed session ticket".to_string())?;
        let claims: serde_json::Value =
            serde_json::from_slice(&claims).map_err(|_| "malformed session ticket".to_string())?;
        if !claims.is_object() {
            return Err("malformed session ticket".into());
        }
        let target = claims
            .get("launch_target")
            .cloned()
            .map(serde_json::from_value::<nebula_common::LaunchTarget>)
            .transpose()
            .map_err(|_| "invalid session target".to_string())?
            .unwrap_or_default();
        if target != request.launch_target
            || (target.is_application() && request.signed_ticket.is_none())
        {
            return Err("session target does not match authenticated client ticket".into());
        }
    } else if request.launch_target.is_application() {
        return Err("malformed application ticket".into());
    }
    Ok(())
}

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
    if let nebula_common::LaunchTarget::Application { launch, .. } = &request.launch_target {
        pump_application(request, session, receiver, platform, launch.clone(), tally).await;
        return;
    }
    let _controller = if request.policy.input {
        match crate::application::ControllerLease::acquire(platform.shared_desktop()) {
            Ok(lease) => Some(lease),
            Err(_) => {
                session.close(0x23, b"shared desktop controller busy");
                return;
            }
        }
    } else {
        None
    };
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
    let mut video = match start_video(video, VideoConfig::default(), frames_tx.into()).await {
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

    // File-copy gestures use the same native watcher, with independent policy.
    let mut clipboard = if request.policy.clipboard || request.policy.file_transfer {
        match platform.clipboard() {
            Ok(board) => Some(clipboard::spawn_with_files(
                board,
                request.policy.clipboard,
                request.policy.file_transfer,
            )),
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
                if let clipboard::Action::Files(paths) = action {
                    if let Some(worker) = transfers.as_ref() {
                        for path in paths {
                            worker.deliver(files::Inbound::Send(path));
                        }
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

async fn application_control(
    session: &Session,
    seq: &mut u32,
    message: ndp_proto::ControlMessage,
) -> bool {
    let Ok(payload) = message.encode() else {
        return false;
    };
    let header = MsgHeader::new(message.kind(), *seq, 0);
    *seq = seq.wrapping_add(1);
    session
        .send(Channel::Control, header, &payload)
        .await
        .is_ok()
}

async fn application_failure(session: &Session, seq: &mut u32, detail: &'static str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        application_control(
            session,
            seq,
            ndp_proto::ControlMessage::Bye {
                reason: ndp_proto::control::ByeReason::NegotiationFailed,
                detail: Some(detail.into()),
            },
        ),
    )
    .await;
}

async fn pump_application(
    request: &SessionRequest,
    session: Session,
    mut receiver: SessionReceiver,
    platform: Arc<dyn Platform>,
    launch: nebula_common::ApplicationLaunch,
    tally: &mut Tally,
) {
    pump_application_inner(
        request,
        session.clone(),
        &mut receiver,
        platform,
        launch,
        tally,
    )
    .await;
    // Reliable send completion only means queued to QUIC. Keep the transport
    // alive until the client acknowledges Bye/closes, otherwise session Drop
    // can discard the final Remove/StartupFailed/Bye before it is delivered.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(message)) = receiver.recv().await {
            if message.channel == Channel::Control && message.header.kind == MsgKind::Bye {
                break;
            }
        }
    })
    .await;
}

async fn pump_application_inner(
    request: &SessionRequest,
    session: Session,
    receiver: &mut SessionReceiver,
    platform: Arc<dyn Platform>,
    launch: nebula_common::ApplicationLaunch,
    tally: &mut Tally,
) {
    use crate::application::{WorkerCommand, WorkerEvent};
    use ndp_proto::application::{
        ApplicationMessage, SurfaceRegistry, APPLICATION_PROTOCOL_VERSION, MAX_APPLICATION_SURFACES,
    };
    use ndp_proto::{ControlMessage, FeatureFlags, VideoCodec, VideoFrameInfo};
    let mut seq = 0;
    if request
        .launch_target
        .validate(
            nebula_common::ResourceId::from_uuid(request.resource_id),
            request.policy,
        )
        .is_err()
    {
        application_failure(&session, &mut seq, "application_authority_invalid").await;
        return;
    }
    let offered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = receiver.recv().await?.ok()?;
            tally.received += message.payload.len() as u64;
            if message.channel != Channel::Control {
                return None;
            }
            if message.header.kind == MsgKind::Ping {
                let header = MsgHeader::new(
                    MsgKind::Pong,
                    message.header.seq,
                    message.header.timestamp_us,
                );
                session
                    .send(Channel::Control, header, &message.payload)
                    .await
                    .ok()?;
                continue;
            }
            if message.header.kind != MsgKind::Hello {
                return None;
            }
            let ControlMessage::Hello { caps, .. } =
                ControlMessage::decode(&message.payload).ok()?
            else {
                return None;
            };
            return Some(caps);
        }
    })
    .await
    .ok()
    .flatten();
    let Some(mut caps) = offered.filter(|caps| {
        caps.is_valid()
            && caps.features.contains(FeatureFlags::APPLICATION_WINDOWS)
            && caps.video_codecs.contains(&VideoCodec::H264)
    }) else {
        application_failure(&session, &mut seq, "application_negotiation_required").await;
        return;
    };
    let lease = match crate::application::ControllerLease::acquire(platform.shared_desktop()) {
        Ok(lease) => lease,
        Err(_) => {
            application_failure(&session, &mut seq, "shared_desktop_controller_busy").await;
            return;
        }
    };
    if !application_control(
        &session,
        &mut seq,
        ControlMessage::Application(ApplicationMessage::Hello {
            protocol_version: APPLICATION_PROTOCOL_VERSION,
            max_surfaces: MAX_APPLICATION_SURFACES,
            host_os: crate::platform::application_host_os(),
            keyboard_profile: ndp_proto::application::ApplicationKeyboardProfile::Physical,
        }),
    )
    .await
    {
        return;
    }
    caps.features = FeatureFlags::APPLICATION_WINDOWS;
    caps.video_codecs = vec![VideoCodec::H264];
    caps.audio_codecs.clear();
    caps.max_bitrate_bps = caps.max_bitrate_bps.min(12_000_000);
    let mut worker = match crate::application::spawn(
        platform,
        launch,
        request.policy.input,
        caps.max_bitrate_bps,
        lease,
    ) {
        Ok(worker) => worker,
        Err(_) => {
            application_failure(&session, &mut seq, "application_backend_unavailable").await;
            return;
        }
    };
    let mut registry = SurfaceRegistry::new(MAX_APPLICATION_SURFACES).expect("protocol limit");
    let mut sending: Option<VideoSend> = None;
    let mut sending_surface = None;
    let mut surface_sequences = [0u32; MAX_APPLICATION_SURFACES as usize];
    let mut unavailable = std::collections::BTreeSet::new();
    let mut ready = false;
    let mut path_changes = session.path_changes();
    loop {
        tokio::select! {
            biased;
            event = worker.events.recv() => {
                match event {
                    Some(WorkerEvent::Ready) => {
                        if !application_control(&session, &mut seq, ControlMessage::HelloAck {
                            caps: caps.clone(), agent_version: env!("CARGO_PKG_VERSION").into(), active_codec: VideoCodec::H264,
                        }).await { break; }
                        ready = true;
                    }
                    Some(WorkerEvent::Metadata(message)) => {
                        let refresh_surface = match &message {
                            ApplicationMessage::SurfaceUpsert { surface } => Some(surface.surface_id),
                            _ => None,
                        };
                        let valid = match &message {
                            ApplicationMessage::SurfaceUpsert { surface } => {
                                if sending_surface == Some(surface.surface_id)
                                    && registry.get(surface.surface_id).is_some_and(|old|
                                        old.geometry_generation != surface.geometry_generation || surface.minimized)
                                { sending = None; }
                                unavailable.remove(&surface.surface_id);
                                registry.upsert(surface.clone()).is_ok()
                            }
                            ApplicationMessage::SurfaceRemove { surface_id } => {
                                if sending_surface == Some(*surface_id) { sending = None; }
                                unavailable.remove(surface_id);
                                registry.remove(*surface_id).is_ok()
                            }
                            _ => false,
                        };
                        if !valid || !application_control(&session, &mut seq, ControlMessage::Application(message)).await { break; }
                        if let Some(id) = refresh_surface {
                            if worker.commands.try_send(WorkerCommand::Keyframe(id)).is_err() { break; }
                        }
                    }
                    Some(WorkerEvent::SurfaceFailed(id)) => {
                        unavailable.insert(id);
                        if sending_surface == Some(id) { sending = None; }
                        if !application_control(&session, &mut seq, ControlMessage::Application(
                            ApplicationMessage::SurfaceUnavailable {
                                surface_id: id,
                                reason: ndp_proto::application::ApplicationFailureReason::SurfaceUnavailable,
                            }
                        )).await { break; }
                    }
                    Some(WorkerEvent::Failed(reason)) => {
                        let _ = application_control(&session, &mut seq, ControlMessage::Application(
                            ApplicationMessage::StartupFailed { reason }
                        )).await;
                        application_failure(&session, &mut seq, "application_backend_unavailable").await;
                        break;
                    }
                    Some(WorkerEvent::Ended) => {
                        let _ = application_control(&session, &mut seq, ControlMessage::Bye {
                            reason: ndp_proto::control::ByeReason::UserClosed, detail: None,
                        }).await;
                        break;
                    }
                    None => break,
                }
            }
            changed = async {
                match path_changes.as_mut() {
                    Some(changes) => changes.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if changed.is_err() { path_changes = None; continue; }
                sending = None;
                while worker.frames.try_recv().is_ok() {}
                for id in 0..MAX_APPLICATION_SURFACES {
                    if registry.get(id).is_some() {
                        let _ = worker.commands.try_send(WorkerCommand::Keyframe(id));
                    }
                }
            }
            message = receiver.recv() => {
                let Some(Ok(message)) = message else { break; };
                tally.received += message.payload.len() as u64;
                if message.channel != Channel::Control {
                    // Desktop input, audio, global clipboard and file paths
                    // are never dispatched from application sessions.
                    continue;
                }
                match message.header.kind {
                    MsgKind::Ping => {
                        let header = MsgHeader::new(MsgKind::Pong, message.header.seq, message.header.timestamp_us);
                        if session.send(Channel::Control, header, &message.payload).await.is_err() { break; }
                    }
                    MsgKind::Bye => break,
                    MsgKind::Application => {
                        let Ok(ControlMessage::Application(command)) = ControlMessage::decode(&message.payload) else { continue; };
                        if registry.validate_command(&command).is_err() {
                            // The backend also validates on its posting thread.
                            // Forward invalid commands to release held input.
                            if worker.commands.try_send(WorkerCommand::Client(command)).is_err() { break; }
                            continue;
                        }
                        if worker.commands.try_send(WorkerCommand::Client(command)).is_err() {
                            application_failure(&session, &mut seq, "application_command_queue_full").await;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            frame = worker.frames.recv(), if ready && sending.is_none() => {
                let Some(frame) = frame else { break; };
                let Some(surface) = registry.get(frame.id) else { continue; };
                if !frame.is_current() || surface.geometry_generation != frame.generation || surface.minimized || unavailable.contains(&frame.id) { continue; }
                let info = VideoFrameInfo { display: frame.id, codec: VideoCodec::H264,
                    width: surface.width as u16, height: surface.height as u16, duration_us: 33_333 };
                let scoped = ndp_proto::application::SurfaceFrameInfo {
                    geometry_generation: frame.generation,
                    surface_sequence: surface_sequences[frame.id as usize],
                }.frame_payload(&frame.frame.data);
                surface_sequences[frame.id as usize] = surface_sequences[frame.id as usize].wrapping_add(1);
                let payload = info.frame_payload(&scoped);
                let flags = if frame.frame.keyframe { MsgFlags::KEYFRAME } else { MsgFlags::DISCARDABLE };
                let header = MsgHeader::new(MsgKind::VideoFrame, seq, frame.frame.timestamp_us).with_flags(flags);
                seq = seq.wrapping_add(1);
                let deadline = (!frame.frame.keyframe).then(|| tokio::time::Instant::now() + FRAME_DEADLINE);
                let sender = session.clone();
                sending_surface = Some(frame.id);
                sending = Some(Box::pin(async move {
                    let outcome = sender.send_video_frame(header, &payload, deadline).await?;
                    Ok((outcome, payload.len()))
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
                    Ok((ndp_transport::FrameOutcome::Sent, bytes)) => tally.sent += bytes as u64,
                    Ok((ndp_transport::FrameOutcome::Discarded, _)) => {
                        if let Some(id) = sending_surface {
                            let _ = worker.commands.try_send(WorkerCommand::Keyframe(id));
                        }
                    }
                    Err(_) => break,
                }
                sending_surface = None;
            }
        }
    }
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
        clipboard::Action::Files(_) => {
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
            clipboard::Action::Offer(offer) => tracing::debug!(
                offer_id = offer.offer_id,
                format = ?offer.formats,
                bytes = offer.size_hint,
                "clipboard offer sent on Nebula channel"
            ),
            clipboard::Action::Data(header, bytes) => tracing::debug!(
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

    #[test]
    fn application_noise_ticket_cannot_be_downgraded_by_stripping_gateway_authority() {
        use base64::Engine;
        let resource = nebula_common::ResourceId::new();
        let launch = nebula_common::ApplicationLaunch {
            launch_path: "/Applications/Editor.app".into(),
            launch_args: vec![],
            working_dir: None,
        };
        let target = nebula_common::LaunchTarget::Application {
            resource_id: resource,
            version: launch.version(),
            launch,
        };
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&serde_json::json!({ "launch_target": target })).unwrap());
        // Signature checking is independently exercised by SessionRequest tests;
        // this helper only enforces authenticated-Noise/envelope consistency.
        let token = format!("header.{encoded}.signature");
        let mut request = SessionRequest {
            session: nebula_common::SessionId::new(),
            resource_id: resource.as_uuid(),
            launch_target: target,
            signed_ticket: Some(token.clone()),
            policy: nebula_common::application::application_policy(
                nebula_common::SessionPolicy::full(),
            ),
            role: nebula_common::SessionRole::Controller,
            relay_addr: String::new(),
            relay_pin: String::new(),
            pair_token: String::new(),
            client_key: String::new(),
        };
        assert!(bind_session_ticket(&request, token.as_bytes()).is_ok());
        assert!(bind_session_ticket(&request, b"different-client-ticket").is_err());
        request.launch_target = Default::default();
        assert!(bind_session_ticket(&request, token.as_bytes()).is_err());
        request.signed_ticket = None;
        assert!(bind_session_ticket(&request, token.as_bytes()).is_err());
        let hello = serde_json::to_vec(&SessionHello {
            ticket: token,
            multipath: true,
        })
        .unwrap();
        assert!(bind_session_ticket(&request, &hello).is_err());
        assert!(bind_session_ticket(&request, b"legacy-desktop-ticket").is_ok());
        let legacy_claims =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"legacy\":true}");
        assert!(bind_session_ticket(
            &request,
            format!("header.{legacy_claims}.signature").as_bytes()
        )
        .is_ok());
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
        fn shared_desktop(&self) -> bool {
            false
        }
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

    struct AppPumpProbe(Arc<AtomicUsize>, Arc<std::sync::atomic::AtomicBool>);

    impl Platform for AppPumpProbe {
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
        ) -> anyhow::Result<Box<dyn crate::application::ApplicationBackend>> {
            Ok(Box::new(Self(self.0.clone(), self.1.clone())))
        }
        fn video(&self) -> anyhow::Result<Box<dyn VideoSource>> {
            panic!("application captured desktop")
        }
        fn input(&self) -> anyhow::Result<Box<dyn crate::media::InputInjector>> {
            panic!("application opened desktop input")
        }
        fn audio(&self) -> anyhow::Result<Box<dyn AudioSource>> {
            panic!("application opened global audio")
        }
        fn clipboard(&self) -> anyhow::Result<Box<dyn crate::clipboard::ClipboardAccess>> {
            panic!("application opened global clipboard")
        }
    }

    impl crate::application::ApplicationBackend for AppPumpProbe {
        fn snapshot(&mut self) -> anyhow::Result<Vec<crate::application::NativeSurface>> {
            if self.1.load(Ordering::SeqCst) {
                return Ok(Vec::new());
            }
            Ok(vec![crate::application::NativeSurface {
                native_id: 12345,
                parent: None,
                title: "App".into(),
                width: 640,
                height: 480,
                scale: 1.0,
                geometry_generation: 1,
                modal: false,
                minimized: false,
            }])
        }
        fn video(&mut self, _: u64) -> anyhow::Result<Box<dyn VideoSource>> {
            Ok(Box::new(crate::media::SyntheticVideo::default()))
        }
        fn input(&mut self, _: u64, _: u32, _: &InputEvent) -> anyhow::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn operate(
            &mut self,
            _: u64,
            command: &ndp_proto::application::ApplicationMessage,
        ) -> anyhow::Result<()> {
            if matches!(
                command,
                ndp_proto::application::ApplicationMessage::Close { .. }
            ) {
                self.1.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
        fn release_input(&mut self) {}
        fn stop(&mut self) {}
    }

    #[tokio::test]
    async fn application_pump_negotiates_before_media_and_never_dispatches_global_input() {
        use ndp_proto::application::ApplicationMessage;
        use ndp_proto::{ControlMessage, FeatureFlags};
        tokio::time::timeout(Duration::from_secs(5), async {
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
                    Responder::new(&keys, b"application pump").unwrap(),
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
                    b"application pump",
                )
                .unwrap(),
                b"ticket",
                &config,
            )
            .await
            .unwrap();
            let (agent, receiver) = accepting.await.unwrap();
            let launch = nebula_common::ApplicationLaunch {
                launch_path: "/synthetic".into(),
                launch_args: vec![],
                working_dir: None,
            };
            let resource_id = nebula_common::ResourceId::new();
            let request = SessionRequest {
                session: nebula_common::SessionId::new(),
                resource_id: resource_id.as_uuid(),
                launch_target: nebula_common::LaunchTarget::Application {
                    resource_id,
                    version: launch.version(),
                    launch,
                },
                signed_ticket: None,
                policy: nebula_common::application::application_policy(
                    nebula_common::SessionPolicy::full(),
                ),
                role: nebula_common::SessionRole::Controller,
                relay_addr: String::new(),
                relay_pin: String::new(),
                pair_token: String::new(),
                client_key: String::new(),
            };
            let injected = Arc::new(AtomicUsize::new(0));
            let platform = Arc::new(AppPumpProbe(injected.clone(), Arc::default()));
            let (stop, stopped) = oneshot::channel();
            let running =
                tokio::spawn(
                    async move { pump(&request, agent, receiver, platform, stopped).await },
                );
            let caps = ndp_proto::Caps {
                video_codecs: vec![ndp_proto::VideoCodec::H264],
                audio_codecs: vec![ndp_proto::AudioCodec::Opus],
                displays: vec![ndp_proto::DisplayGeometry {
                    width: 640,
                    height: 480,
                    scale: 1.0,
                    refresh_hz: 30,
                }],
                audio: Default::default(),
                color: Default::default(),
                max_bitrate_bps: 4_000_000,
                features: FeatureFlags::APPLICATION_WINDOWS | FeatureFlags::CLIPBOARD,
            };
            let mut seq = 0;
            assert!(
                application_control(
                    &client,
                    &mut seq,
                    ControlMessage::Hello {
                        caps,
                        client_version: "test".into(),
                        client_os: "synthetic".into()
                    }
                )
                .await
            );
            let mut hello = false;
            let mut ack = false;
            let mut surface = false;
            loop {
                let message = incoming.recv().await.unwrap().unwrap();
                if message.channel == Channel::Video {
                    assert!(hello && ack && surface);
                    let (info, scoped) =
                        ndp_proto::VideoFrameInfo::split(&message.payload).unwrap();
                    assert_eq!(
                        ndp_proto::application::SurfaceFrameInfo::split(scoped)
                            .unwrap()
                            .0
                            .geometry_generation,
                        1
                    );
                    assert_eq!((info.display, info.width, info.height), (0, 640, 480));
                    break;
                }
                match ControlMessage::decode(&message.payload).unwrap() {
                    ControlMessage::Application(ApplicationMessage::Hello { .. }) => hello = true,
                    ControlMessage::HelloAck { caps, .. } => {
                        assert!(hello);
                        assert_eq!(caps.features, FeatureFlags::APPLICATION_WINDOWS);
                        assert!(caps.audio_codecs.is_empty());
                        ack = true;
                    }
                    ControlMessage::Application(ApplicationMessage::SurfaceUpsert { .. }) => {
                        assert!(ack);
                        surface = true;
                    }
                    _ => panic!("unexpected application control"),
                }
            }
            let event = InputEvent::mouse_move(0.5, 0.5, ndp_proto::Modifiers::NONE);
            client
                .send(
                    Channel::Input,
                    MsgHeader::new(MsgKind::InputBatch, 90, 0),
                    &event.to_bytes(),
                )
                .await
                .unwrap();
            assert!(
                application_control(
                    &client,
                    &mut seq,
                    ControlMessage::Application(ApplicationMessage::Input {
                        surface_id: 0,
                        geometry_generation: 2,
                        event: event.to_bytes()
                    })
                )
                .await
            );
            assert!(
                application_control(
                    &client,
                    &mut seq,
                    ControlMessage::Application(ApplicationMessage::Input {
                        surface_id: 0,
                        geometry_generation: 1,
                        event: event.to_bytes()
                    })
                )
                .await
            );
            while injected.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(injected.load(Ordering::SeqCst), 1);
            assert!(
                application_control(
                    &client,
                    &mut seq,
                    ControlMessage::Application(ApplicationMessage::Close {
                        surface_id: 0,
                        geometry_generation: 1
                    },)
                )
                .await
            );
            let mut removed = false;
            loop {
                let message = incoming.recv().await.unwrap().unwrap();
                if message.channel != Channel::Control {
                    continue;
                }
                match ControlMessage::decode(&message.payload).unwrap() {
                    ControlMessage::Application(ApplicationMessage::SurfaceRemove {
                        surface_id: 0,
                    }) => removed = true,
                    ControlMessage::Bye {
                        reason: ndp_proto::control::ByeReason::UserClosed,
                        ..
                    } => {
                        assert!(removed, "native removal must precede orderly completion");
                        assert!(
                            application_control(
                                &client,
                                &mut seq,
                                ControlMessage::Bye {
                                    reason: ndp_proto::control::ByeReason::UserClosed,
                                    detail: None,
                                }
                            )
                            .await
                        );
                        break;
                    }
                    _ => {}
                }
            }
            running.await.unwrap();
            drop(stop);
        })
        .await
        .unwrap();
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
                launch_target: Default::default(),
                signed_ticket: None,
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
                launch_target: Default::default(),
                signed_ticket: None,
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
                launch_target: Default::default(),
                signed_ticket: None,
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
        let task = tokio::spawn(start_video(
            Box::new(video),
            VideoConfig::default(),
            sink.into(),
        ));
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
