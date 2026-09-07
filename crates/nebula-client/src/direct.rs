//! Bounded direct candidate probing without blocking the media receive loop.

use std::time::Duration;

use ndp_crypto::{Initiator, PublicKey, StaticKeypair};
use ndp_proto::{Channel, MsgKind};
use ndp_signal::direct::{DirectOffer, ALPN_DIRECT};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Incoming, Session, SessionReceiver,
    TransportConfig,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Incoming application records, with authenticated path offers handled
/// internally. Both GUI and headless clients get the same automatic routing.
pub struct ConnectedReceiver {
    incoming: SessionReceiver,
    direct: Option<Connector>,
    id: nebula_common::SessionId,
}

impl ConnectedReceiver {
    pub(crate) fn new(
        incoming: SessionReceiver,
        session: Session,
        keys: StaticKeypair,
        agent: PublicKey,
        id: nebula_common::SessionId,
        multipath: bool,
    ) -> Self {
        Self {
            incoming,
            direct: multipath.then(|| Connector::spawn(session, keys, agent)),
            id,
        }
    }

    /// Receive the next application message; direct-path probing runs in a
    /// separate bounded worker, so unreachable candidates cannot freeze video.
    pub async fn recv(&mut self) -> Option<ndp_transport::Result<Incoming>> {
        loop {
            let message = self.incoming.recv().await?;
            if let (Some(direct), Ok(message)) = (&self.direct, &message) {
                if message.channel == Channel::Control
                    && message.header.kind == MsgKind::PathCandidates
                {
                    match DirectOffer::decode(&message.payload)
                        .and_then(|offer| offer.validate(self.id).map(|()| offer))
                    {
                        Ok(offer) => direct.offer(offer),
                        Err(error) => tracing::warn!(%error, "refusing invalid direct-path offer"),
                    }
                    continue;
                }
            }
            return Some(message);
        }
    }
}

struct Connector {
    offers: mpsc::Sender<DirectOffer>,
    task: JoinHandle<()>,
}

impl Connector {
    fn spawn(session: Session, keys: StaticKeypair, agent: PublicKey) -> Self {
        let (offers, mut pending) = mpsc::channel::<DirectOffer>(1);
        let task = tokio::spawn(async move {
            let mut attached: Option<(Endpoint, quinn::Connection)> = None;
            while let Some(offer) = pending.recv().await {
                // A healthy direct connection may still be a standby while the
                // route selector measures it. Do not redial that listener.
                if attached
                    .as_ref()
                    .is_some_and(|(_, conn)| conn.close_reason().is_none())
                {
                    continue;
                }
                attached = None;
                for address in &offer.addresses {
                    let attempted = tokio::time::timeout(
                        Duration::from_secs(5),
                        dial(&offer, *address, &keys, agent),
                    )
                    .await;
                    match attempted {
                        Ok(Ok(Dialed {
                            native,
                            incoming,
                            endpoint,
                            connection,
                        })) => {
                            let mut candidate = Candidate(Some(native.clone()));
                            match session.attach_direct(native, incoming).await {
                                Ok(()) => {
                                    candidate.0.take();
                                    attached = Some((endpoint, connection));
                                    tracing::info!(session = %offer.session, peer = %address, "authenticated direct candidate attached");
                                }
                                Err(error) => {
                                    tracing::debug!(%error, "direct candidate was not attached")
                                }
                            }
                            break;
                        }
                        Ok(Err(error)) => {
                            tracing::debug!(peer = %address, %error, "direct candidate unavailable; keeping relay")
                        }
                        Err(_) => {
                            tracing::debug!(peer = %address, "direct candidate timed out; keeping relay")
                        }
                    }
                }
            }
        });
        Self { offers, task }
    }

    fn offer(&self, offer: DirectOffer) {
        match self.offers.try_send(offer) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!("direct probe already queued; ignoring repeated offer");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("direct probe worker stopped; retaining current route");
            }
        }
    }
}

impl Drop for Connector {
    fn drop(&mut self) {
        self.task.abort();
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

struct Endpoint(quinn::Endpoint);

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"direct connector closed");
    }
}

struct Dialed {
    native: Session,
    incoming: SessionReceiver,
    endpoint: Endpoint,
    connection: quinn::Connection,
}

async fn dial(
    offer: &DirectOffer,
    address: std::net::SocketAddr,
    keys: &StaticKeypair,
    agent: PublicKey,
) -> anyhow::Result<Dialed> {
    let bind = if address.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let endpoint = Endpoint(client_endpoint(bind.parse()?)?);
    let config = TransportConfig::default();
    let pin = CertificateFingerprint::from_hex(&offer.certificate_pin)?;
    let conn = connect(&endpoint.0, address, "localhost", pin, ALPN_DIRECT, &config).await?;
    let initiator = Initiator::new(keys, &agent, &offer.prologue())?;
    let (session, incoming, greeting) =
        Session::initiate(conn.clone(), initiator, offer.id.as_bytes(), &config).await?;
    if greeting != b"direct-ready" || session.peer_static() != Some(agent) {
        session.close(0x20, b"wrong direct peer");
        anyhow::bail!("the direct peer did not prove the expected session identity");
    }
    Ok(Dialed {
        native: session,
        incoming,
        endpoint,
        connection: conn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndp_crypto::Responder;
    use ndp_proto::MsgHeader;
    use ndp_transport::{dev_credentials, server_endpoint};
    use tokio::task::JoinSet;

    struct Listener {
        endpoint: Endpoint,
        offer: DirectOffer,
        agent: StaticKeypair,
        client: StaticKeypair,
    }

    impl Listener {
        fn new() -> Self {
            Self::with_keys(StaticKeypair::generate(), StaticKeypair::generate())
        }

        fn with_keys(agent: StaticKeypair, client: StaticKeypair) -> Self {
            let credentials = dev_credentials(&[]).unwrap();
            let endpoint = Endpoint(
                server_endpoint(
                    "127.0.0.1:0".parse().unwrap(),
                    &credentials,
                    &[ALPN_DIRECT],
                    &TransportConfig::default(),
                )
                .unwrap(),
            );
            let offer = DirectOffer {
                version: 1,
                session: nebula_common::SessionId::new(),
                id: uuid::Uuid::now_v7(),
                addresses: vec![endpoint.0.local_addr().unwrap()],
                certificate_pin: credentials.fingerprint.to_hex(),
            };
            Self {
                endpoint,
                offer,
                agent,
                client,
            }
        }

        async fn accept(&self, greeting: &[u8]) -> anyhow::Result<(Session, SessionReceiver)> {
            let conn = self.endpoint.0.accept().await.unwrap().await?;
            let responder = Responder::new(&self.agent, &self.offer.prologue())?;
            Ok(Session::accept(
                conn,
                responder,
                |payload| {
                    if payload != self.offer.id.as_bytes() {
                        return Err("wrong listener binding".into());
                    }
                    Ok(greeting.to_vec())
                },
                &TransportConfig::default(),
            )
            .await?)
        }

        async fn handshake(
            &self,
            offer: &DirectOffer,
            agent: PublicKey,
            greeting: &[u8],
        ) -> (
            anyhow::Result<Dialed>,
            anyhow::Result<(Session, SessionReceiver)>,
        ) {
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::join!(
                    dial(offer, self.offer.addresses[0], &self.client, agent),
                    self.accept(greeting)
                )
            })
            .await
            .expect("direct TLS/Noise handshake did not finish")
        }

        async fn pair(&self) -> (Dialed, Session, SessionReceiver) {
            let (client, agent) = self
                .handshake(&self.offer, self.agent.public(), b"direct-ready")
                .await;
            let (agent, receiver) = agent.unwrap();
            (client.unwrap(), agent, receiver)
        }
    }

    #[tokio::test]
    async fn pinned_direct_dial_proves_original_identities_and_delivers_encrypted_data() {
        let listener = Listener::new();
        assert!(listener.offer.validate(listener.offer.session).is_err());
        let (mut client, agent, mut incoming) = listener.pair().await;
        assert_eq!(client.native.peer_static(), Some(listener.agent.public()));
        assert_eq!(agent.peer_static(), Some(listener.client.public()));
        client
            .native
            .send(
                Channel::Input,
                MsgHeader::new(MsgKind::InputBatch, 0, 0),
                b"direct input",
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), incoming.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .payload,
            b"direct input"
        );
        agent
            .send_video_frame(
                MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                b"direct video",
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), client.incoming.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .payload,
            b"direct video"
        );
    }

    #[tokio::test]
    async fn wrong_tls_pin_cannot_yield_an_attachable_session() {
        let listener = Listener::new();
        let mut offer = listener.offer.clone();
        offer.certificate_pin = dev_credentials(&[]).unwrap().fingerprint.to_hex();
        let (client, agent) = listener
            .handshake(&offer, listener.agent.public(), b"direct-ready")
            .await;
        assert!(client.is_err());
        assert!(agent.is_err());
    }

    #[tokio::test]
    async fn wrong_original_agent_key_cannot_yield_an_attachable_session() {
        let listener = Listener::new();
        let (client, agent) = listener
            .handshake(
                &listener.offer,
                StaticKeypair::generate().public(),
                b"direct-ready",
            )
            .await;
        assert!(client.is_err());
        assert!(agent.is_err());
    }

    #[tokio::test]
    async fn different_logical_session_or_listener_nonce_cannot_attach() {
        let listener = Listener::new();
        for change_session in [false, true] {
            let mut offer = listener.offer.clone();
            if change_session {
                offer.session = nebula_common::SessionId::new();
            } else {
                offer.id = uuid::Uuid::now_v7();
            }
            let (client, agent) = listener
                .handshake(&offer, listener.agent.public(), b"direct-ready")
                .await;
            assert!(client.is_err());
            assert!(agent.is_err());
        }
    }

    #[tokio::test]
    async fn wrong_direct_greeting_is_rejected() {
        let listener = Listener::new();
        let (client, _) = listener
            .handshake(&listener.offer, listener.agent.public(), b"agent-ready")
            .await;
        let Err(error) = client else {
            panic!("a non-direct greeting must not be attachable");
        };
        assert!(error.to_string().contains("expected session identity"));
    }

    #[tokio::test]
    async fn cancelled_dial_closes_its_endpoint_and_pending_noise_connection() {
        let listener = Listener::new();
        let mut tasks = JoinSet::new();
        let offer = listener.offer.clone();
        let keys = listener.client.clone();
        let agent = listener.agent.public();
        tasks.spawn(async move { dial(&offer, offer.addresses[0], &keys, agent).await });
        let connection = tokio::time::timeout(Duration::from_secs(1), async {
            listener.endpoint.0.accept().await.unwrap().await.unwrap()
        })
        .await
        .unwrap();
        tasks.abort_all();
        assert!(tasks
            .join_next()
            .await
            .unwrap()
            .err()
            .unwrap()
            .is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), connection.closed())
            .await
            .expect("aborting a dial must close the listener's accepted connection");
    }

    #[tokio::test]
    async fn authenticated_candidate_attaches_to_both_original_relay_peers() {
        let relay = Listener::new();
        let (client, agent, agent_rx) = relay.pair().await;
        let (client_session, _client_rx) = client.native.into_multipath(client.incoming);
        let (agent_session, _agent_rx) = agent.into_multipath(agent_rx);
        let mut direct = Listener::with_keys(relay.agent.clone(), relay.client.clone());
        direct.offer.session = relay.offer.session;
        let (candidate, agent, agent_rx) = direct.pair().await;
        client_session
            .attach_direct(candidate.native, candidate.incoming)
            .await
            .unwrap();
        agent_session.attach_direct(agent, agent_rx).await.unwrap();
        for session in [&client_session, &agent_session] {
            assert!(session
                .path_stats()
                .iter()
                .any(|path| path.kind == ndp_transport::PathKind::Direct));
        }
        assert!(candidate.connection.close_reason().is_none());
        let (duplicate, agent, agent_rx) = direct.pair().await;
        assert!(client_session
            .attach_direct(duplicate.native, duplicate.incoming)
            .await
            .is_err());
        assert!(agent_session.attach_direct(agent, agent_rx).await.is_err());
        assert!(candidate.connection.close_reason().is_none());
    }

    #[tokio::test]
    async fn connector_drop_closes_attached_candidate_without_revoking_relay() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let relay = Listener::new();
            let (client, agent, agent_rx) = relay.pair().await;
            let (client_session, _client_rx) = client.native.into_multipath(client.incoming);
            let (_agent_session, mut agent_rx) = agent.into_multipath(agent_rx);
            let direct = Listener::with_keys(relay.agent.clone(), relay.client.clone());
            let connector = Connector::spawn(
                client_session.clone(),
                relay.client.clone(),
                relay.agent.public(),
            );
            let handle = connector.task.abort_handle();
            connector.offer(direct.offer.clone());
            let conn = direct.endpoint.0.accept().await.unwrap().await.unwrap();
            let (_native, _incoming) = Session::accept(
                conn.clone(),
                Responder::new(&direct.agent, &direct.offer.prologue()).unwrap(),
                |payload| {
                    assert_eq!(payload, direct.offer.id.as_bytes());
                    Ok(b"direct-ready".to_vec())
                },
                &TransportConfig::default(),
            )
            .await
            .unwrap();
            while !client_session
                .path_stats()
                .iter()
                .any(|path| path.kind == ndp_transport::PathKind::Direct)
            {
                tokio::task::yield_now().await;
            }
            // A still-healthy standby must not be closed or dialed again.
            connector.offer(direct.offer.clone());
            while connector.offers.capacity() == 0 {
                tokio::task::yield_now().await;
            }
            assert!(conn.close_reason().is_none());
            drop(connector);
            conn.closed().await;
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
            client_session
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::Ping, 0, 0),
                    b"relay remains authorised",
                )
                .await
                .unwrap();
            let message = agent_rx.recv().await.unwrap().unwrap();
            assert_eq!(message.payload, b"relay remains authorised");
        })
        .await
        .expect("connector teardown must close its attached path, not the relay session");
    }

    #[tokio::test]
    async fn headless_receiver_filters_offers_and_keeps_media_live_during_bounded_probe() {
        tokio::time::timeout(Duration::from_secs(3), async {
            let relay = Listener::new();
            let (client, agent, agent_rx) = relay.pair().await;
            let (client_session, client_rx) = client.native.into_multipath(client.incoming);
            let (agent_session, _agent_rx) = agent.into_multipath(agent_rx);
            let mut incoming = ConnectedReceiver::new(
                client_rx,
                client_session,
                relay.client.clone(),
                relay.agent.public(),
                relay.offer.session,
                true,
            );
            let stalled = Listener::with_keys(relay.agent.clone(), relay.client.clone());
            let connector = incoming.direct.as_ref().unwrap();
            let handle = connector.task.abort_handle();
            // Inject a loopback candidate directly into the private worker;
            // public offer validation is still exercised below and unchanged.
            connector.offer(stalled.offer.clone());
            let pending = stalled.endpoint.0.accept().await.unwrap().await.unwrap();
            connector.offer(stalled.offer.clone());
            connector.offer(stalled.offer.clone());
            assert_eq!(connector.offers.capacity(), 0);
            let mut wrong_session = relay.offer.clone();
            wrong_session.session = nebula_common::SessionId::new();
            wrong_session.addresses = vec!["192.0.2.1:1234".parse().unwrap()];
            for payload in [b"malformed offer".to_vec(), wrong_session.encode().unwrap()] {
                agent_session
                    .send(
                        Channel::Control,
                        MsgHeader::new(MsgKind::PathCandidates, 0, 0),
                        &payload,
                    )
                    .await
                    .unwrap();
            }
            agent_session
                .send(
                    Channel::Control,
                    MsgHeader::new(MsgKind::Pong, 0, 0),
                    b"control during stalled probe",
                )
                .await
                .unwrap();
            let control = incoming.recv().await.unwrap().unwrap();
            assert_eq!(control.header.kind, MsgKind::Pong);
            assert_eq!(control.payload, b"control during stalled probe");
            agent_session
                .send_video_frame(
                    MsgHeader::new(MsgKind::VideoFrame, 0, 0),
                    b"video during stalled probe",
                    None,
                )
                .await
                .unwrap();
            let video = incoming.recv().await.unwrap().unwrap();
            assert_eq!(video.header.kind, MsgKind::VideoFrame);
            assert_eq!(video.payload, b"video during stalled probe");
            drop(incoming);
            pending.closed().await;
            while !handle.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a direct probe must not hold up media, control, or receiver teardown");
    }
}
