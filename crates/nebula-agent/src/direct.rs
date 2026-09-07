//! A session-scoped, mutually authenticated direct QUIC listener.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ndp_crypto::{PublicKey, Responder, StaticKeypair};
use ndp_proto::{Channel, MsgHeader, MsgKind};
use ndp_signal::direct::{candidate_ip, DirectOffer, ALPN_DIRECT, MAX_DIRECT_ADDRESSES};
use ndp_transport::{
    server_endpoint, ServerCredentials, Session, SessionReceiver, TransportConfig,
};
use tokio::task::{JoinHandle, JoinSet};

pub(crate) struct DirectService {
    task: JoinHandle<()>,
}

impl DirectService {
    pub(crate) fn spawn(
        id: nebula_common::SessionId,
        keys: StaticKeypair,
        expected: PublicKey,
        session: Session,
    ) -> Self {
        let task = tokio::spawn(async move {
            if let Err(error) = run(id, Arc::new(keys), expected, session).await {
                tracing::warn!(session = %id, %error, "direct listener stopped; retaining relay");
            }
        });
        Self { task }
    }
}

impl Drop for DirectService {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Endpoints(Vec<quinn::Endpoint>);

impl Drop for Endpoints {
    fn drop(&mut self) {
        for endpoint in &self.0 {
            endpoint.close(0u32.into(), b"direct listener closed");
        }
    }
}

struct Candidate(Option<Session>);

impl Drop for Candidate {
    fn drop(&mut self) {
        if let Some(session) = &self.0 {
            session.close(0x20, b"direct candidate not attached");
        }
    }
}

async fn run(
    id: nebula_common::SessionId,
    keys: Arc<StaticKeypair>,
    expected: PublicKey,
    session: Session,
) -> anyhow::Result<()> {
    let config = TransportConfig::default();
    let credentials = ServerCredentials::load_or_generate(None, None, &["localhost".into()])?;
    let mut endpoints = Endpoints(Vec::new());
    for bind in ["0.0.0.0:0", "[::]:0"] {
        match server_endpoint(bind.parse()?, &credentials, &[ALPN_DIRECT], &config) {
            Ok(endpoint) => endpoints.0.push(endpoint),
            Err(error) => tracing::debug!(bind, %error, "direct address family unavailable"),
        }
    }
    anyhow::ensure!(
        !endpoints.0.is_empty(),
        "no direct QUIC socket could be bound"
    );
    let mut offer = DirectOffer {
        version: 1,
        session: id,
        id: uuid::Uuid::now_v7(),
        addresses: Vec::new(),
        certificate_pin: credentials.fingerprint.to_hex(),
    };
    let slots = Arc::new(tokio::sync::Semaphore::new(2));
    let mut listeners = JoinSet::new();
    for endpoint in &endpoints.0 {
        listeners.spawn(accept_loop(
            endpoint.clone(),
            offer.clone(),
            Arc::clone(&keys),
            expected,
            session.clone(),
            Arc::clone(&slots),
        ));
    }
    let mut publish = tokio::time::interval(Duration::from_secs(10));
    publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = publish.tick() => {
                let interfaces = tokio::task::spawn_blocking(if_addrs::get_if_addrs).await??;
                let mut addresses = Vec::new();
                for interface in interfaces {
                    let ip = interface.ip();
                    if !candidate_ip(ip) {
                        continue;
                    }
                    for endpoint in &endpoints.0 {
                        let bound = endpoint.local_addr()?;
                        if ip.is_ipv4() == bound.is_ipv4() {
                            addresses.push(SocketAddr::new(ip, bound.port()));
                        }
                    }
                }
                addresses.sort_by_key(|address| {
                    let family = u8::from(matches!(address.ip(), IpAddr::V6(_)));
                    (family, *address)
                });
                addresses.dedup();
                addresses.truncate(MAX_DIRECT_ADDRESSES);
                if addresses.is_empty() {
                    tracing::debug!(session = %id, "no usable direct interface; retaining relay");
                    continue;
                }
                let changed = offer.addresses != addresses;
                offer.addresses = addresses;
                offer.validate(id)?;
                if changed {
                    tracing::info!(session = %id, addresses = ?offer.addresses, "advertising authenticated direct candidates");
                }
                let payload = offer.encode()?;
                session.send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::PathCandidates, 0, 0),
                    &payload,
                ).await?;
            }
            result = listeners.join_next() => {
                match result {
                    Some(Ok(())) => anyhow::bail!("a direct listener closed"),
                    Some(Err(error)) => return Err(error.into()),
                    None => anyhow::bail!("all direct listeners closed"),
                }
            }
        }
    }
}

async fn accept_loop(
    endpoint: quinn::Endpoint,
    offer: DirectOffer,
    keys: Arc<StaticKeypair>,
    expected: PublicKey,
    session: Session,
    slots: Arc<tokio::sync::Semaphore>,
) {
    let mut handshakes = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let Ok(permit) = Arc::clone(&slots).try_acquire_owned() else {
                    incoming.refuse();
                    continue;
                };
                let (offer, keys, session) = (offer.clone(), Arc::clone(&keys), session.clone());
                handshakes.spawn(async move {
                    let _permit = permit;
                    let result = tokio::time::timeout(Duration::from_secs(5), async {
                        let conn = incoming.await?;
                        let peer = conn.remote_address();
                        let (native, receiver) =
                            accept_candidate(conn, &offer, &keys, expected).await?;
                        let mut candidate = Candidate(Some(native.clone()));
                        session.attach_direct(native, receiver).await?;
                        candidate.0.take();
                        tracing::info!(session = %offer.session, %peer, "direct QUIC peer authenticated");
                        Ok::<_, anyhow::Error>(())
                    }).await;
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::debug!(%error, "direct candidate refused"),
                        Err(_) => tracing::debug!("direct candidate handshake timed out"),
                    }
                });
            }
            result = handshakes.join_next(), if !handshakes.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "direct handshake task failed");
                }
            }
        }
    }
}

struct Handshake(Option<quinn::Connection>);

impl Drop for Handshake {
    fn drop(&mut self) {
        if let Some(conn) = &self.0 {
            conn.close(0x20u32.into(), b"direct handshake not completed");
        }
    }
}

async fn accept_candidate(
    conn: quinn::Connection,
    offer: &DirectOffer,
    keys: &StaticKeypair,
    expected: PublicKey,
) -> anyhow::Result<(Session, SessionReceiver)> {
    let mut handshake = Handshake(Some(conn.clone()));
    let responder = Responder::new(keys, &offer.prologue())?;
    let (native, receiver) = Session::accept(
        conn,
        responder,
        |payload| {
            if payload != offer.id.as_bytes() {
                return Err("wrong direct listener binding".into());
            }
            Ok(b"direct-ready".to_vec())
        },
        &TransportConfig::default(),
    )
    .await?;
    anyhow::ensure!(
        native.peer_static() == Some(expected),
        "direct peer is not the gateway-authorised client"
    );
    handshake.0.take();
    Ok((native, receiver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndp_crypto::Initiator;
    use ndp_transport::{client_endpoint, connect, dev_credentials};

    struct Listener {
        offer: DirectOffer,
        agent: StaticKeypair,
        client: StaticKeypair,
        endpoints: Endpoints,
    }

    impl Listener {
        fn new() -> Self {
            let credentials = dev_credentials(&[]).unwrap();
            let server = server_endpoint(
                "127.0.0.1:0".parse().unwrap(),
                &credentials,
                &[ALPN_DIRECT],
                &TransportConfig::default(),
            )
            .unwrap();
            let offer = DirectOffer {
                version: 1,
                session: nebula_common::SessionId::new(),
                id: uuid::Uuid::now_v7(),
                addresses: vec![server.local_addr().unwrap()],
                certificate_pin: credentials.fingerprint.to_hex(),
            };
            Self {
                offer,
                agent: StaticKeypair::generate(),
                client: StaticKeypair::generate(),
                endpoints: Endpoints(vec![
                    server,
                    client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap(),
                ]),
            }
        }

        async fn tls(&self) -> (quinn::Connection, quinn::Connection) {
            let config = TransportConfig::default();
            let server = async { self.endpoints.0[0].accept().await.unwrap().await.unwrap() };
            let client = connect(
                &self.endpoints.0[1],
                self.offer.addresses[0],
                "localhost",
                ndp_transport::CertificateFingerprint::from_hex(&self.offer.certificate_pin)
                    .unwrap(),
                ALPN_DIRECT,
                &config,
            );
            let (server, client) = tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(server, client)
            })
            .await
            .unwrap();
            (server, client.unwrap())
        }

        async fn handshake(
            &self,
            client: &StaticKeypair,
            offer: &DirectOffer,
            payload: &[u8],
        ) -> (
            anyhow::Result<(Session, SessionReceiver)>,
            ndp_transport::Result<(Session, SessionReceiver, Vec<u8>)>,
        ) {
            let (server, conn) = self.tls().await;
            let config = TransportConfig::default();
            let accepting =
                accept_candidate(server, &self.offer, &self.agent, self.client.public());
            let initiating = Session::initiate(
                conn,
                Initiator::new(client, &self.agent.public(), &offer.prologue()).unwrap(),
                payload,
                &config,
            );
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(accepting, initiating)
            })
            .await
            .expect("direct Noise handshake did not finish")
        }
    }

    #[tokio::test]
    async fn exact_original_client_identity_authenticates_and_carries_encrypted_records() {
        let listener = Listener::new();
        // Loopback is test-only; discovery must continue rejecting it.
        assert!(listener.offer.validate(listener.offer.session).is_err());
        let (agent, client) = listener
            .handshake(
                &listener.client,
                &listener.offer,
                listener.offer.id.as_bytes(),
            )
            .await;
        let (agent, mut agent_rx) = agent.unwrap();
        let (client, mut client_rx, greeting) = client.unwrap();
        assert_eq!(greeting, b"direct-ready");
        assert_eq!(agent.peer_static(), Some(listener.client.public()));
        assert_eq!(client.peer_static(), Some(listener.agent.public()));
        client
            .send(
                Channel::Input,
                MsgHeader::new(MsgKind::InputBatch, 0, 0),
                b"authenticated input",
            )
            .await
            .unwrap();
        let input = tokio::time::timeout(Duration::from_secs(1), agent_rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(input.payload, b"authenticated input");
        agent
            .send(
                Channel::Control,
                MsgHeader::new(MsgKind::Pong, 0, 0),
                b"authenticated control",
            )
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_secs(1), client_rx.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(pong.payload, b"authenticated control");
    }

    #[tokio::test]
    async fn wrong_original_client_key_is_rejected_before_attachment() {
        let listener = Listener::new();
        let (agent, _) = listener
            .handshake(
                &StaticKeypair::generate(),
                &listener.offer,
                listener.offer.id.as_bytes(),
            )
            .await;
        assert!(agent
            .expect_err("another client must not receive an attachable session")
            .to_string()
            .contains("gateway-authorised client"));
    }

    #[tokio::test]
    async fn wrong_session_or_listener_prologue_is_rejected() {
        let listener = Listener::new();
        for change_session in [false, true] {
            let mut wrong = listener.offer.clone();
            if change_session {
                wrong.session = nebula_common::SessionId::new();
            } else {
                wrong.id = uuid::Uuid::now_v7();
            }
            let (agent, client) = listener
                .handshake(&listener.client, &wrong, listener.offer.id.as_bytes())
                .await;
            assert!(agent.is_err());
            assert!(client.is_err());
        }
    }

    #[tokio::test]
    async fn wrong_encrypted_listener_payload_is_rejected() {
        let listener = Listener::new();
        let (agent, client) = listener
            .handshake(
                &listener.client,
                &listener.offer,
                uuid::Uuid::now_v7().as_bytes(),
            )
            .await;
        assert!(agent
            .expect_err("wrong listener payload must not be attachable")
            .to_string()
            .contains("wrong direct listener binding"));
        assert!(client.is_err());
    }

    #[tokio::test]
    async fn cancelling_native_handshake_closes_even_a_retained_connection() {
        let listener = Listener::new();
        let (server, client) = listener.tls().await;
        let retained = server.clone();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            accept_candidate(
                server,
                &listener.offer,
                &listener.agent,
                listener.client.public(),
            )
            .await
        });
        tokio::task::yield_now().await;
        tasks.abort_all();
        assert!(tasks.join_next().await.unwrap().unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), client.closed())
            .await
            .expect("cancelled handshake retained a live QUIC connection");
        assert!(retained.close_reason().is_some());
    }

    #[tokio::test]
    async fn listener_drop_closes_connections_and_aborts_both_handshake_slots() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let listener = Listener::new();
            let (agent, client) = listener
                .handshake(
                    &listener.client,
                    &listener.offer,
                    listener.offer.id.as_bytes(),
                )
                .await;
            let (agent, receiver) = agent.unwrap();
            let (agent, _incoming) = agent.into_multipath(receiver);
            let (_client, _client_rx, _) = client.unwrap();
            let endpoint = listener.endpoints.0[0].clone();
            let slots = Arc::new(tokio::sync::Semaphore::new(2));
            let permits = Arc::clone(&slots);
            let offer = listener.offer.clone();
            let expected = listener.client.public();
            let keys = Arc::new(listener.agent.clone());
            let service = DirectService {
                task: tokio::spawn(async move {
                    let _endpoints = Endpoints(vec![endpoint.clone()]);
                    accept_loop(endpoint, offer, keys, expected, agent, permits).await;
                }),
            };
            let handle = service.task.abort_handle();
            let config = TransportConfig::default();
            let pin =
                ndp_transport::CertificateFingerprint::from_hex(&listener.offer.certificate_pin)
                    .unwrap();
            let mut connections = Vec::new();
            for _ in 0..2 {
                connections.push(
                    connect(
                        &listener.endpoints.0[1],
                        listener.offer.addresses[0],
                        "localhost",
                        pin,
                        ALPN_DIRECT,
                        &config,
                    )
                    .await
                    .unwrap(),
                );
            }
            while slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
            assert!(connect(
                &listener.endpoints.0[1],
                listener.offer.addresses[0],
                "localhost",
                pin,
                ALPN_DIRECT,
                &config,
            )
            .await
            .is_err());
            drop(service);
            for connection in connections {
                connection.closed().await;
            }
            while !handle.is_finished() || slots.available_permits() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping listener must close QUIC and release all handshake tasks");
    }
}
