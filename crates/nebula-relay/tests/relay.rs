//! End-to-end tests through a real relay.
//!
//! The point of these is that a full encrypted session — handshake, control
//! messages, video frames on their own streams, audio datagrams — survives
//! being forwarded by the relay unchanged, and that the relay itself admits
//! nobody it should not.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ndp_crypto::{Initiator, Responder, StaticKeypair};
use ndp_proto::{Channel, MsgHeader, MsgKind};
use ndp_signal::{PairKey, RelayHello, RelayHelloAck, Side};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Incoming, Session, SessionReceiver,
    TransportConfig, ALPN_RELAY, ALPN_SESSION,
};
use nebula_common::SessionId;
use nebula_relay::{Config, Relay};

const PROLOGUE: &[u8] = b"ndp/3 relay test";
const TICKET: &[u8] = b"signed-session-ticket";
const AGENT_HELLO: &[u8] = b"agent-caps";

struct Harness {
    relay: Arc<Relay>,
    addr: SocketAddr,
    pin: CertificateFingerprint,
    keys: PairKey,
}

/// Start a relay on an ephemeral port, as an operator would provision one.
async fn harness() -> Harness {
    let secret = ndp_signal::generate_secret();
    let relay = Arc::new(
        Relay::bind(&Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            pair_secret: secret.clone(),
            ..Config::default()
        })
        .expect("the relay should bind"),
    );
    let addr = relay.local_addr().unwrap();
    let pin = CertificateFingerprint::from_hex(&relay.fingerprint).unwrap();

    let running = relay.clone();
    tokio::spawn(async move { running.run().await });

    Harness {
        relay,
        addr,
        pin,
        keys: PairKey::new(secret.as_bytes()).unwrap(),
    }
}

/// Connect to the relay and present a pairing token, as either peer would.
async fn attach(
    h: &Harness,
    token: &str,
) -> Result<(quinn::Endpoint, quinn::Connection, RelayHelloAck), String> {
    let cfg = TransportConfig::default();
    let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).map_err(|e| e.to_string())?;
    let conn = connect(&endpoint, h.addr, "localhost", h.pin, ALPN_RELAY, &cfg)
        .await
        .map_err(|e| e.to_string())?;

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    ndp_signal::write_message(
        &mut send,
        &RelayHello {
            pair_token: token.to_string(),
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    let ack: RelayHelloAck =
        tokio::time::timeout(Duration::from_secs(10), ndp_signal::read_message(&mut recv))
            .await
            .map_err(|_| "timed out waiting for the relay".to_string())?
            .map_err(|e| e.to_string())?;

    Ok((endpoint, conn, ack))
}

struct Spliced {
    client: Session,
    client_rx: SessionReceiver,
    agent: Session,
    agent_rx: SessionReceiver,
    // The endpoints own the sockets; dropping them would close the sessions.
    _endpoints: (quinn::Endpoint, quinn::Endpoint),
}

/// Run a complete session between two peers that meet at the relay.
async fn session(h: &Harness) -> Spliced {
    let id = SessionId::new();
    let client_token = h.keys.mint(id, Side::Client);
    let agent_token = h.keys.mint(id, Side::Agent);

    let agent_keys = StaticKeypair::generate();
    let agent_public = agent_keys.public();

    // The agent arrives first, as it does in production: the gateway pushes
    // it a session request before the client has finished connecting. Its
    // acknowledgement is therefore deferred until the client shows up, so it
    // must be awaited concurrently rather than before.
    let agent_attach = {
        let cfg = TransportConfig::default();
        let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = connect(&endpoint, h.addr, "localhost", h.pin, ALPN_RELAY, &cfg)
            .await
            .unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        ndp_signal::write_message(
            &mut send,
            &RelayHello {
                pair_token: agent_token,
            },
        )
        .await
        .unwrap();

        let agent_conn = conn.clone();
        tokio::spawn(async move {
            let ack: RelayHelloAck = ndp_signal::read_message(&mut recv).await.unwrap();
            assert!(matches!(ack, RelayHelloAck::Spliced));
            let responder = Responder::new(&agent_keys, PROLOGUE).unwrap();
            let session = Session::accept(
                agent_conn,
                responder,
                |ticket| {
                    assert_eq!(ticket, TICKET, "the agent must see the client's ticket");
                    Ok(AGENT_HELLO.to_vec())
                },
                &TransportConfig::default(),
            )
            .await;
            (endpoint, session)
        })
    };

    let (client_endpoint, client_conn, client_ack) = attach(h, &client_token).await.unwrap();
    assert!(matches!(client_ack, RelayHelloAck::Spliced));

    let initiator = Initiator::new(&StaticKeypair::generate(), &agent_public, PROLOGUE).unwrap();
    let (client, client_rx, agent_payload) =
        Session::initiate(client_conn, initiator, TICKET, &TransportConfig::default())
            .await
            .expect("the end-to-end handshake should complete through the relay");
    assert_eq!(agent_payload, AGENT_HELLO);

    let (agent_endpoint, agent) = agent_attach.await.unwrap();
    let (agent, agent_rx) = agent.unwrap();

    Spliced {
        client,
        client_rx,
        agent,
        agent_rx,
        _endpoints: (client_endpoint, agent_endpoint),
    }
}

async fn expect(rx: &mut SessionReceiver) -> Incoming {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for a message")
        .expect("the session ended early")
        .expect("transport error")
}

#[tokio::test]
async fn a_full_session_survives_the_relay() {
    let h = harness().await;
    let mut s = session(&h).await;

    s.client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Hello, 0, 1),
            b"hello from the client",
        )
        .await
        .unwrap();
    let got = expect(&mut s.agent_rx).await;
    assert_eq!(got.channel, Channel::Control);
    assert_eq!(got.payload, b"hello from the client");

    s.agent
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::HelloAck, 0, 2),
            b"hello from the agent",
        )
        .await
        .unwrap();
    assert_eq!(
        expect(&mut s.client_rx).await.payload,
        b"hello from the agent"
    );

    // Input travels client to agent on the ordered carrier.
    s.client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputBatch, 0, 3),
            &[7u8; 32],
        )
        .await
        .unwrap();
    let got = expect(&mut s.agent_rx).await;
    assert_eq!(got.channel, Channel::Input);
    assert_eq!(got.payload, vec![7u8; 32]);

    s.client.close(0, b"done");
    s.agent.close(0, b"done");
    h.relay.shutdown();
}

#[tokio::test]
async fn video_frames_keep_their_own_streams_through_the_relay() {
    let h = harness().await;
    let mut s = session(&h).await;

    // A keyframe large enough to span many relay chunks, so a bug in the copy
    // loop shows up as corruption rather than passing by luck.
    let keyframe: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let delta = vec![0xAAu8; 4096];

    s.agent
        .send(
            Channel::Video,
            MsgHeader::new(MsgKind::VideoFrame, 10, 0),
            &keyframe,
        )
        .await
        .unwrap();
    s.agent
        .send(
            Channel::Video,
            MsgHeader::new(MsgKind::VideoFrame, 11, 0),
            &delta,
        )
        .await
        .unwrap();

    // Each frame is its own stream, so they may arrive in either order.
    let mut seen = Vec::new();
    for _ in 0..2 {
        let got = expect(&mut s.client_rx).await;
        assert_eq!(got.channel, Channel::Video);
        assert_eq!(got.header.kind, MsgKind::VideoFrame);
        seen.push(got.payload);
    }

    let key = seen
        .iter()
        .find(|p| p.len() == keyframe.len())
        .expect("the keyframe should arrive intact");
    assert_eq!(*key, keyframe, "the relay corrupted a large frame");

    let d = seen
        .iter()
        .find(|p| p.len() == delta.len())
        .expect("the delta should arrive");
    assert_eq!(*d, delta);

    h.relay.shutdown();
}

#[tokio::test]
async fn audio_datagrams_are_forwarded() {
    let h = harness().await;
    let mut s = session(&h).await;

    let audio = vec![0x5Au8; 240];
    // Datagrams may be dropped by design, so sending a few is the honest
    // test: the assertion is that forwarding works, not that UDP is reliable.
    for seq in 0..8u32 {
        s.agent
            .send(
                Channel::Audio,
                MsgHeader::new(MsgKind::AudioFrame, seq, 0),
                &audio,
            )
            .await
            .unwrap();
    }

    let got = expect(&mut s.client_rx).await;
    assert_eq!(got.channel, Channel::Audio);
    assert_eq!(got.payload, audio);

    h.relay.shutdown();
}

#[tokio::test]
async fn the_relay_refuses_a_forged_token() {
    let h = harness().await;

    // A token minted with a different deployment's secret.
    let stranger = PairKey::new(ndp_signal::generate_secret().as_bytes()).unwrap();
    let forged = stranger.mint(SessionId::new(), Side::Client);

    // Either the relay answers with a rejection or it closes outright; both
    // are correct, and neither may result in a splice.
    if let Ok((_e, _c, ack)) = attach(&h, &forged).await {
        assert!(
            matches!(ack, RelayHelloAck::Rejected { .. }),
            "a forged token must never be spliced"
        );
    }

    h.relay.shutdown();
}

#[tokio::test]
async fn the_relay_will_not_splice_a_session_to_itself() {
    let h = harness().await;
    let id = SessionId::new();
    let token = h.keys.mint(id, Side::Client);

    // The first client parks, waiting for an agent that never comes.
    let cfg = TransportConfig::default();
    let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let conn = connect(&endpoint, h.addr, "localhost", h.pin, ALPN_RELAY, &cfg)
        .await
        .unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    ndp_signal::write_message(
        &mut send,
        &RelayHello {
            pair_token: token.clone(),
        },
    )
    .await
    .unwrap();

    // Give the relay a moment to register the first arrival.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A second connection presenting the same client token must be refused
    // rather than spliced to the first: otherwise anyone who captured a
    // client token could talk to a waiting client.
    if let Ok((_e, _c, ack)) = attach(&h, &token).await {
        assert!(
            matches!(ack, RelayHelloAck::Rejected { .. }),
            "two clients must not be spliced together"
        );
    }

    h.relay.shutdown();
}

#[tokio::test]
async fn a_peer_that_sends_junk_is_dropped() {
    let h = harness().await;
    let cfg = TransportConfig::default();
    let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let conn = connect(&endpoint, h.addr, "localhost", h.pin, ALPN_RELAY, &cfg)
        .await
        .unwrap();

    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    // A length prefix that promises far more than it delivers.
    send.write_all(&1_000_000u32.to_le_bytes()).await.unwrap();
    send.write_all(b"nope").await.unwrap();
    send.finish().unwrap();

    // The relay must close the connection rather than wait forever.
    let result = tokio::time::timeout(Duration::from_secs(10), recv.read_to_end(1024)).await;
    assert!(result.is_ok(), "the relay hung on a malformed message");

    h.relay.shutdown();
}

#[tokio::test]
async fn the_relay_serves_only_its_own_protocol() {
    let h = harness().await;
    let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    // A peer that speaks the session ALPN directly at the relay must be
    // refused during the TLS handshake.
    let result = connect(
        &endpoint,
        h.addr,
        "localhost",
        h.pin,
        ALPN_SESSION,
        &TransportConfig::default(),
    )
    .await;
    assert!(result.is_err(), "the relay accepted an unexpected protocol");

    h.relay.shutdown();
}
