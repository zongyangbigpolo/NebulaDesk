//! NebulaDesk edge gateway.
//!
//! The gateway is the only component with a public address, and it carries no
//! media. Its job is to hold every agent's control tunnel open and, when a
//! client presents a valid ticket, introduce the two at a relay.
//!
//! # Why the agent dials the gateway
//!
//! A server machine sits behind whatever NAT its owner happens to have. If
//! the gateway had to reach *it*, deployment would mean port forwarding on
//! every desk in the building. Instead the agent opens one outbound QUIC
//! connection and keeps it, so the path already exists by the time anyone
//! wants to use it, and the machine never accepts an unsolicited packet.
//!
//! # What the gateway is not trusted with
//!
//! Nothing in a session is readable here. The client and the agent run a
//! Noise handshake end to end, keyed by a static public key the client learnt
//! from its ticket, so a compromised gateway can deny service or misdirect a
//! connection, but it cannot read or forge one frame of it.

#![warn(missing_docs)]

pub mod config;
pub mod manager;
pub mod tickets;
pub mod tunnels;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ndp_signal::{
    AgentHello, AgentHelloAck, AgentMessage, ClientHello, ClientHelloAck, GatewayMessage, PairKey,
    SessionRequest, Side,
};
use ndp_transport::{
    server_endpoint, ServerCredentials, TransportConfig, ALPN_AGENT_GATEWAY, ALPN_SESSION,
};
use nebula_common::MachineId;
use tokio::sync::mpsc;
use uuid::Uuid;

pub use config::Config;
pub use manager::{ManagerClient, NodeIdentity};
pub use tickets::{TicketError, TicketVerifier};
pub use tunnels::{Replaced, Tunnel, Tunnels};

/// How many messages may be queued for one agent before the gateway gives up
/// on it. An agent that will not drain its tunnel is not serving sessions
/// either, so buffering more only delays noticing.
const TUNNEL_QUEUE: usize = 64;

/// A running gateway.
pub struct Gateway {
    config: Config,
    endpoint: quinn::Endpoint,
    /// Hex SHA-256 of this gateway's certificate, published to the manager
    /// and pinned by everyone who dials it.
    pub fingerprint: String,
    /// This gateway's node id, as registered with the manager.
    pub node_id: Uuid,
    manager: ManagerClient,
    tickets: Arc<TicketVerifier>,
    tunnels: Arc<Tunnels>,
    pair: PairKey,
    transport: TransportConfig,
    connections: Arc<AtomicUsize>,
}

impl Gateway {
    /// Bind, register with the manager, and load its key set.
    ///
    /// All three happen before the gateway accepts anything, so a
    /// misconfiguration surfaces at startup instead of as a wave of refused
    /// clients later.
    pub async fn start(config: Config) -> anyhow::Result<Arc<Self>> {
        let pair = PairKey::new(config.pair_secret.as_bytes())
            .map_err(|e| anyhow::anyhow!("the pairing secret is unusable: {e}"))?;

        let credentials = ServerCredentials::load_or_generate(
            config.cert_path.as_deref(),
            config.key_path.as_deref(),
            &[],
        )?;
        let fingerprint = credentials.fingerprint.to_hex();

        let transport = TransportConfig::default();
        let endpoint = server_endpoint(
            config.listen,
            &credentials,
            &[ALPN_SESSION, ALPN_AGENT_GATEWAY],
            &transport,
        )?;

        // An operator who binds an ephemeral port cannot know the address to
        // advertise, so an empty setting means "whatever we actually bound".
        let advertised = if config.advertised_addr.is_empty() {
            endpoint.local_addr()?.to_string()
        } else {
            config.advertised_addr.clone()
        };

        let mut manager = ManagerClient::new(&config.manager_url, config.node_credential.clone())?;
        let node_id = match &config.bootstrap_secret {
            Some(secret) => {
                manager
                    .register(
                        secret,
                        &config.name,
                        &advertised,
                        &fingerprint,
                        &config.region,
                        config.capacity,
                    )
                    .await?
                    .id
            }
            None => {
                let credential = config.node_credential.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "either a bootstrap secret or a node credential must be configured"
                    )
                })?;
                credential
                    .split_once('.')
                    .and_then(|(id, _)| Uuid::parse_str(id).ok())
                    .ok_or_else(|| anyhow::anyhow!("the node credential is malformed"))?
            }
        };

        let tickets = TicketVerifier::new(
            ManagerClient::new(&config.manager_url, None)?,
            config.manager_url.trim_end_matches('/'),
        );
        tickets.prime().await?;
        tokio::spawn(Arc::clone(&tickets).refresh_forever());

        Ok(Arc::new(Self {
            config,
            endpoint,
            fingerprint,
            node_id,
            manager,
            tickets,
            tunnels: Tunnels::new(),
            pair,
            transport,
            connections: Arc::new(AtomicUsize::new(0)),
        }))
    }

    /// The address actually bound, which resolves an ephemeral port.
    pub fn local_addr(&self) -> anyhow::Result<std::net::SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// The agent tunnels currently attached.
    #[must_use]
    pub fn tunnels(&self) -> &Arc<Tunnels> {
        &self.tunnels
    }

    /// Serve until [`Gateway::shutdown`] is called.
    pub async fn run(self: &Arc<Self>) {
        tracing::info!(
            addr = %self.local_addr().map(|a| a.to_string()).unwrap_or_default(),
            node = %self.node_id,
            pin = %self.fingerprint,
            "gateway listening"
        );

        while let Some(incoming) = self.endpoint.accept().await {
            // Counted before the handshake completes, because handshakes are
            // what an attacker floods; counting after would let an unbounded
            // number of them run.
            if self.connections.load(Ordering::Relaxed) >= self.config.max_connections {
                incoming.refuse();
                continue;
            }
            self.connections.fetch_add(1, Ordering::Relaxed);

            let gateway = Arc::clone(self);
            tokio::spawn(async move {
                let connections = Arc::clone(&gateway.connections);
                let result = gateway.dispatch(incoming).await;
                connections.fetch_sub(1, Ordering::Relaxed);
                if let Err(error) = result {
                    tracing::debug!(%error, "connection ended");
                }
            });
        }
    }

    /// Stop accepting and close every live connection.
    pub fn shutdown(&self) {
        self.endpoint.close(0u32.into(), b"gateway shutting down");
    }

    /// Route a connection by the protocol it negotiated.
    ///
    /// One socket serves both agents and clients, distinguished by ALPN, so a
    /// deployment needs exactly one public port and one certificate.
    async fn dispatch(self: Arc<Self>, incoming: quinn::Incoming) -> anyhow::Result<()> {
        let conn = incoming.await?;
        let alpn = conn
            .handshake_data()
            .and_then(|data| data.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
            .and_then(|data| data.protocol)
            .unwrap_or_default();

        match alpn.as_slice() {
            ALPN_AGENT_GATEWAY => self.serve_agent(conn).await,
            ALPN_SESSION => self.serve_client(conn).await,
            other => {
                conn.close(0x10u32.into(), b"unsupported protocol");
                anyhow::bail!("unexpected ALPN {:?}", String::from_utf8_lossy(other))
            }
        }
    }

    /// Hold one agent's control tunnel for as long as it lasts.
    async fn serve_agent(self: Arc<Self>, conn: quinn::Connection) -> anyhow::Result<()> {
        let (mut send, mut recv) =
            tokio::time::timeout(self.config.handshake_timeout, conn.accept_bi()).await??;
        let hello: AgentHello = tokio::time::timeout(
            self.config.handshake_timeout,
            ndp_signal::read_message(&mut recv),
        )
        .await??;

        // The manager owns the answer to "is this really that machine", and
        // answering it also records the machine as attached here, so there is
        // no window in which a tunnelled machine is unplaceable.
        if let Err(error) = self
            .manager
            .verify_machine(hello.machine_id, &hello.credential, self.node_id)
            .await
        {
            tracing::info!(machine = %hello.machine_id, %error, "refused an agent tunnel");
            ndp_signal::write_message(
                &mut send,
                &AgentHelloAck::Rejected {
                    reason: "the machine credential was not accepted".into(),
                },
            )
            .await?;
            flush(&mut send).await;
            conn.close(0x11u32.into(), b"unauthorised");
            return Ok(());
        }

        ndp_signal::write_message(
            &mut send,
            &AgentHelloAck::Accepted {
                gateway_id: self.node_id,
                heartbeat_secs: self.config.heartbeat.as_secs(),
            },
        )
        .await?;

        let machine = hello.machine_id;
        let stable = conn.stable_id() as u64;
        let (outbound, mut queue) = mpsc::channel(TUNNEL_QUEUE);
        let credential = hello.credential.clone();

        if let Replaced::Older(old) = self
            .tunnels
            .insert(
                machine,
                Tunnel {
                    outbound,
                    connection: stable,
                    credential: credential.clone(),
                },
            )
            .await
        {
            tracing::info!(%machine, "an agent reconnected, displacing its previous tunnel");
            drop(old);
        }
        tracing::info!(%machine, version = %hello.agent_version, "agent tunnel attached");

        // One task writes, so nothing else ever touches the send stream and
        // messages cannot interleave halfway through a frame.
        let writer = tokio::spawn(async move {
            while let Some(message) = queue.recv().await {
                if ndp_signal::write_message(&mut send, &message)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let result = self.pump_agent(machine, &mut recv).await;
        writer.abort();

        if self.tunnels.remove(machine, stable).await.is_some() {
            // Telling the manager immediately is what keeps a machine from
            // being offered for the length of the liveness grace period after
            // its agent has plainly gone.
            if let Err(error) = self
                .manager
                .mark_machine_draining(machine, &credential)
                .await
            {
                tracing::warn!(%machine, %error, "could not mark the machine as draining");
            }
        }
        tracing::info!(%machine, "agent tunnel detached");
        result
    }

    /// Read an agent's reports until its tunnel ends.
    async fn pump_agent(
        &self,
        machine: MachineId,
        recv: &mut quinn::RecvStream,
    ) -> anyhow::Result<()> {
        loop {
            let message: AgentMessage = match ndp_signal::read_message(recv).await {
                Ok(message) => message,
                Err(ndp_signal::SignalError::Read(_)) => return Ok(()),
                Err(error) => return Err(error.into()),
            };

            match message {
                AgentMessage::Heartbeat { active_sessions } => {
                    tracing::trace!(%machine, sessions = active_sessions.len(), "agent heartbeat");
                }
                AgentMessage::SessionActive { session } => {
                    self.report(session.as_uuid(), "ACTIVE", None, None, None)
                        .await;
                }
                AgentMessage::SessionFailed { session, reason } => {
                    self.report(session.as_uuid(), "FAILED", Some(&reason), None, None)
                        .await;
                }
                AgentMessage::SessionClosed {
                    session,
                    bytes_sent,
                    bytes_received,
                } => {
                    // The agent's "sent" is the client's "down".
                    self.report(
                        session.as_uuid(),
                        "CLOSED",
                        None,
                        Some(bytes_received as i64),
                        Some(bytes_sent as i64),
                    )
                    .await;
                }
            }
        }
    }

    /// Redeem one client's ticket and introduce it to its agent.
    async fn serve_client(self: Arc<Self>, conn: quinn::Connection) -> anyhow::Result<()> {
        let (mut send, mut recv) =
            tokio::time::timeout(self.config.handshake_timeout, conn.accept_bi()).await??;
        let hello: ClientHello = tokio::time::timeout(
            self.config.handshake_timeout,
            ndp_signal::read_message(&mut recv),
        )
        .await??;

        let claims = match self.tickets.redeem(&hello.ticket).await {
            Ok(claims) => claims,
            Err(error) => {
                // Deliberately uniform: which of expired, forged and already
                // redeemed a ticket is tells an attacker where to push.
                tracing::info!(%error, "refused a client ticket");
                return reject(&mut send, &conn, "the ticket was not accepted").await;
            }
        };

        let Some(tunnel) = self.tunnels.get(claims.mid).await else {
            tracing::info!(machine = %claims.mid, session = %claims.sid, "no agent tunnel for the ticketed machine");
            self.report(
                claims.sid.as_uuid(),
                "FAILED",
                Some("machine_unreachable"),
                None,
                None,
            )
            .await;
            return reject(&mut send, &conn, "the machine is not reachable").await;
        };

        // Two halves of the same rendezvous. Each is worthless alone: the
        // relay will only splice a client token to an agent token.
        let client_token = self.pair.mint(claims.sid, Side::Client);
        let agent_token = self.pair.mint(claims.sid, Side::Agent);

        // The agent is told first so it is already dialling the relay while
        // the client is still reading its acknowledgement.
        let request = GatewayMessage::StartSession(SessionRequest {
            session: claims.sid,
            resource_id: claims.rid.as_uuid(),
            policy: claims.policy,
            role: claims.role,
            relay_addr: claims.relay_addr.clone(),
            relay_pin: claims.relay_pin.clone(),
            pair_token: agent_token,
            client_key: hello.noise_public_key.clone(),
        });
        if tunnel.outbound.send(request).await.is_err() {
            self.report(
                claims.sid.as_uuid(),
                "FAILED",
                Some("machine_unreachable"),
                None,
                None,
            )
            .await;
            return reject(&mut send, &conn, "the machine is not reachable").await;
        }

        ndp_signal::write_message(
            &mut send,
            &ClientHelloAck::Accepted {
                session: claims.sid,
                relay_addr: claims.relay_addr.clone(),
                relay_pin: claims.relay_pin.clone(),
                pair_token: client_token,
                agent_key: claims.agent_key.clone(),
            },
        )
        .await?;

        tracing::info!(
            session = %claims.sid,
            machine = %claims.mid,
            user = %claims.uid,
            "session brokered"
        );
        self.report(claims.sid.as_uuid(), "ACTIVE", None, None, None)
            .await;

        // The signalling connection stays up for the session's lifetime. It
        // costs nothing, and it is how the gateway learns the client is gone
        // without waiting for anyone to notice at the relay.
        conn.closed().await;

        let _ = tunnel
            .outbound
            .send(GatewayMessage::StopSession {
                session: claims.sid,
                reason: "the client disconnected".into(),
            })
            .await;
        self.report(
            claims.sid.as_uuid(),
            "CLOSED",
            Some("client_disconnected"),
            None,
            None,
        )
        .await;
        Ok(())
    }

    /// Tell the manager how a session is getting on, best effort.
    ///
    /// Accounting must never take a session down, so a manager that is slow
    /// or absent is logged and ignored.
    async fn report(
        &self,
        session: Uuid,
        state: &str,
        reason: Option<&str>,
        bytes_up: Option<i64>,
        bytes_down: Option<i64>,
    ) {
        if let Err(error) = self
            .manager
            .report_session(session, state, reason, bytes_up, bytes_down)
            .await
        {
            tracing::warn!(%session, %state, %error, "could not report the session");
        }
    }
}

/// How long to wait for a refusal to reach the peer before hanging up.
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Refuse a client without saying why.
async fn reject(
    send: &mut quinn::SendStream,
    conn: &quinn::Connection,
    reason: &str,
) -> anyhow::Result<()> {
    ndp_signal::write_message(
        send,
        &ClientHelloAck::Rejected {
            reason: reason.into(),
        },
    )
    .await?;
    flush(send).await;
    conn.close(0x12u32.into(), b"refused");
    Ok(())
}

/// Wait for a final message to be acknowledged before closing.
///
/// Closing a QUIC connection discards anything still in flight, so a refusal
/// written and immediately followed by `close` reaches the peer as a reset
/// rather than as a reason. Waiting for the acknowledgement is the difference
/// between a client that can report "the ticket was not accepted" and one
/// that can only report that the connection broke.
async fn flush(send: &mut quinn::SendStream) {
    let _ = send.finish();
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, send.stopped()).await;
}

impl Gateway {
    /// The transport settings this gateway serves with, exposed for tests.
    #[must_use]
    pub fn transport(&self) -> &TransportConfig {
        &self.transport
    }
}
