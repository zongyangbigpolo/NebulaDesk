//! Codec-independent media recovery over local encrypted QUIC, without a
//! manager, database, display, or native capture/decoder.

use std::collections::BTreeMap;
use std::time::Duration;

use ndp_crypto::{Initiator, Responder, StaticKeypair};
use ndp_proto::{Channel, MsgFlags, MsgHeader, MsgKind};
use ndp_transport::{
    client_endpoint, connect, dev_credentials, server_endpoint, Incoming, Session, SessionReceiver,
    TransportConfig, ALPN_SESSION,
};
use nebula_client::video::VideoOrder;

async fn receive(receiver: &mut SessionReceiver) -> Incoming {
    tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("message deadline")
        .expect("session ended")
        .expect("authenticated message")
}

async fn ask_on_timer(order: &mut VideoOrder, client: &Session, agent_rx: &mut SessionReceiver) {
    let at = order
        .recovery_deadline()
        .expect("recovery must be scheduled");
    tokio::time::sleep_until(at.into()).await;
    assert!(order.recover().ask_for_keyframe);
    client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::CapsUpdate, 0, 0),
            b"",
        )
        .await
        .unwrap();
    let request = receive(agent_rx).await;
    assert_eq!(request.channel, Channel::Control);
    assert_eq!(request.header.kind, MsgKind::CapsUpdate);
    assert!(
        !order.recover().ask_for_keyframe,
        "retry must be rate limited"
    );
}

#[tokio::test]
async fn idle_recovery_and_reordering_use_independent_authenticated_video_sequences() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let config = TransportConfig {
            max_concurrent_uni: 0,
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
        let public = keys.public();
        let listener = server.clone();
        let accept_config = config.clone();
        let accepting = tokio::spawn(async move {
            let conn = listener.accept().await.unwrap().await.unwrap();
            Session::accept(
                conn,
                Responder::new(&keys, b"stream recovery").unwrap(),
                |_| Ok(Vec::new()),
                &accept_config,
            )
            .await
            .unwrap()
        });
        let connection = connect(
            &endpoint,
            server.local_addr().unwrap(),
            "localhost",
            credentials.fingerprint,
            ALPN_SESSION,
            &config,
        )
        .await
        .unwrap();
        let (client, mut client_rx, _) = Session::initiate(
            connection.clone(),
            Initiator::new(&StaticKeypair::generate(), &public, b"stream recovery").unwrap(),
            b"ticket",
            &config,
        )
        .await
        .unwrap();
        let (agent, mut agent_rx) = accepting.await.unwrap();

        let mut order = VideoOrder::new();
        ask_on_timer(&mut order, &client, &mut agent_rx).await;
        // Stream credit is actually exhausted, so this abandons a real send
        // and consumes a video sequence, rather than faking transport success.
        assert_eq!(
            agent
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                    b"abandoned",
                    Some(tokio::time::Instant::now() + Duration::from_millis(20)),
                )
                .await
                .unwrap(),
            ndp_transport::FrameOutcome::Discarded
        );
        // Nothing arrived at all, yet recovery must retry over control.
        ask_on_timer(&mut order, &client, &mut agent_rx).await;
        connection.set_max_concurrent_uni_streams(16u32.into());

        for seq in 1..=6u8 {
            agent
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 999, 0).with_flags(
                        if seq == 1 || seq == 4 {
                            MsgFlags::KEYFRAME
                        } else {
                            MsgFlags::DISCARDABLE
                        },
                    ),
                    &[seq],
                    None,
                )
                .await
                .unwrap();
            for (channel, kind) in [
                (Channel::Clipboard, MsgKind::ClipboardOffer),
                (Channel::File, MsgKind::FileChunk),
            ] {
                agent
                    .send(channel, MsgHeader::new(kind, 999, 0), &[seq])
                    .await
                    .unwrap();
            }
        }
        let mut frames = BTreeMap::new();
        for _ in 0..18 {
            let message = receive(&mut client_rx).await;
            if message.channel == Channel::Video {
                assert_eq!(message.header.seq, u32::from(message.payload[0]));
                frames.insert(message.header.seq, message);
            }
        }
        assert_eq!(frames.len(), 6);
        // Deliberately schedule the decrypted frames in a repeatable arrival
        // order, including a refresh keyframe overtaken by its successors.
        let mut released = Vec::new();
        for seq in [1, 3, 6, 5, 4, 2] {
            let message = frames.remove(&seq).unwrap();
            let ready = order.accept(
                message.header.seq,
                message.header.flags.contains(MsgFlags::KEYFRAME),
                message.payload,
            );
            assert!(!ready.ask_for_keyframe);
            released.extend(ready.frames.into_iter().flatten());
        }
        assert_eq!(released, [1, 4, 5, 6]);
        assert!(order.recovery_deadline().is_none());

        // Hold the next authenticated predecessor (7) behind successor 8.
        // Even this one-frame gap must recover when the desktop goes idle.
        for seq in 7..=8u8 {
            agent
                .send_video_frame(MsgHeader::new(MsgKind::VideoFrame, 0, 0), &[seq], None)
                .await
                .unwrap();
        }
        for _ in 0..2 {
            let message = receive(&mut client_rx).await;
            frames.insert(message.header.seq, message);
        }
        let successor = frames.remove(&8).unwrap();
        assert!(order.accept(8, false, successor.payload).frames.is_empty());
        // The prior request's rate limit may still apply to this new gap.
        tokio::time::sleep_until(order.recovery_deadline().unwrap().into()).await;
        let recovery = order.recover();
        if recovery.ask_for_keyframe {
            client
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::CapsUpdate, 0, 0),
                    b"",
                )
                .await
                .unwrap();
            assert_eq!(
                receive(&mut agent_rx).await.header.kind,
                MsgKind::CapsUpdate
            );
        }
        ask_on_timer(&mut order, &client, &mut agent_rx).await;
        agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
                &[9],
                None,
            )
            .await
            .unwrap();
        let keyframe = receive(&mut client_rx).await;
        assert_eq!(
            order
                .accept(keyframe.header.seq, true, keyframe.payload)
                .frames,
            [vec![9]]
        );
        let predecessor = frames.remove(&7).unwrap();
        assert!(order
            .accept(predecessor.header.seq, false, predecessor.payload)
            .frames
            .is_empty());
        assert!(order.recovery_deadline().is_none());
    })
    .await
    .expect("local stream recovery test timed out");
}
