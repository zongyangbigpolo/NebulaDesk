use super::*;
use crate::{client_endpoint, connect, dev_credentials, server_endpoint, ALPN_SESSION};
use ndp_crypto::StaticKeypair;
use ndp_proto::MsgKind;
use std::time::Duration;

struct RawPeer {
    agent: Session,
    peer: quinn::Connection,
    opener: RecordOpener,
    _receiver: SessionReceiver,
    _ordered: Vec<(quinn::SendStream, quinn::RecvStream)>,
    _endpoints: (quinn::Endpoint, quinn::Endpoint),
}

/// A real Noise/QUIC peer whose video reader is deliberately not running.
async fn raw_peer() -> RawPeer {
    let config = TransportConfig {
        stream_receive_window: 1024,
        ..TransportConfig::default()
    };
    let credentials = dev_credentials(&[]).unwrap();
    let server = server_endpoint(
        "127.0.0.1:0".parse().unwrap(),
        &credentials,
        &[ALPN_SESSION],
        &config,
    )
    .unwrap();
    let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let keys = StaticKeypair::generate();
    let mut initiator =
        Initiator::new(&StaticKeypair::generate(), &keys.public(), b"blocked video").unwrap();
    let listener = server.clone();
    let accept_config = config.clone();
    let accepting = tokio::spawn(async move {
        let conn = listener.accept().await.unwrap().await.unwrap();
        conn.set_send_window(1024);
        Session::accept(
            conn,
            Responder::new(&keys, b"blocked video").unwrap(),
            |_| Ok(Vec::new()),
            &accept_config,
        )
        .await
        .unwrap()
    });
    let peer = connect(
        &endpoint,
        server.local_addr().unwrap(),
        "localhost",
        credentials.fingerprint,
        ALPN_SESSION,
        &config,
    )
    .await
    .unwrap();
    let mut ordered = Vec::new();
    for channel in ORDERED_CHANNELS {
        let (mut send, recv) = peer.open_bi().await.unwrap();
        send.write_all(&wire::channel_prefix(channel, channel.priority()))
            .await
            .unwrap();
        ordered.push((send, recv));
    }
    write_framed(
        &mut ordered[0].0,
        &initiator.write_first(b"ticket").unwrap(),
        MAX_HANDSHAKE_MSG,
    )
    .await
    .unwrap();
    let reply = read_framed(&mut ordered[0].1, MAX_HANDSHAKE_MSG)
        .await
        .unwrap()
        .unwrap();
    let done = initiator.read_second(&reply).unwrap();
    let (agent, receiver) = accepting.await.unwrap();
    RawPeer {
        agent,
        peer,
        opener: done.opener,
        _receiver: receiver,
        _ordered: ordered,
        _endpoints: (endpoint, server),
    }
}

async fn partial_video_is_reset(deadline: bool) {
    let p = raw_peer().await;
    let agent = p.agent.clone();
    let sending = tokio::spawn(async move {
        agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
                &vec![7; 64 * 1024],
                deadline.then(|| Instant::now() + Duration::from_millis(100)),
            )
            .await
    });
    let mut partial = p.peer.accept_uni().await.unwrap();
    // Arrival of the prefix proves that this is a partial record, not merely
    // cancellation before the send had a chance to run.
    let mut prefix = [0; CHANNEL_PREFIX_LEN];
    partial.read_exact(&mut prefix).await.unwrap();
    assert_eq!(wire::parse_channel(&prefix).unwrap(), Channel::Video);
    assert!(
        !sending.is_finished(),
        "the unread frame must be backpressured"
    );
    if deadline {
        assert_eq!(sending.await.unwrap().unwrap(), FrameOutcome::Discarded);
    } else {
        sending.abort();
        assert!(sending.await.unwrap_err().is_cancelled());
    }
    assert!(
        matches!(
            partial.read_to_end(128 * 1024).await,
            Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code)))
                if code == quinn::VarInt::from_u32(CODE_STALE_FRAME)
        ),
        "cancellation must reset, never finish a truncated authenticated record"
    );

    p.agent
        .send_video_frame(
            MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
            b"recovery",
            None,
        )
        .await
        .unwrap();
    let mut next = p.peer.accept_uni().await.unwrap();
    next.read_exact(&mut prefix).await.unwrap();
    let record = next.read_to_end(1024).await.unwrap();
    let (header, payload) = p.opener.open(Channel::Video, &record).unwrap();
    assert_eq!(header.seq, 1, "the abandoned frame leaves a detectable gap");
    assert_eq!(payload, b"recovery");
}

#[tokio::test]
async fn cancelling_a_partial_video_resets_instead_of_finishing() {
    tokio::time::timeout(Duration::from_secs(5), partial_video_is_reset(false))
        .await
        .expect("cancellation regression timed out");
}

#[tokio::test]
async fn a_video_write_deadline_resets_the_partial_record() {
    tokio::time::timeout(Duration::from_secs(5), partial_video_is_reset(true))
        .await
        .expect("deadline regression timed out");
}

#[tokio::test]
async fn video_deadline_includes_a_blocked_channel_prefix() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let p = raw_peer().await;
        p.agent.inner.conn.set_send_window(0);
        assert_eq!(
            p.agent
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                    b"not writable",
                    Some(Instant::now() + Duration::from_millis(20)),
                )
                .await
                .unwrap(),
            FrameOutcome::Discarded
        );
        let mut stream = p.peer.accept_uni().await.unwrap();
        assert!(matches!(
            stream.read_to_end(1024).await,
            Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code)))
                if code == quinn::VarInt::from_u32(CODE_STALE_FRAME)
        ));
    })
    .await
    .expect("the prefix write must be covered by the deadline");
}

async fn receive_raw_record(
    peer: &RawPeer,
    channel: Channel,
    record: &[u8],
    tx: &mpsc::Sender<Result<Incoming>>,
) {
    let mut send = peer.agent.inner.conn.open_uni().await.unwrap();
    send.write_all(&wire::channel_prefix(channel, channel.priority()))
        .await
        .unwrap();
    send.write_all(record).await.unwrap();
    send.finish().unwrap();
    read_message_stream(
        peer.peer.accept_uni().await.unwrap(),
        peer.opener.clone(),
        tx.clone(),
        TransportConfig::default().max_record,
    )
    .await;
}

#[tokio::test]
async fn video_older_than_the_replay_window_does_not_end_the_session() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let peer = raw_peer().await;
        let header = MsgHeader::new(MsgKind::VideoFrame, 0, 0);
        let sealer = &peer.agent.inner.sealer;
        let old = sealer.seal(Channel::Video, header, b"late").unwrap();
        for _ in 0..64 {
            sealer.seal(Channel::Video, header, b"overtaken").unwrap();
        }
        let latest = sealer.seal(Channel::Video, header, b"latest").unwrap();
        let (tx, mut incoming) = mpsc::channel(8);
        receive_raw_record(&peer, Channel::Video, &latest, &tx).await;
        assert_eq!(incoming.recv().await.unwrap().unwrap().header.seq, 65);

        for rejected in [&old, &latest] {
            receive_raw_record(&peer, Channel::Video, rejected, &tx).await;
            assert!(
                matches!(incoming.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
                "a stale or duplicate video must be discarded, not delivered or fatal"
            );
        }
        let recovery = sealer
            .seal(
                Channel::Video,
                header.with_flags(MsgFlags::KEYFRAME),
                b"recovery",
            )
            .unwrap();
        receive_raw_record(&peer, Channel::Video, &recovery, &tx).await;
        let frame = incoming.recv().await.unwrap().unwrap();
        assert_eq!(frame.header.seq, 66);
        assert_eq!(frame.payload, b"recovery");
    })
    .await
    .expect("late-video regression timed out");
}

#[tokio::test]
async fn video_authentication_failures_are_still_fatal() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let peer = raw_peer().await;
        let mut record = peer
            .agent
            .inner
            .sealer
            .seal(
                Channel::Video,
                MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                b"tampered",
            )
            .unwrap();
        *record.last_mut().unwrap() ^= 1;
        let (tx, mut incoming) = mpsc::channel(1);
        receive_raw_record(&peer, Channel::Video, &record, &tx).await;
        assert!(matches!(
            incoming.recv().await.unwrap(),
            Err(TransportError::Crypto(
                ndp_crypto::CryptoError::Decrypt { .. }
            ))
        ));
    })
    .await
    .expect("authentication regression timed out");
}

#[tokio::test]
async fn reliable_message_replays_remain_errors() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for channel in [Channel::Clipboard, Channel::File] {
            let peer = raw_peer().await;
            let header = MsgHeader::new(MsgKind::FileChunk, 0, 0);
            let sealer = &peer.agent.inner.sealer;
            let old = sealer.seal(channel, header, b"old").unwrap();
            for _ in 0..64 {
                sealer.seal(channel, header, b"ahead").unwrap();
            }
            let latest = sealer.seal(channel, header, b"latest").unwrap();
            let (tx, mut incoming) = mpsc::channel(2);
            receive_raw_record(&peer, channel, &latest, &tx).await;
            incoming.recv().await.unwrap().unwrap();
            receive_raw_record(&peer, channel, &old, &tx).await;
            assert!(matches!(
                incoming.recv().await.unwrap(),
                Err(TransportError::Crypto(
                    ndp_crypto::CryptoError::Replay { .. }
                ))
            ));
        }
    })
    .await
    .expect("reliable replay regression timed out");
}
