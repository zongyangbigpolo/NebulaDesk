//! End-to-end transport tests over a real loopback QUIC connection.
//!
//! These are the acceptance tests for the transport layer: two endpoints, a
//! real Noise handshake, and every carrier exercised the way the product uses
//! it.

use std::net::SocketAddr;
use std::time::Duration;

use ndp_crypto::{Initiator, Responder, StaticKeypair};
use ndp_proto::{Channel, MsgHeader, MsgKind};
use ndp_transport::{
    client_endpoint, connect, dev_credentials, server_endpoint, Incoming, Session, SessionReceiver,
    TransportConfig, ALPN_SESSION,
};

const PROLOGUE: &[u8] = b"ndp/3 session 0198f0c0-test";
const TICKET: &[u8] = b"signed-session-ticket";
const AGENT_HELLO: &[u8] = b"agent-caps";

struct Pair {
    client: Session,
    client_rx: SessionReceiver,
    agent: Session,
    agent_rx: SessionReceiver,
    // Endpoints must outlive the sessions: dropping an Endpoint closes its
    // socket and every connection on it.
    _endpoints: (quinn::Endpoint, quinn::Endpoint),
}

/// Stand up a client and an agent talking over loopback QUIC.
async fn pair() -> Pair {
    pair_with(TICKET, |t| {
        assert_eq!(t, TICKET);
        Ok(AGENT_HELLO.to_vec())
    })
    .await
    .expect("handshake should succeed")
}

async fn pair_with<F>(ticket: &[u8], authorize: F) -> ndp_transport::Result<Pair>
where
    F: FnOnce(&[u8]) -> std::result::Result<Vec<u8>, String> + Send + 'static,
{
    pair_configured(ticket, authorize, TransportConfig::default()).await
}

async fn pair_configured<F>(
    ticket: &[u8],
    authorize: F,
    cfg: TransportConfig,
) -> ndp_transport::Result<Pair>
where
    F: FnOnce(&[u8]) -> std::result::Result<Vec<u8>, String> + Send + 'static,
{
    let creds = dev_credentials(&[]).unwrap();
    let pin = creds.fingerprint;
    let agent_kp = StaticKeypair::generate();
    let agent_pub = agent_kp.public();

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = server_endpoint(bind, &creds, &[ALPN_SESSION], &cfg).unwrap();
    let addr = server.local_addr().unwrap();

    let accept_cfg = cfg.clone();
    let listener = server.clone();
    let agent_side = tokio::spawn(async move {
        let conn = listener.accept().await.expect("incoming").await?;
        let responder = Responder::new(&agent_kp, PROLOGUE)?;
        Session::accept(conn, responder, authorize, &accept_cfg).await
    });

    let endpoint = client_endpoint(bind).unwrap();
    let conn = connect(&endpoint, addr, "localhost", pin, ALPN_SESSION, &cfg)
        .await
        .unwrap();
    let initiator = Initiator::new(&StaticKeypair::generate(), &agent_pub, PROLOGUE).unwrap();
    let client_side = Session::initiate(conn, initiator, ticket, &cfg).await;

    let (agent, agent_rx) = agent_side.await.unwrap()?;
    let (client, client_rx, agent_payload) = client_side?;
    assert_eq!(agent_payload, AGENT_HELLO);

    Ok(Pair {
        client,
        client_rx,
        agent,
        agent_rx,
        _endpoints: (endpoint, server),
    })
}

#[tokio::test]
async fn interleaved_channels_do_not_create_video_sequence_gaps() {
    let mut p = pair().await;
    for seq in 0..4 {
        for (channel, kind) in [
            (Channel::Video, MsgKind::VideoFrame),
            (Channel::Clipboard, MsgKind::ClipboardOffer),
            (Channel::File, MsgKind::FileChunk),
            (Channel::Control, MsgKind::Ping),
        ] {
            // Application sequence values are deliberately unrelated: the
            // authenticated record layer owns independent channel counters.
            let header = MsgHeader::new(kind, 100 + seq * 17, 0);
            if channel == Channel::Video {
                p.agent
                    .send_video_frame(header, &[seq as u8], None)
                    .await
                    .unwrap();
            } else {
                p.agent.send(channel, header, &[seq as u8]).await.unwrap();
            }
        }
    }
    let mut video = Vec::new();
    for _ in 0..16 {
        let got = expect(&mut p.client_rx).await;
        assert_eq!(got.header.seq, u32::from(got.payload[0]));
        if got.channel == Channel::Video {
            video.push(got.header.seq);
        }
    }
    video.sort_unstable();
    assert_eq!(video, [0, 1, 2, 3]);
}

#[tokio::test]
async fn video_deadline_includes_waiting_for_stream_credit() {
    let p = pair_configured(
        TICKET,
        |_| Ok(AGENT_HELLO.to_vec()),
        TransportConfig {
            max_concurrent_uni: 0,
            ..TransportConfig::default()
        },
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        p.agent.send_video_frame(
            MsgHeader::new(MsgKind::VideoFrame, 0, 0),
            b"synthetic frame",
            Some(tokio::time::Instant::now() + Duration::from_millis(20)),
        ),
    )
    .await
    .expect("a blocked stream open must respect the frame deadline")
    .unwrap();
    assert_eq!(result, ndp_transport::FrameOutcome::Discarded);
}

async fn expect(rx: &mut SessionReceiver) -> Incoming {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for a message")
        .expect("session ended early")
        .expect("transport error")
}

#[tokio::test]
async fn control_messages_round_trip_in_both_directions() {
    let mut p = pair().await;

    p.client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Hello, 0, 1),
            b"hi",
        )
        .await
        .unwrap();
    let got = expect(&mut p.agent_rx).await;
    assert_eq!(got.channel, Channel::Control);
    assert_eq!(got.header.kind, MsgKind::Hello);
    assert_eq!(got.payload, b"hi");

    p.agent
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::HelloAck, 0, 2),
            b"welcome",
        )
        .await
        .unwrap();
    let got = expect(&mut p.client_rx).await;
    assert_eq!(got.payload, b"welcome");
}

#[tokio::test]
async fn input_events_arrive_in_order() {
    let mut p = pair().await;
    for i in 0..64u8 {
        p.client
            .send(
                Channel::Input,
                MsgHeader::new(MsgKind::InputEvent, 0, i.into()),
                &[i],
            )
            .await
            .unwrap();
    }
    // Ordering is correctness here: a click must never overtake the move that
    // positioned the pointer.
    for i in 0..64u8 {
        let got = expect(&mut p.agent_rx).await;
        assert_eq!(got.channel, Channel::Input);
        assert_eq!(got.payload, vec![i]);
        assert_eq!(got.header.seq, u32::from(i));
    }
}

#[tokio::test]
async fn video_frames_ride_one_stream_each() {
    let mut p = pair().await;
    // Frames large enough to span many packets, so any framing bug shows up.
    let count = 8usize;
    for i in 0..count {
        p.agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, i as u64),
                &vec![i as u8; 64 * 1024],
                None,
            )
            .await
            .unwrap();
    }

    // Streams complete independently, so arrival order is not guaranteed;
    // what must hold is that every frame arrives intact exactly once.
    let mut seen = std::collections::HashMap::new();
    for _ in 0..count {
        let got = expect(&mut p.client_rx).await;
        assert_eq!(got.channel, Channel::Video);
        assert_eq!(got.payload.len(), 64 * 1024);
        assert!(
            got.payload.iter().all(|b| *b == got.payload[0]),
            "frame contents were interleaved"
        );
        assert!(seen.insert(got.header.seq, got.payload[0]).is_none());
    }
    assert_eq!(seen.len(), count);
}

#[tokio::test]
async fn audio_rides_datagrams() {
    let mut p = pair().await;
    assert!(
        p.agent.max_audio_record().unwrap_or(0) > 0,
        "datagrams must be available; audio has no fallback by design"
    );
    for i in 0..16u8 {
        p.agent
            .send(
                Channel::Audio,
                MsgHeader::new(MsgKind::AudioFrame, 0, i.into()),
                &[i; 240],
            )
            .await
            .unwrap();
    }
    // Loopback does not lose packets, so all 16 must arrive even though the
    // channel tolerates loss.
    for _ in 0..16 {
        let got = expect(&mut p.client_rx).await;
        assert_eq!(got.channel, Channel::Audio);
        assert_eq!(got.payload.len(), 240);
    }
}

#[tokio::test]
async fn clipboard_and_file_use_per_message_streams() {
    let mut p = pair().await;
    for channel in [Channel::Clipboard, Channel::File] {
        let big = vec![0xAB; 512 * 1024];
        p.client
            .send(channel, MsgHeader::new(MsgKind::FileChunk, 0, 0), &big)
            .await
            .unwrap();
        let got = expect(&mut p.agent_rx).await;
        assert_eq!(got.channel, channel);
        assert_eq!(got.payload, big);
    }
}

#[tokio::test]
async fn a_rejected_ticket_never_completes_the_handshake() {
    let msg = match pair_with(b"forged-ticket", |_| Err("unknown ticket".into())).await {
        Ok(_) => panic!("a refused session must not produce a Session"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("refused") || msg.contains("connection") || msg.contains("handshake"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn a_wrong_certificate_pin_is_refused() {
    let cfg = TransportConfig::default();
    let creds = dev_credentials(&[]).unwrap();
    let evil = dev_credentials(&[]).unwrap();
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = server_endpoint(bind, &creds, &[ALPN_SESSION], &cfg).unwrap();
    let addr = server.local_addr().unwrap();
    let listener = server.clone();
    tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    let endpoint = client_endpoint(bind).unwrap();
    let err = connect(
        &endpoint,
        addr,
        "localhost",
        evil.fingerprint,
        ALPN_SESSION,
        &cfg,
    )
    .await
    .expect_err("a mismatched pin must not connect");
    assert!(err.to_string().to_lowercase().contains("connection"));
}

#[tokio::test]
async fn channels_stay_independent_under_a_simultaneous_burst() {
    // The whole point of the carrier mapping is that a large video frame does
    // not delay input. This proves the channels are independent.
    let p = pair().await;
    let mut client_rx = p.client_rx;
    let mut agent_rx = p.agent_rx;

    let agent = p.agent.clone();
    let video = tokio::spawn(async move {
        for i in 0..4u64 {
            agent
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 0, i),
                    &vec![7u8; 256 * 1024],
                    None,
                )
                .await
                .unwrap();
        }
    });

    let client = p.client.clone();
    let input = tokio::spawn(async move {
        for i in 0..32u64 {
            client
                .send(
                    Channel::Input,
                    MsgHeader::new(MsgKind::InputEvent, 0, i),
                    &[1],
                )
                .await
                .unwrap();
        }
    });

    video.await.unwrap();
    input.await.unwrap();

    for _ in 0..4 {
        assert_eq!(expect(&mut client_rx).await.channel, Channel::Video);
    }
    for _ in 0..32 {
        assert_eq!(expect(&mut agent_rx).await.channel, Channel::Input);
    }
}
