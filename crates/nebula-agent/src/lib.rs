//! NebulaDesk agent: the process that makes a machine reachable.
//!
//! It does two things forever. It keeps one outbound QUIC connection to a
//! gateway, which is what lets a client reach this machine without anyone
//! forwarding a port to it, and it serves the sessions that arrive down that
//! connection.
//!
//! Nothing here decides who may connect. That was settled by the manager
//! before this machine heard about the session at all; the agent's part is to
//! check that the peer really holds the key it was told to expect, and to
//! enforce the policy it was handed.

#![warn(missing_docs)]

pub mod identity;
pub mod manager;
pub mod media;
pub mod platform;
pub mod session;

use std::sync::Arc;
use std::time::Duration;

use ndp_crypto::StaticKeypair;
use ndp_signal::{AgentHello, AgentHelloAck, AgentMessage, GatewayMessage};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, TransportConfig, ALPN_AGENT_GATEWAY,
};
use tokio::sync::mpsc;

pub use identity::Identity;
pub use manager::ManagerClient;
pub use media::{Platform, TestPattern};
pub use platform::native;

/// The shortest and longest waits between reconnection attempts.
///
/// Backoff exists to protect the gateway, not the agent: a thousand machines
/// whose gateway restarts must not all retry in the same millisecond. The cap
/// is low enough that a machine is never unreachable for long after the
/// gateway comes back.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How many messages may be queued towards the gateway.
const TUNNEL_QUEUE: usize = 64;

/// A running agent.
pub struct Agent {
    identity: Identity,
    keys: StaticKeypair,
    manager: ManagerClient,
    platform: Arc<dyn Platform>,
}

impl Agent {
    /// Build an agent from a stored identity.
    pub fn new(identity: Identity, platform: Arc<dyn Platform>) -> anyhow::Result<Self> {
        let keys = identity.keypair()?;
        let manager = ManagerClient::new(&identity.manager_url, Some(identity.credential.clone()))?;
        Ok(Self {
            identity,
            keys,
            manager,
            platform,
        })
    }

    /// The machine's Noise public key, hex encoded.
    #[must_use]
    pub fn public_key(&self) -> String {
        hex::encode(self.keys.public().as_bytes())
    }

    /// Stay attached to a gateway for as long as the process lives.
    ///
    /// Every failure is treated the same way — wait, ask the manager where to
    /// go, and try again — because from the machine's point of view a gateway
    /// that has been redeployed and one that has crashed are the same event.
    pub async fn run(&self) -> ! {
        let mut backoff = BACKOFF_MIN;
        loop {
            match self.attach_once().await {
                Ok(()) => {
                    tracing::info!("the control tunnel closed; reconnecting");
                    backoff = BACKOFF_MIN;
                }
                Err(error) => {
                    tracing::warn!(%error, retry_in = ?backoff, "could not hold a control tunnel");
                }
            }
            tokio::time::sleep(jitter(backoff)).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }

    /// Attach to a gateway and serve until the tunnel ends.
    async fn attach_once(&self) -> anyhow::Result<()> {
        let assignment = self.manager.gateway().await?;
        let pin = CertificateFingerprint::from_hex(&assignment.cert_pin)
            .map_err(|_| anyhow::anyhow!("the manager gave a malformed gateway pin"))?;

        let config = TransportConfig::default();
        let endpoint = client_endpoint("0.0.0.0:0".parse().expect("literal address"))?;
        let (addr, server_name) = resolve(&assignment.quic_addr).await?;
        let conn = connect(
            &endpoint,
            addr,
            &server_name,
            pin,
            ALPN_AGENT_GATEWAY,
            &config,
        )
        .await?;

        let (mut send, mut recv) = conn.open_bi().await?;
        ndp_signal::write_message(
            &mut send,
            &AgentHello {
                machine_id: nebula_common::MachineId::from_uuid(self.identity.machine_id),
                credential: self.identity.credential.clone(),
                agent_version: env!("CARGO_PKG_VERSION").into(),
            },
        )
        .await?;

        let heartbeat = match ndp_signal::read_message::<AgentHelloAck>(&mut recv).await? {
            AgentHelloAck::Accepted { heartbeat_secs, .. } => Duration::from_secs(heartbeat_secs),
            AgentHelloAck::Rejected { reason } => {
                anyhow::bail!("the gateway refused this machine: {reason}")
            }
        };
        tracing::info!(gateway = %assignment.id, addr = %assignment.quic_addr, "control tunnel attached");

        let (outbound, mut queue) = mpsc::channel::<AgentMessage>(TUNNEL_QUEUE);

        // One writer, so a heartbeat can never interleave with a session
        // report halfway through a frame.
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

        let beats = outbound.clone();
        let beat_task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(heartbeat);
            loop {
                ticker.tick().await;
                if beats
                    .send(AgentMessage::Heartbeat {
                        active_sessions: Vec::new(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        let result = self.read_tunnel(&mut recv, &outbound).await;

        beat_task.abort();
        writer.abort();
        conn.close(0u32.into(), b"agent detaching");
        result
    }

    /// Handle everything the gateway pushes down the tunnel.
    async fn read_tunnel(
        &self,
        recv: &mut quinn::RecvStream,
        outbound: &mpsc::Sender<AgentMessage>,
    ) -> anyhow::Result<()> {
        loop {
            let message = match ndp_signal::read_message::<GatewayMessage>(recv).await {
                Ok(message) => message,
                Err(ndp_signal::SignalError::Read(_)) => return Ok(()),
                Err(error) => return Err(error.into()),
            };

            match message {
                GatewayMessage::StartSession(request) => {
                    // Each session runs on its own task: one that hangs must
                    // not stop this machine answering the next request.
                    let keys = self.keys.clone();
                    let platform = Arc::clone(&self.platform);
                    let reply = outbound.clone();
                    tokio::spawn(async move {
                        let session = request.session;
                        match session::serve(request, keys, platform).await {
                            Ok(tally) => {
                                let _ = reply
                                    .send(AgentMessage::SessionClosed {
                                        session,
                                        bytes_sent: tally.sent,
                                        bytes_received: tally.received,
                                    })
                                    .await;
                            }
                            Err(error) => {
                                tracing::warn!(%session, %error, "could not serve a session");
                                let _ = reply
                                    .send(AgentMessage::SessionFailed {
                                        session,
                                        // Deliberately vague: the client is
                                        // not owed the machine's diagnostics.
                                        reason: "the machine could not start the session".into(),
                                    })
                                    .await;
                            }
                        }
                    });
                }

                GatewayMessage::StopSession { session, reason } => {
                    tracing::info!(%session, %reason, "the gateway asked to stop a session");
                }

                GatewayMessage::Ping => {
                    let _ = outbound
                        .send(AgentMessage::Heartbeat {
                            active_sessions: Vec::new(),
                        })
                        .await;
                }
            }
        }
    }
}

/// Turn a configured `host:port` into an address to dial.
///
/// Gateways are addressed by name in any real deployment, because the whole
/// point of asking the manager where to go is that the answer changes. The
/// name is kept for the TLS handshake even though the certificate is pinned:
/// a pin proves which key, a name proves which service.
async fn resolve(target: &str) -> anyhow::Result<(std::net::SocketAddr, String)> {
    let (host, _) = target
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("the gateway address `{target}` has no port"))?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();

    let addr = tokio::net::lookup_host(target)
        .await
        .map_err(|e| anyhow::anyhow!("could not resolve the gateway address `{target}`: {e}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("the gateway address `{target}` resolved to nothing"))?;

    // An IP literal is not a name a certificate can carry, so fall back to
    // the name development certificates are issued for.
    let server_name = if host.parse::<std::net::IpAddr>().is_ok() {
        "localhost".to_string()
    } else {
        host
    };
    Ok((addr, server_name))
}

/// Spread reconnection attempts so a restarted gateway is not stampeded.
fn jitter(base: Duration) -> Duration {
    use rand::Rng;
    let millis = base.as_millis() as u64;
    Duration::from_millis(rand::thread_rng().gen_range(millis / 2..=millis))
}

/// Enrol this machine, writing an identity that later runs will load.
///
/// The Noise keypair is generated here and never leaves: the manager and
/// every client only ever see the public half.
pub async fn enroll(
    manager_url: &str,
    token: &str,
    name: &str,
    path: &std::path::Path,
) -> anyhow::Result<Identity> {
    if Identity::load(path)?.is_some() {
        anyhow::bail!(
            "this machine is already enrolled ({}); remove that file to enrol again",
            path.display()
        );
    }

    let keys = StaticKeypair::generate();
    let client = ManagerClient::new(manager_url, None)?;
    let enrolled = client
        .enroll(token, name, &hex::encode(keys.public().as_bytes()))
        .await?;

    let identity = Identity {
        machine_id: enrolled.machine_id,
        credential: enrolled.credential,
        noise_secret: hex::encode(keys.secret_bytes()),
        manager_url: manager_url.trim_end_matches('/').to_string(),
    };
    identity.save(path)?;
    Ok(identity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_name_survives_resolution_but_a_literal_does_not_become_one() {
        let (addr, name) = resolve("127.0.0.1:7443").await.unwrap();
        assert_eq!(addr.port(), 7443);
        assert_eq!(name, "localhost", "an IP is not a certificate name");

        let (_, name) = resolve("localhost:7443").await.unwrap();
        assert_eq!(name, "localhost");

        assert!(
            resolve("gateway.example").await.is_err(),
            "a port is required"
        );
    }

    #[test]
    fn backoff_jitter_never_exceeds_its_base() {
        for _ in 0..100 {
            let d = jitter(BACKOFF_MAX);
            assert!(d <= BACKOFF_MAX);
            assert!(d >= BACKOFF_MAX / 2);
        }
    }
}
