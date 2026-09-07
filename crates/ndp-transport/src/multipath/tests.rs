use super::*;
use crate::{
    client_endpoint, connect, dev_credentials, server_endpoint, TransportConfig, ALPN_SESSION,
};
use ndp_crypto::{Initiator, Responder, StaticKeypair};
use ndp_proto::MsgFlags;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

const PROLOGUE: &[u8] = b"multipath loopback: authorized logical session";

struct Proxy {
    address: SocketAddr,
    blackhole: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    async fn new(upstream: SocketAddr, delay: Duration) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let address = socket.local_addr().unwrap();
        let blackhole = Arc::new(AtomicBool::new(false));
        let drop_packets = blackhole.clone();
        let task = tokio::spawn(async move {
            let mut client = None;
            let mut buffer = vec![0; 65536];
            let mut forwarding = FuturesUnordered::<BoxFuture<'static, ()>>::new();
            loop {
                tokio::select! {
                    received = socket.recv_from(&mut buffer), if forwarding.len() < 512 => {
                        let (length, source) = received.unwrap();
                        let target = if source == upstream {
                            let Some(client) = client else { continue };
                            client
                        } else {
                            client = Some(source);
                            upstream
                        };
                        if drop_packets.load(Ordering::Relaxed) { continue; }
                        let bytes = buffer[..length].to_vec();
                        let socket = socket.clone();
                        let drop_packets = drop_packets.clone();
                        forwarding.push(Box::pin(async move {
                            tokio::time::sleep(delay).await;
                            if !drop_packets.load(Ordering::Relaxed) {
                                socket.send_to(&bytes, target).await.unwrap();
                            }
                        }));
                    }
                    Some(()) = forwarding.next(), if !forwarding.is_empty() => {}
                }
            }
        });
        Self {
            address,
            blackhole,
            task,
        }
    }
}

struct RelayBridge {
    front: quinn::Connection,
    back: quinn::Connection,
    task: JoinHandle<()>,
}

impl Drop for RelayBridge {
    fn drop(&mut self) {
        self.front.close(0u32.into(), b"test relay stopped");
        self.back.close(0u32.into(), b"test relay stopped");
        self.task.abort();
    }
}

async fn relay_stream(mut read: quinn::RecvStream, mut write: quinn::SendStream) {
    if tokio::io::copy(&mut read, &mut write).await.is_ok() {
        let _ = write.finish();
    } else {
        let _ = write.reset(0x11u32.into());
    }
}

impl RelayBridge {
    fn new(front: quinn::Connection, back: quinn::Connection) -> Self {
        let client = front.clone();
        let agent = back.clone();
        let task = tokio::spawn(async move {
            let mut streams = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = client.closed() => return,
                    _ = agent.closed() => return,
                    accepted = client.accept_bi(), if streams.len() < 64 => {
                        let Ok((to_client, from_client)) = accepted else { return };
                        let agent = agent.clone();
                        streams.spawn(async move {
                            if let Ok((to_agent, from_agent)) = agent.open_bi().await {
                                tokio::join!(relay_stream(from_client, to_agent), relay_stream(from_agent, to_client));
                            }
                        });
                    }
                    accepted = client.accept_uni(), if streams.len() < 64 => {
                        let Ok(from_client) = accepted else { return };
                        let agent = agent.clone();
                        streams.spawn(async move {
                            if let Ok(to_agent) = agent.open_uni().await {
                                relay_stream(from_client, to_agent).await;
                            }
                        });
                    }
                    accepted = agent.accept_uni(), if streams.len() < 64 => {
                        let Ok(from_agent) = accepted else { return };
                        let client = client.clone();
                        streams.spawn(async move {
                            if let Ok(to_client) = client.open_uni().await {
                                relay_stream(from_agent, to_client).await;
                            }
                        });
                    }
                    datagram = client.read_datagram() => {
                        let Ok(datagram) = datagram else { return };
                        if agent.send_datagram(datagram).is_err() { return; }
                    }
                    datagram = agent.read_datagram() => {
                        let Ok(datagram) = datagram else { return };
                        if client.send_datagram(datagram).is_err() { return; }
                    }
                    Some(_) = streams.join_next(), if !streams.is_empty() => {}
                }
            }
        });
        Self { front, back, task }
    }
}

struct TwoHopPair {
    client: Session,
    client_rx: SessionReceiver,
    agent: Session,
    agent_rx: SessionReceiver,
    agent_started: Instant,
    _endpoints: [quinn::Endpoint; 4],
    _proxies: [Proxy; 2],
    _bridge: RelayBridge,
}

/// Unlike a UDP-only proxy, this relay terminates two distinct QUIC
/// connections. A Noise reply can be QUIC-ACKed at the relay before the client
/// receives it, which is the startup race seen on the real WAN.
async fn two_hop_pair(keys: &(StaticKeypair, StaticKeypair)) -> TwoHopPair {
    let config = TransportConfig::default();
    let agent_credentials = dev_credentials(&[]).unwrap();
    let relay_credentials = dev_credentials(&[]).unwrap();
    let bind = "127.0.0.1:0".parse().unwrap();
    let agent_endpoint =
        server_endpoint(bind, &agent_credentials, &[ALPN_SESSION], &config).unwrap();
    let relay_front = server_endpoint(bind, &relay_credentials, &[ALPN_SESSION], &config).unwrap();
    let relay_back = client_endpoint(bind).unwrap();
    let client_endpoint = client_endpoint(bind).unwrap();
    // Each QUIC hop has ~350 ms RTT; the encrypted echo traverses both (~700).
    let back_proxy = Proxy::new(
        agent_endpoint.local_addr().unwrap(),
        Duration::from_millis(175),
    )
    .await;
    let front_proxy = Proxy::new(
        relay_front.local_addr().unwrap(),
        Duration::from_millis(175),
    )
    .await;
    let accepting = agent_endpoint.clone();
    let responder = Responder::new(&keys.1, PROLOGUE).unwrap();
    let accept_config = config.clone();
    let agent_task = tokio::spawn(async move {
        let connection = accepting.accept().await.unwrap().await.unwrap();
        let (session, receiver) = Session::accept(
            connection,
            responder,
            |_| Ok(b"multipath/1".to_vec()),
            &accept_config,
        )
        .await
        .unwrap();
        let started = Instant::now();
        let (session, receiver) = session.into_multipath(receiver);
        (session, receiver, started)
    });
    let back = connect(
        &relay_back,
        back_proxy.address,
        "localhost",
        agent_credentials.fingerprint,
        ALPN_SESSION,
        &config,
    )
    .await
    .unwrap();
    let listener = relay_front.clone();
    let front_task = tokio::spawn(async move { listener.accept().await.unwrap().await.unwrap() });
    let connection = connect(
        &client_endpoint,
        front_proxy.address,
        "localhost",
        relay_credentials.fingerprint,
        ALPN_SESSION,
        &config,
    )
    .await
    .unwrap();
    let bridge = RelayBridge::new(front_task.await.unwrap(), back);
    let (client, client_rx, _) = Session::initiate(
        connection,
        Initiator::new(&keys.0, &keys.1.public(), PROLOGUE).unwrap(),
        b"multipath/1",
        &config,
    )
    .await
    .unwrap();
    let (agent, agent_rx, agent_started) = agent_task.await.unwrap();
    TwoHopPair {
        client,
        client_rx,
        agent,
        agent_rx,
        agent_started,
        _endpoints: [client_endpoint, relay_front, relay_back, agent_endpoint],
        _proxies: [front_proxy, back_proxy],
        _bridge: bridge,
    }
}

struct NativePair {
    client: Session,
    client_rx: SessionReceiver,
    agent: Session,
    agent_rx: SessionReceiver,
    client_conn: quinn::Connection,
    agent_conn: quinn::Connection,
    _endpoints: (quinn::Endpoint, quinn::Endpoint),
    proxy: Option<Proxy>,
}

async fn native_pair(
    keys: &(StaticKeypair, StaticKeypair),
    delay: Option<Duration>,
    config: TransportConfig,
) -> NativePair {
    let credentials = dev_credentials(&[]).unwrap();
    let server = server_endpoint(
        "127.0.0.1:0".parse().unwrap(),
        &credentials,
        &[ALPN_SESSION],
        &config,
    )
    .unwrap();
    let server_address = server.local_addr().unwrap();
    let proxy = match delay {
        Some(delay) => Some(Proxy::new(server_address, delay).await),
        None => None,
    };
    let address = proxy.as_ref().map_or(server_address, |p| p.address);
    let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = server.clone();
    let responder = Responder::new(&keys.1, PROLOGUE).unwrap();
    let accept_config = config.clone();
    let accepting = tokio::spawn(async move {
        let connection = listener.accept().await.unwrap().await.unwrap();
        let result = Session::accept(
            connection.clone(),
            responder,
            |payload| {
                assert_eq!(payload, b"multipath/1");
                Ok(b"multipath/1".to_vec())
            },
            &accept_config,
        )
        .await
        .unwrap();
        (result, connection)
    });
    let client_conn = connect(
        &endpoint,
        address,
        "localhost",
        credentials.fingerprint,
        ALPN_SESSION,
        &config,
    )
    .await
    .unwrap();
    let (client, client_rx, greeting) = Session::initiate(
        client_conn.clone(),
        Initiator::new(&keys.0, &keys.1.public(), PROLOGUE).unwrap(),
        b"multipath/1",
        &config,
    )
    .await
    .unwrap();
    assert_eq!(greeting, b"multipath/1");
    let ((agent, agent_rx), agent_conn) = accepting.await.unwrap();
    NativePair {
        client,
        client_rx,
        agent,
        agent_rx,
        client_conn,
        agent_conn,
        _endpoints: (endpoint, server),
        proxy,
    }
}

struct LogicalPair {
    client: Session,
    client_rx: SessionReceiver,
    agent: Session,
    agent_rx: SessionReceiver,
    relay: (quinn::Connection, quinn::Connection),
    direct: Option<(quinn::Connection, quinn::Connection)>,
    _endpoints: Vec<(quinn::Endpoint, quinn::Endpoint)>,
    proxies: Vec<Proxy>,
}

impl LogicalPair {
    async fn new(
        keys: &(StaticKeypair, StaticKeypair),
        delay: Duration,
        config: TransportConfig,
    ) -> Self {
        let relay = native_pair(keys, Some(delay), config).await;
        let (client, client_rx) = relay.client.into_multipath(relay.client_rx);
        let (agent, agent_rx) = relay.agent.into_multipath(relay.agent_rx);
        Self {
            client,
            client_rx,
            agent,
            agent_rx,
            relay: (relay.client_conn, relay.agent_conn),
            direct: None,
            _endpoints: vec![relay._endpoints],
            proxies: relay.proxy.into_iter().collect(),
        }
    }

    async fn attach(
        &mut self,
        keys: &(StaticKeypair, StaticKeypair),
        delay: Option<Duration>,
        config: TransportConfig,
    ) {
        let direct = native_pair(keys, delay, config).await;
        self.client
            .attach_direct(direct.client, direct.client_rx)
            .await
            .unwrap();
        self.agent
            .attach_direct(direct.agent, direct.agent_rx)
            .await
            .unwrap();
        self.direct = Some((direct.client_conn, direct.agent_conn));
        self._endpoints.push(direct._endpoints);
        self.proxies.extend(direct.proxy);
    }
}

fn keys() -> (StaticKeypair, StaticKeypair) {
    (StaticKeypair::generate(), StaticKeypair::generate())
}

async fn route(session: &Session, kind: PathKind) {
    let mut changes = session.path_changes().unwrap();
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if changes.borrow_and_update().kind == kind {
                return;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("route did not change");
}

async fn expect(receiver: &mut SessionReceiver) -> Incoming {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

fn kind(channel: Channel) -> MsgKind {
    match channel {
        Channel::Control => MsgKind::Ping,
        Channel::Input => MsgKind::InputEvent,
        Channel::Clipboard => MsgKind::ClipboardData,
        Channel::File => MsgKind::FileChunk,
        Channel::Video => MsgKind::VideoFrame,
        Channel::Audio => MsgKind::AudioFrame,
    }
}

#[tokio::test]
async fn two_quic_relay_hops_allow_delayed_initiator_conversion() {
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut pair = two_hop_pair(&keys()).await;
        pair.agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
                b"media before initiator conversion",
                None,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(3100)).await;
        assert!(pair.agent_started.elapsed() > PATH_TIMEOUT);
        assert!(
            pair.agent.native_connection().close_reason().is_none(),
            "QUIC acceptance of Noise reply must not arm steady-state relay timeout"
        );
        assert!(pair.agent.path_stats()[0].end_to_end_rtt.is_none());
        let manager = pair.agent_rx._multipath.as_ref().unwrap().clone();
        assert!(
            !manager.shared.routes.lock().unwrap().paths[0]
                .as_ref()
                .unwrap()
                .peer_ready
        );

        let (client, mut client_rx) = pair.client.into_multipath(pair.client_rx);
        assert_eq!(
            expect(&mut client_rx).await.payload,
            b"media before initiator conversion"
        );
        client
            .send(
                Channel::Control,
                MsgHeader::new(MsgKind::Ping, 0, 0),
                b"client ready",
            )
            .await
            .unwrap();
        assert_eq!(expect(&mut pair.agent_rx).await.payload, b"client ready");
        tokio::time::timeout(Duration::from_secs(5), async {
            while pair.agent.path_stats()[0].end_to_end_rtt.is_none()
                || client.path_stats()[0].end_to_end_rtt.is_none()
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        let rtt = pair.agent.path_stats()[0].end_to_end_rtt.unwrap();
        assert!(
            rtt >= Duration::from_millis(600),
            "end-to-end probe must traverse both QUIC hops"
        );
        assert!(
            rtt < Duration::from_secs(2),
            "queued pre-readiness echoes must not contaminate the RTT"
        );
        assert!(rtt > pair.agent.connection_stats().path.rtt.mul_f64(1.4));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(pair.agent.native_connection().close_reason().is_none());
        pair.agent
            .send(
                Channel::Control,
                MsgHeader::new(MsgKind::Pong, 0, 0),
                b"steady relay alive",
            )
            .await
            .unwrap();
        assert_eq!(expect(&mut client_rx).await.payload, b"steady relay alive");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn relay_startup_grace_is_bounded_and_old_startup_echoes_are_not_rtt_samples() {
    let native = native_pair(&keys(), None, TransportConfig::default()).await;
    let (agent, receiver) = native.agent.into_multipath(native.agent_rx);
    let shared = &receiver._multipath.as_ref().unwrap().shared;
    {
        let mut routes = shared.routes.lock().unwrap();
        let path = routes.paths[0].as_mut().unwrap();
        path.last_echo = Instant::now() - PATH_TIMEOUT - Duration::from_millis(100);
        path.probes.push_back((123, path.last_echo));
    }
    shared.heartbeat();
    assert!(agent.native_connection().close_reason().is_none());
    shared.peer_ready(0);
    shared.echo(0, 123);
    {
        let routes = shared.routes.lock().unwrap();
        let path = routes.paths[0].as_ref().unwrap();
        assert!(path.peer_ready);
        assert!(path.probes.is_empty());
        assert!(path.rtt.is_none());
        assert!(path.last_echo.elapsed() < Duration::from_millis(100));
    }
    // A separate silent initiator must eventually exhaust its one startup
    // grace, even though QUIC transport-level ACKs/keepalives remain healthy.
    let native = native_pair(&keys(), None, TransportConfig::default()).await;
    let (agent, receiver) = native.agent.into_multipath(native.agent_rx);
    let shared = &receiver._multipath.as_ref().unwrap().shared;
    shared.routes.lock().unwrap().paths[0]
        .as_mut()
        .unwrap()
        .last_echo = Instant::now() - RELAY_STARTUP_TIMEOUT - Duration::from_millis(1);
    shared.heartbeat();
    assert!(agent.native_connection().close_reason().is_some());
    assert!(*shared.shutdown.borrow());
}

#[test]
fn relay_latency_budget_does_not_extend_direct_blackhole_deadline() {
    let wan_rtt = Duration::from_millis(1200);
    assert_eq!(
        probe_timeout(PathKind::Relay, false, None),
        RELAY_STARTUP_TIMEOUT
    );
    assert_eq!(
        probe_timeout(PathKind::Relay, true, Some(wan_rtt)),
        Duration::from_millis(5300)
    );
    assert_eq!(
        probe_timeout(PathKind::Relay, true, Some(Duration::from_secs(60))),
        MAX_RELAY_TIMEOUT
    );
    for ready in [false, true] {
        assert_eq!(
            probe_timeout(PathKind::Direct, ready, Some(wan_rtt)),
            PATH_TIMEOUT
        );
    }
}

#[tokio::test]
async fn direct_upgrade_bypasses_relay_and_failover_preserves_all_logical_channels() {
    tokio::time::timeout(Duration::from_secs(18), async {
        let keys = keys();
        let mut pair =
            LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
        pair.agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 88, 0).with_flags(MsgFlags::KEYFRAME),
                b"relay IDR",
                None,
            )
            .await
            .unwrap();
        let first = expect(&mut pair.client_rx).await;
        assert_eq!(first.header.seq, 0);
        assert!(first.header.flags.contains(MsgFlags::KEYFRAME));
        pair.agent
            .send(
                Channel::Audio,
                MsgHeader::new(MsgKind::AudioFrame, 99, 0),
                b"relay audio",
            )
            .await
            .unwrap();
        assert_eq!(expect(&mut pair.client_rx).await.header.seq, 0);
        for channel in RELIABLE {
            pair.client
                .send(
                    channel,
                    MsgHeader::new(kind(channel), 88, 0),
                    b"before direct",
                )
                .await
                .unwrap();
        }
        for _ in RELIABLE {
            assert_eq!(expect(&mut pair.agent_rx).await.header.seq, 0);
        }
        pair.attach(&keys, None, TransportConfig::default()).await;
        route(&pair.client, PathKind::Direct).await;
        route(&pair.agent, PathKind::Direct).await;
        let before = pair.agent.path_stats();
        let relay_bytes = before
            .iter()
            .find(|p| p.kind == PathKind::Relay)
            .unwrap()
            .udp_tx_bytes;
        let direct_bytes = before
            .iter()
            .find(|p| p.kind == PathKind::Direct)
            .unwrap()
            .udp_tx_bytes;
        let frame = vec![0xa5; 128 * 1024];
        pair.agent
            .send_video_frame(MsgHeader::new(MsgKind::VideoFrame, 88, 0), &frame, None)
            .await
            .unwrap();
        let received = expect(&mut pair.client_rx).await;
        assert_eq!(received.header.seq, 1);
        assert!(!received.header.flags.contains(MsgFlags::KEYFRAME));
        assert_eq!(received.payload, frame);
        pair.agent
            .send(
                Channel::Audio,
                MsgHeader::new(MsgKind::AudioFrame, 99, 0),
                b"direct audio",
            )
            .await
            .unwrap();
        let audio = expect(&mut pair.client_rx).await;
        assert_eq!(audio.header.seq, 1);
        assert_eq!(audio.payload, b"direct audio");
        let after = pair.agent.path_stats();
        assert!(
            after
                .iter()
                .find(|p| p.kind == PathKind::Direct)
                .unwrap()
                .udp_tx_bytes
                - direct_bytes
                > 100_000
        );
        assert!(
            after
                .iter()
                .find(|p| p.kind == PathKind::Relay)
                .unwrap()
                .udp_tx_bytes
                - relay_bytes
                < 16_000,
            "standby relay must not carry bulk media"
        );
        assert_eq!(
            pair.agent.remote_address(),
            pair.direct.as_ref().unwrap().1.remote_address()
        );

        // Freeze native stream writes after the record is retained but before
        // it can leave. All four logical writers must replay their in-flight
        // message, with no exposed path error and no reset Noise sequence.
        pair.direct.as_ref().unwrap().0.set_send_window(0);
        let manager = pair.client_rx._multipath.as_ref().unwrap().clone();
        let mut sending = Vec::new();
        for channel in RELIABLE {
            let client = pair.client.clone();
            sending.push(tokio::spawn(async move {
                client
                    .send(channel, MsgHeader::new(kind(channel), 999, 1), b"in flight")
                    .await
            }));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while RELIABLE
                .iter()
                .any(|c| manager.shared.allocated[c.id() as usize].load(Ordering::Acquire) != 2)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(sending.iter().all(|s| !s.is_finished()));
        pair.client.disconnect_direct().unwrap();
        assert!(pair.direct.as_ref().unwrap().0.close_reason().is_some());
        assert!(pair.relay.0.close_reason().is_none());
        route(&pair.client, PathKind::Relay).await;
        route(&pair.agent, PathKind::Relay).await;
        let fallback = pair.agent.path_stats();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].kind, PathKind::Relay);
        assert!(fallback[0].active);
        for task in sending {
            task.await.unwrap().unwrap();
        }
        let mut seen = Vec::new();
        for _ in RELIABLE {
            let message = expect(&mut pair.agent_rx).await;
            assert_eq!(message.header.seq, 1);
            assert_eq!(message.payload, b"in flight");
            seen.push(message.channel);
        }
        seen.sort();
        let mut channels = RELIABLE.to_vec();
        channels.sort();
        assert_eq!(seen, channels);
        for channel in RELIABLE {
            pair.client
                .send(
                    channel,
                    MsgHeader::new(kind(channel), 0, 2),
                    b"after failover",
                )
                .await
                .unwrap();
        }
        for _ in RELIABLE {
            let message = expect(&mut pair.agent_rx).await;
            assert_eq!(message.header.seq, 2);
            assert_eq!(message.payload, b"after failover");
        }
        pair.agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 2).with_flags(MsgFlags::KEYFRAME),
                b"relay recovery IDR",
                None,
            )
            .await
            .unwrap();
        let message = expect(&mut pair.client_rx).await;
        assert_eq!(message.header.seq, 2);
        assert!(message.header.flags.contains(MsgFlags::KEYFRAME));
        assert_eq!(message.payload, b"relay recovery IDR");
        pair.agent
            .send(
                Channel::Audio,
                MsgHeader::new(MsgKind::AudioFrame, 99, 0),
                b"relay recovery audio",
            )
            .await
            .unwrap();
        let audio = expect(&mut pair.client_rx).await;
        assert_eq!(audio.header.seq, 2);
        assert_eq!(audio.payload, b"relay recovery audio");
        assert!(
            tokio::time::timeout(Duration::from_millis(650), pair.agent_rx.recv())
                .await
                .is_err(),
            "logical retransmission must not duplicate delivery"
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn path_stats_and_probe_counter_updates_do_not_publish_media_epochs() {
    let keys = keys();
    let mut pair =
        LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
    pair.attach(&keys, None, TransportConfig::default()).await;
    route(&pair.client, PathKind::Direct).await;
    route(&pair.agent, PathKind::Direct).await;
    let mut changes = pair.client.path_changes().unwrap();
    let epoch = *changes.borrow_and_update();
    let before = pair.client.path_stats();
    assert_eq!(before.len(), 2);
    assert_ne!(before[0].path_id, before[1].path_id);
    assert_eq!(before.iter().filter(|path| path.active).count(), 1);
    assert_eq!(
        before.iter().find(|path| path.active).unwrap().kind,
        PathKind::Direct
    );
    assert!(before.iter().all(|path| path.end_to_end_rtt.is_some()));
    assert!(before
        .iter()
        .all(|path| path.udp_rx_bytes > 0 && path.udp_tx_bytes > 0));
    tokio::time::sleep(Duration::from_millis(700)).await;
    let after = pair.client.path_stats();
    assert!(
        !changes.has_changed().unwrap(),
        "probing and diagnostic snapshots must not reset media"
    );
    assert_eq!(*changes.borrow(), epoch);
    for previous in before {
        let current = after
            .iter()
            .find(|p| p.path_id == previous.path_id)
            .unwrap();
        assert!(current.udp_tx_bytes > previous.udp_tx_bytes);
        assert!(current.udp_rx_bytes > previous.udp_rx_bytes);
    }
    pair.client.close(0, b"finished diagnostics");
    assert!(pair.client.path_stats().is_empty());
}

#[tokio::test]
async fn native_path_stats_do_not_claim_end_to_end_probe_measurements() {
    let native = native_pair(&keys(), None, TransportConfig::default()).await;
    assert!(matches!(
        native.client.disconnect_direct(),
        Err(TransportError::NoDirectPath)
    ));
    let stats = native.client.path_stats();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].kind, PathKind::Relay);
    assert_eq!(stats[0].path_id, 0);
    assert!(stats[0].active);
    assert!(stats[0].end_to_end_rtt.is_none());
    assert_eq!(
        stats[0].udp_tx_bytes,
        native.client.connection_stats().udp_tx.bytes
    );
    assert_eq!(
        stats[0].udp_rx_bytes,
        native.client.connection_stats().udp_rx.bytes
    );
    assert!(native.client.path_changes().is_none());
    native.client.close(0, b"finished native diagnostics");
    assert!(native.client.path_stats().is_empty());
}

#[tokio::test]
async fn disconnecting_standby_direct_keeps_the_relay_and_epoch_unchanged() {
    let keys = keys();
    let mut pair = LogicalPair::new(&keys, Duration::ZERO, TransportConfig::default()).await;
    assert!(matches!(
        pair.client.disconnect_direct(),
        Err(TransportError::NoDirectPath)
    ));
    pair.attach(
        &keys,
        Some(Duration::from_millis(20)),
        TransportConfig::default(),
    )
    .await;
    let mut changes = pair.client.path_changes().unwrap();
    let before = *changes.borrow_and_update();
    assert_eq!(before.kind, PathKind::Relay);
    assert!(pair
        .client
        .path_stats()
        .iter()
        .any(|path| path.kind == PathKind::Direct && !path.active));
    pair.client.disconnect_direct().unwrap();
    assert!(!changes.has_changed().unwrap());
    assert_eq!(*changes.borrow(), before);
    assert!(pair
        .client
        .path_stats()
        .iter()
        .all(|path| path.kind == PathKind::Relay));
    assert!(pair.direct.as_ref().unwrap().0.close_reason().is_some());
    assert!(pair.relay.0.close_reason().is_none());
    assert!(matches!(
        pair.client.disconnect_direct(),
        Err(TransportError::NoDirectPath)
    ));
    pair.client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 0, 0),
            b"relay remains live",
        )
        .await
        .unwrap();
    assert_eq!(
        expect(&mut pair.agent_rx).await.payload,
        b"relay remains live"
    );
}

#[tokio::test]
async fn direct_blackhole_is_detected_without_waiting_for_quic_idle_timeout() {
    let keys = keys();
    let mut pair =
        LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
    pair.attach(&keys, Some(Duration::ZERO), TransportConfig::default())
        .await;
    route(&pair.client, PathKind::Direct).await;
    route(&pair.agent, PathKind::Direct).await;
    let started = Instant::now();
    pair.proxies[1].blackhole.store(true, Ordering::Relaxed);
    // A native write can complete into QUIC buffering while every UDP packet
    // is lost. Retention must not confuse that with a receive ACK.
    pair.client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputEvent, 0, 0),
            b"blackholed click",
        )
        .await
        .unwrap();
    route(&pair.client, PathKind::Relay).await;
    assert!(started.elapsed() < Duration::from_secs(3));
    let message = expect(&mut pair.agent_rx).await;
    assert_eq!(message.payload, b"blackholed click");
    assert_eq!(message.header.seq, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(650), pair.agent_rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn slower_direct_remains_standby_and_cannot_interrupt_the_relay() {
    let keys = keys();
    let mut pair = LogicalPair::new(&keys, Duration::ZERO, TransportConfig::default()).await;
    pair.attach(
        &keys,
        Some(Duration::from_millis(20)),
        TransportConfig::default(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(
        pair.client.path_changes().unwrap().borrow().kind,
        PathKind::Relay
    );
    assert_eq!(
        pair.agent.path_changes().unwrap().borrow().kind,
        PathKind::Relay
    );
    pair.proxies[1].blackhole.store(true, Ordering::Relaxed);
    for sequence in 0..8 {
        pair.client
            .send(
                Channel::Input,
                MsgHeader::new(MsgKind::InputEvent, 0, 0),
                &[sequence],
            )
            .await
            .unwrap();
        let message = expect(&mut pair.agent_rx).await;
        assert_eq!(message.header.seq, u32::from(sequence));
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        pair.client.path_changes().unwrap().borrow().kind,
        PathKind::Relay
    );
    pair.client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 0, 0),
            b"still healthy",
        )
        .await
        .unwrap();
    assert_eq!(expect(&mut pair.agent_rx).await.payload, b"still healthy");
}

#[tokio::test]
async fn wrong_identity_direct_is_rejected_without_disrupting_relay() {
    let mut pair = LogicalPair::new(&keys(), Duration::ZERO, TransportConfig::default()).await;
    let wrong = native_pair(&keys(), None, TransportConfig::default()).await;
    assert!(matches!(
        pair.client
            .attach_direct(wrong.client, wrong.client_rx)
            .await,
        Err(TransportError::Rejected { .. })
    ));
    pair.client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputEvent, 0, 0),
            b"authorized relay",
        )
        .await
        .unwrap();
    assert_eq!(
        expect(&mut pair.agent_rx).await.payload,
        b"authorized relay"
    );
    assert!(wrong.client_conn.close_reason().is_some());
}

#[tokio::test]
async fn cancelled_application_send_is_retained_and_delivered_exactly_once() {
    let keys = keys();
    let mut pair =
        LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
    pair.attach(&keys, None, TransportConfig::default()).await;
    route(&pair.client, PathKind::Direct).await;
    pair.direct.as_ref().unwrap().0.set_send_window(0);
    let client = pair.client.clone();
    let sender = tokio::spawn(async move {
        client
            .send(
                Channel::Input,
                MsgHeader::new(MsgKind::InputEvent, 0, 0),
                b"cancelled caller",
            )
            .await
    });
    let manager = pair.client_rx._multipath.as_ref().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.shared.allocated[Channel::Input.id() as usize].load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    sender.abort();
    assert!(sender.await.unwrap_err().is_cancelled());
    pair.direct
        .as_ref()
        .unwrap()
        .0
        .close(0u32.into(), b"fail cancelled caller path");
    let message = expect(&mut pair.agent_rx).await;
    assert_eq!(message.payload, b"cancelled caller");
    pair.client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputEvent, 0, 0),
            b"next click",
        )
        .await
        .unwrap();
    let message = expect(&mut pair.agent_rx).await;
    assert_eq!(message.payload, b"next click");
    assert_eq!(message.header.seq, 1);
}

#[tokio::test]
async fn blocked_keyframe_does_not_block_input_backpressure_or_revocation() {
    let config = TransportConfig {
        max_concurrent_uni: 0,
        ..TransportConfig::default()
    };
    let mut pair = LogicalPair::new(&keys(), Duration::ZERO, config).await;
    let agent = pair.agent.clone();
    let video = tokio::spawn(async move {
        agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
                b"blocked IDR",
                None,
            )
            .await
    });
    pair.agent
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputEvent, 0, 0),
            b"input bypasses video",
        )
        .await
        .unwrap();
    assert_eq!(
        expect(&mut pair.client_rx).await.payload,
        b"input bypasses video"
    );
    assert!(!video.is_finished());
    let agent = pair.agent.clone();
    let producer = tokio::spawn(async move {
        for sequence in 0..200 {
            agent
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::Ping, 0, sequence),
                    b"backpressured",
                )
                .await?;
        }
        Ok::<_, TransportError>(())
    });
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        !producer.is_finished(),
        "a stalled receiver must backpressure bounded send queues"
    );
    let manager = pair.agent_rx._multipath.as_ref().unwrap();
    assert!(
        manager.outboxes[Channel::Control.id() as usize]
            .as_ref()
            .unwrap()
            .count
            .available_permits()
            < WINDOW
    );
    pair.agent.close(99, b"revoked");
    tokio::time::timeout(Duration::from_millis(500), async {
        assert!(video.await.unwrap().is_err());
        assert!(producer.await.unwrap().is_err());
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match pair.client_rx.recv().await {
                Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .expect("logical revocation must close the peer");
}

#[tokio::test]
async fn logical_close_revokes_both_live_paths() {
    let keys = keys();
    let mut pair =
        LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
    pair.attach(&keys, None, TransportConfig::default()).await;
    route(&pair.client, PathKind::Direct).await;
    pair.agent.close(42, b"gateway revoked the session");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), pair.client_rx.recv())
            .await
            .unwrap(),
        Some(Err(_))
    ));
    assert!(pair.relay.0.close_reason().is_some());
    assert!(pair.direct.as_ref().unwrap().0.close_reason().is_some());
    assert!(pair
        .client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputEvent, 0, 0),
            b"must not send"
        )
        .await
        .is_err());
}

#[tokio::test]
async fn switching_does_not_cancel_an_ordered_write_on_a_live_old_path() {
    let keys = keys();
    let mut pair =
        LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
    pair.relay.0.set_send_window(0);
    let client = pair.client.clone();
    let sending = tokio::spawn(async move {
        client
            .send(
                Channel::Control,
                MsgHeader::new(MsgKind::Ping, 0, 0),
                b"old path pending",
            )
            .await
    });
    let manager = pair.client_rx._multipath.as_ref().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.shared.allocated[0].load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pair.attach(&keys, None, TransportConfig::default()).await;
    route(&pair.client, PathKind::Direct).await;
    sending.await.unwrap().unwrap();
    assert_eq!(
        expect(&mut pair.agent_rx).await.payload,
        b"old path pending"
    );
    assert!(
        pair.relay.0.close_reason().is_none(),
        "route changes must not cancel native ordered writes"
    );
    pair.relay.0.set_send_window(32 * 1024 * 1024);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(pair.relay.0.close_reason().is_none());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pair.agent_rx.recv())
            .await
            .is_err(),
        "late old-path delivery must be deduplicated"
    );
    pair.direct
        .as_ref()
        .unwrap()
        .0
        .close(0u32.into(), b"return to old relay stream");
    route(&pair.client, PathKind::Relay).await;
    pair.client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 0, 0),
            b"framing intact",
        )
        .await
        .unwrap();
    let message = expect(&mut pair.agent_rx).await;
    assert_eq!(message.header.seq, 1);
    assert_eq!(message.payload, b"framing intact");
}

#[tokio::test]
async fn blocked_relay_keyframe_is_discarded_when_direct_activates() {
    let keys = keys();
    let mut pair = LogicalPair::new(
        &keys,
        Duration::from_millis(15),
        TransportConfig {
            max_concurrent_uni: 0,
            ..TransportConfig::default()
        },
    )
    .await;
    let agent = pair.agent.clone();
    let sending = tokio::spawn(async move {
        agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
                b"blocked relay IDR",
                None,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!sending.is_finished());
    pair.attach(&keys, None, TransportConfig::default()).await;
    route(&pair.agent, PathKind::Direct).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(500), sending)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        FrameOutcome::Discarded
    );
    pair.agent
        .send_video_frame(
            MsgHeader::new(MsgKind::VideoFrame, 0, 0).with_flags(MsgFlags::KEYFRAME),
            b"fresh direct IDR",
            None,
        )
        .await
        .unwrap();
    let message = expect(&mut pair.client_rx).await;
    assert_eq!(
        message.header.seq, 1,
        "discarded media leaves a logical gap across epochs"
    );
    assert_eq!(message.payload, b"fresh direct IDR");
    assert!(message.header.flags.contains(MsgFlags::KEYFRAME));
    assert!(pair.relay.1.close_reason().is_none());
}

#[tokio::test]
async fn first_large_fallback_keyframe_survives_peer_epoch_notification_with_flags_and_sequence() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let keys = keys();
        let mut pair =
            LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
        pair.agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 999, 100).with_flags(MsgFlags::KEYFRAME),
                b"initial relay keyframe",
                None,
            )
            .await
            .unwrap();
        let first = expect(&mut pair.client_rx).await;
        assert_eq!(first.header.seq, 0);
        assert_eq!(first.header.flags, MsgFlags::KEYFRAME);
        pair.attach(&keys, None, TransportConfig::default()).await;
        route(&pair.client, PathKind::Direct).await;
        route(&pair.agent, PathKind::Direct).await;
        pair.agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 999, 200).with_flags(MsgFlags::KEYFRAME),
                b"direct keyframe",
                None,
            )
            .await
            .unwrap();
        let direct = expect(&mut pair.client_rx).await;
        assert_eq!(direct.header.seq, 1);
        assert_eq!(direct.header.flags, MsgFlags::KEYFRAME);
        pair.client.disconnect_direct().unwrap();
        route(&pair.agent, PathKind::Relay).await;

        // Hold the first new-route keyframe in flight, then independently
        // advance the sender's watch through incoming peer media. A watch
        // generation is not the sender's native route epoch.
        pair.relay.1.set_send_window(0);
        let flags = MsgFlags::KEYFRAME | MsgFlags::END_OF_UNIT;
        let payload = vec![0x5a; 512 * 1024];
        let frame = payload.clone();
        let agent = pair.agent.clone();
        let sending = tokio::spawn(async move {
            agent
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 999, 300).with_flags(flags),
                    &frame,
                    None,
                )
                .await
        });
        let shared = pair.agent_rx._multipath.as_ref().unwrap().shared.clone();
        tokio::time::timeout(Duration::from_secs(1), async {
            while shared.allocated[Channel::Video.id() as usize].load(Ordering::Acquire) != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!sending.is_finished());
        let mut changes = pair.agent.path_changes().unwrap();
        let before = *changes.borrow_and_update();
        let outbound_epoch = shared.selected().unwrap().epoch;
        pair.client
            .send(
                Channel::Audio,
                MsgHeader::new(MsgKind::AudioFrame, 0, 0),
                b"peer media on fallback epoch",
            )
            .await
            .unwrap();
        assert_eq!(expect(&mut pair.agent_rx).await.channel, Channel::Audio);
        assert!(changes.has_changed().unwrap());
        assert!(changes.borrow().epoch > before.epoch);
        assert_eq!(shared.selected().unwrap().epoch, outbound_epoch);
        assert_eq!(changes.borrow().kind, PathKind::Relay);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !sending.is_finished(),
            "peer-only media epoch must not discard the first new-route keyframe"
        );

        pair.relay.1.set_send_window(32 * 1024 * 1024);
        assert_eq!(sending.await.unwrap().unwrap(), FrameOutcome::Sent);
        let recovery = expect(&mut pair.client_rx).await;
        assert_eq!(
            recovery.header.seq, 2,
            "fresh native Noise counter must not replace logical sequence"
        );
        assert_eq!(recovery.header.flags, flags);
        assert_eq!(recovery.header.timestamp_us, 300);
        assert_eq!(recovery.payload, payload);
        for sequence in 3..11 {
            pair.agent
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 999, sequence)
                        .with_flags(MsgFlags::DISCARDABLE),
                    b"dependent P record",
                    None,
                )
                .await
                .unwrap();
            let delta = expect(&mut pair.client_rx).await;
            assert_eq!(delta.header.seq, sequence as u32);
            assert_eq!(delta.header.flags, MsgFlags::DISCARDABLE);
        }
        assert!(pair.relay.1.close_reason().is_none());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn all_path_blackholes_surface_a_terminal_error_and_stop_sends() {
    let keys = keys();
    let mut pair =
        LogicalPair::new(&keys, Duration::from_millis(15), TransportConfig::default()).await;
    pair.attach(&keys, Some(Duration::ZERO), TransportConfig::default())
        .await;
    route(&pair.client, PathKind::Direct).await;
    for proxy in &pair.proxies {
        proxy.blackhole.store(true, Ordering::Relaxed);
    }
    let message = tokio::time::timeout(Duration::from_secs(3), pair.client_rx.recv())
        .await
        .unwrap();
    assert!(matches!(message, Some(Err(_))));
    assert!(pair
        .client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 0, 0),
            b"after failure"
        )
        .await
        .is_err());
    assert!(pair.relay.0.close_reason().is_some());
    assert!(pair.direct.as_ref().unwrap().0.close_reason().is_some());
}

#[tokio::test]
async fn an_authenticated_invalid_envelope_closes_the_logical_session() {
    let mut pair = LogicalPair::new(&keys(), Duration::ZERO, TransportConfig::default()).await;
    let native = pair
        .client_rx
        ._multipath
        .as_ref()
        .unwrap()
        .shared
        .selected()
        .unwrap()
        .native;
    native
        .send_native(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 0, 0),
            b"not multipath/1",
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), pair.agent_rx.recv())
            .await
            .unwrap(),
        Some(Err(TransportError::MultipathProtocol(_)))
    ));
    assert!(pair.relay.1.close_reason().is_some());
}

#[tokio::test]
async fn dropping_all_logical_handles_releases_connections_and_tasks() {
    let pair = LogicalPair::new(&keys(), Duration::ZERO, TransportConfig::default()).await;
    let manager = pair.client_rx._multipath.as_ref().unwrap();
    let weak = Arc::downgrade(manager);
    let handles = manager.tasks.lock().unwrap().clone();
    let connection = pair.relay.0.clone();
    drop(pair.client);
    drop(pair.client_rx);
    assert!(weak.upgrade().is_none());
    tokio::time::timeout(Duration::from_millis(500), async {
        while handles.iter().any(|h| !h.is_finished()) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(connection.close_reason().is_some());
}

fn empty_shared() -> Shared {
    let (changes, _) = watch::channel(PathState {
        kind: PathKind::Relay,
        epoch: 0,
    });
    let (shutdown, _) = watch::channel(false);
    let (events, _) = mpsc::channel(1);
    Shared {
        routes: Mutex::new(Routes {
            paths: [None, None],
            active: PathKind::Relay,
            tx_epoch: 0,
            changed_at: Instant::now(),
            next_id: 1,
            next_probe: 0,
            direct_after: Instant::now(),
        }),
        changes,
        shutdown,
        events,
        terminal: Mutex::new(None),
        acknowledged: std::array::from_fn(|_| AtomicU64::new(0)),
        allocated: std::array::from_fn(|_| AtomicU64::new(0)),
        received: std::array::from_fn(|_| AtomicU64::new(0)),
        ack_changed: std::array::from_fn(|_| Notify::new()),
        max_record: TransportConfig::default().max_record,
    }
}

fn logical(channel: Channel, sequence: u64, epoch: u64, payload: &[u8]) -> Incoming {
    Incoming {
        channel,
        header: MsgHeader::new(kind(channel), 123, 0),
        payload: envelope(sequence, epoch, payload),
    }
}

#[tokio::test]
async fn reliable_receive_reorders_deduplicates_and_crosses_u32_wrap() {
    let shared = empty_shared();
    let base = u64::from(u32::MAX) - 1;
    shared.received[0].store(base, Ordering::Release);
    let mut inbox = Inbox::new();
    let (output, mut receiver) = mpsc::channel(4);
    for (sequence, epoch) in [(base + 1, 0), (base + 1, 1), (base + 2, 1), (base, 0)] {
        inbox
            .receive(
                &shared,
                0,
                logical(Channel::Control, sequence, epoch, b"event"),
                &output,
            )
            .unwrap();
    }
    assert_eq!(inbox.pending[0].len(), 3);
    for expected in [u32::MAX - 1, u32::MAX, 0] {
        let index = inbox.ready(&shared).unwrap();
        inbox.deliver(index, &shared, output.reserve().await.unwrap());
        assert_eq!(receiver.recv().await.unwrap().unwrap().header.seq, expected);
    }
    inbox
        .receive(
            &shared,
            0,
            logical(Channel::Control, base, 1, b"late retransmit"),
            &output,
        )
        .unwrap();
    assert!(inbox.ready(&shared).is_none());
    assert!(receiver.try_recv().is_err());
    assert_eq!(inbox.bytes, 0);
}

#[test]
fn duplicate_and_late_acks_never_rewind_or_ack_unsent_records() {
    let shared = empty_shared();
    shared.allocated[0].store(9, Ordering::Release);
    for next in [3u64, 3, 2, 9, 7] {
        let mut bytes = next.to_le_bytes().to_vec();
        bytes.resize(32, 0);
        shared.read_acks(&bytes).unwrap();
    }
    assert_eq!(shared.acknowledged[0].load(Ordering::Acquire), 9);
    let mut invalid = 10u64.to_le_bytes().to_vec();
    invalid.resize(32, 0);
    assert!(shared.read_acks(&invalid).is_err());
    assert!(shared.read_acks(&[0; 31]).is_err());
}

#[tokio::test]
async fn logical_window_bounds_and_conflicting_retransmissions_are_rejected() {
    let shared = empty_shared();
    let mut inbox = Inbox::new();
    let (output, _) = mpsc::channel(1);
    assert!(inbox
        .receive(
            &shared,
            0,
            logical(Channel::Input, WINDOW as u64, 0, b"too far"),
            &output
        )
        .is_err());
    inbox
        .receive(
            &shared,
            0,
            logical(Channel::Input, 0, 0, b"original"),
            &output,
        )
        .unwrap();
    assert!(inbox
        .receive(
            &shared,
            0,
            logical(Channel::Input, 0, 0, b"different"),
            &output
        )
        .is_err());
    assert_eq!(inbox.pending[Channel::Input.id() as usize].len(), 1);
}

#[test]
fn media_window_handles_wrap_duplicates_and_large_gaps() {
    let mut window = LossyWindow::default();
    let wrap = u64::from(u32::MAX);
    assert!(window.accept(wrap));
    assert!(window.accept(wrap + 1));
    assert!(window.accept(wrap - 1));
    assert!(!window.accept(wrap));
    assert!(window.accept(wrap + 100));
    assert!(!window.accept(wrap + 1));
    assert!(window.accept(u64::MAX));
    assert!(!window.accept(u64::MAX));
}

#[tokio::test]
async fn late_old_epoch_media_is_dropped_without_replaying_frames() {
    let shared = empty_shared();
    let mut inbox = Inbox::new();
    let (output, mut receiver) = mpsc::channel(4);
    for (sequence, epoch) in [(10, 2), (9, 1), (10, 2), (11, 2)] {
        inbox
            .receive(
                &shared,
                0,
                logical(Channel::Video, sequence, epoch, b"frame"),
                &output,
            )
            .unwrap();
    }
    assert_eq!(receiver.recv().await.unwrap().unwrap().header.seq, 10);
    assert_eq!(receiver.recv().await.unwrap().unwrap().header.seq, 11);
    assert!(receiver.try_recv().is_err());
    assert_eq!(shared.changes.borrow().epoch, 1);
}

#[test]
fn direct_selection_requires_a_material_latency_advantage() {
    assert!(faster(Duration::from_millis(10), Duration::from_millis(30)));
    assert!(!faster(
        Duration::from_micros(100),
        Duration::from_micros(900)
    ));
    assert!(!faster(
        Duration::from_millis(25),
        Duration::from_millis(30)
    ));
}
