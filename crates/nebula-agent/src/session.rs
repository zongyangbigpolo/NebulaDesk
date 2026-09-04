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

use crate::media::{EncodedFrame, Platform, VideoConfig};

/// How many encoded frames may wait for the network.
///
/// Small on purpose. A backlog of video is not useful data, it is latency
/// that has already been committed to; dropping at the source lets the
/// encoder recover with a keyframe instead of delivering a slideshow.
const FRAME_QUEUE: usize = 3;

/// How long a frame may sit in the transport before it is abandoned.
const FRAME_DEADLINE: Duration = Duration::from_millis(120);

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

    let mut video = match platform.video() {
        Ok(video) => video,
        Err(error) => {
            tracing::error!(%error, "no video source; the session will carry input only");
            session.close(0x21, b"no video source");
            return tally;
        }
    };
    let mut input = match platform.input() {
        Ok(input) => Some(input),
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

    let mut seq: u32 = 0;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
                let flags = if frame.keyframe { MsgFlags::KEYFRAME } else { MsgFlags::DISCARDABLE };
                let header = MsgHeader::new(MsgKind::VideoFrame, seq, frame.timestamp_us)
                    .with_flags(flags);
                seq = seq.wrapping_add(1);

                let deadline = tokio::time::Instant::now() + FRAME_DEADLINE;
                match session.send_video_frame(header, &frame.data, Some(deadline)).await {
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
                if !handle(request, &session, &mut video, input.as_deref_mut(), message).await {
                    break;
                }
            }
        }
    }

    video.stop();
    tracing::info!(session = %request.session, sent = tally.sent, received = tally.received, "session ended");
    tally
}

/// Act on one message from the client. Returns false when the session should
/// end.
async fn handle(
    request: &SessionRequest,
    session: &Session,
    video: &mut Box<dyn crate::media::VideoSource>,
    input: Option<&mut dyn crate::media::InputInjector>,
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
            video.request_keyframe();
            true
        }

        (Channel::Control, MsgKind::Bye) => false,

        (channel, kind) => {
            tracing::debug!(?channel, ?kind, "ignoring an unhandled message");
            true
        }
    }
}
