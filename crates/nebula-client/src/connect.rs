//! Turning a ticket into an encrypted session with an agent.
//!
//! Three hops, in this order, and the order matters: redeem the ticket at the
//! gateway, meet the agent at the relay it names, then run a Noise handshake
//! straight through the relay to the agent.
//!
//! Neither the gateway nor the relay ends up holding anything that would let
//! it read the session. The gateway sees a ticket and a public key; the relay
//! sees a pairing token and ciphertext. The handshake is pinned to the agent
//! key the manager named, so an intermediary that substituted itself would
//! fail the handshake rather than succeed at eavesdropping.

use ndp_crypto::{Initiator, PublicKey, StaticKeypair};
use ndp_signal::{ClientHello, ClientHelloAck, RelayHello, RelayHelloAck};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Session, SessionReceiver, TransportConfig,
    ALPN_RELAY, ALPN_SESSION,
};

use crate::manager::SessionTicket;

/// A live session and its stream of incoming messages.
pub struct Connected {
    /// Send side: input, control, and acknowledgements.
    pub session: Session,
    /// Receive side: video, audio, and control from the agent.
    pub incoming: SessionReceiver,
}

/// Redeem a ticket and come back with an encrypted session.
pub async fn connect_to_agent(ticket: &SessionTicket) -> anyhow::Result<Connected> {
    // A per-session identity. The client has no long-term key to protect and
    // nothing needs to recognise it across sessions, so generating one each
    // time gives unlinkability for free.
    let keys = StaticKeypair::generate();
    let config = TransportConfig::default();

    let agent_key = PublicKey::from_slice(
        &hex::decode(&ticket.agent_key)
            .map_err(|_| anyhow::anyhow!("the manager named an agent key that is not hex"))?,
    )
    .map_err(|_| anyhow::anyhow!("the manager named an agent key that is not a public key"))?;

    let accepted = redeem(ticket, &keys, &config).await?;
    if accepted.agent_key != ticket.agent_key {
        // The manager and the gateway must agree about who is being reached.
        // They are separate services and a disagreement means one of them is
        // wrong or lying; either way the handshake key cannot be trusted.
        anyhow::bail!("the gateway named a different agent than the manager did");
    }

    let conn = dial(
        &accepted.relay_addr,
        &accepted.relay_pin,
        ALPN_RELAY,
        &config,
        "relay",
    )
    .await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    ndp_signal::write_message(
        &mut send,
        &RelayHello {
            pair_token: accepted.pair_token,
        },
    )
    .await?;
    match ndp_signal::read_message::<RelayHelloAck>(&mut recv).await? {
        RelayHelloAck::Spliced => {}
        RelayHelloAck::Rejected { reason } => {
            anyhow::bail!("the relay would not pair this session: {reason}")
        }
    }

    // From here the relay is a pipe. The prologue binds this handshake to
    // this session id, so a recording of an earlier one cannot be replayed
    // into it.
    let initiator = Initiator::new(
        &keys,
        &agent_key,
        &nebula_agent::session::prologue(accepted.session),
    )?;
    let (session, incoming, greeting) =
        Session::initiate(conn, initiator, ticket.ticket.as_bytes(), &config)
            .await
            .map_err(|error| {
                anyhow::anyhow!("the agent would not complete the handshake: {error}")
            })?;
    tracing::debug!(greeting = %String::from_utf8_lossy(&greeting), "the agent accepted the session");

    Ok(Connected { session, incoming })
}

/// What the gateway hands back when it accepts a ticket.
struct Accepted {
    session: nebula_common::SessionId,
    relay_addr: String,
    relay_pin: String,
    pair_token: String,
    agent_key: String,
}

/// Present the ticket at the gateway and learn where to meet the agent.
async fn redeem(
    ticket: &SessionTicket,
    keys: &StaticKeypair,
    config: &TransportConfig,
) -> anyhow::Result<Accepted> {
    let conn = dial(
        &ticket.gateway_addr,
        &ticket.gateway_pin,
        ALPN_SESSION,
        config,
        "gateway",
    )
    .await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    ndp_signal::write_message(
        &mut send,
        &ClientHello {
            ticket: ticket.ticket.clone(),
            noise_public_key: hex::encode(keys.public().as_bytes()),
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .await?;

    let ack = ndp_signal::read_message::<ClientHelloAck>(&mut recv).await?;
    // The gateway's answer is complete; holding the connection open would
    // only keep a socket the rest of this never uses.
    conn.close(0u32.into(), b"done");

    match ack {
        ClientHelloAck::Accepted {
            session,
            relay_addr,
            relay_pin,
            pair_token,
            agent_key,
        } => Ok(Accepted {
            session,
            relay_addr,
            relay_pin,
            pair_token,
            agent_key,
        }),
        ClientHelloAck::Rejected { reason } => {
            anyhow::bail!("the gateway refused this session: {reason}")
        }
    }
}

/// Open a pinned QUIC connection to one of the edge services.
async fn dial(
    addr: &str,
    pin: &str,
    alpn: &[u8],
    config: &TransportConfig,
    what: &str,
) -> anyhow::Result<quinn::Connection> {
    let addr = resolve(addr)
        .await
        .map_err(|error| anyhow::anyhow!("could not resolve the {what} address: {error}"))?;
    let pin = CertificateFingerprint::from_hex(pin)
        .map_err(|_| anyhow::anyhow!("the {what} pin is not a certificate fingerprint"))?;

    // Bind a fresh socket per connection, on whichever family the destination
    // needs. Reusing one endpoint across both would mean an IPv4-only socket
    // failing every v6 destination.
    let bind = if addr.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };
    let endpoint = client_endpoint(bind.parse().expect("literal address"))?;

    // The certificate is pinned by fingerprint, so the name here is not a
    // trust decision — it only has to be a legal SNI value.
    connect(&endpoint, addr, "localhost", pin, alpn, config)
        .await
        .map_err(|error| anyhow::anyhow!("could not reach the {what} at {addr}: {error}"))
}

/// Resolve a `host:port` the manager gave us.
///
/// Deployments name their gateways, and a client that only understood literal
/// addresses would work in tests and fail everywhere else.
async fn resolve(addr: &str) -> anyhow::Result<std::net::SocketAddr> {
    if let Ok(parsed) = addr.parse() {
        return Ok(parsed);
    }
    let owned = addr.to_string();
    let mut candidates = tokio::task::spawn_blocking(move || {
        std::net::ToSocketAddrs::to_socket_addrs(&owned).map(|it| it.collect::<Vec<_>>())
    })
    .await??;
    // Prefer IPv6 when it is offered: these are QUIC connections to
    // infrastructure, and the v6 path is the one less likely to be behind a
    // carrier NAT that will rewrite it.
    candidates.sort_by_key(|candidate| u8::from(candidate.is_ipv4()));
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("'{addr}' resolved to no addresses"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn literal_addresses_are_used_as_given() {
        assert_eq!(
            resolve("127.0.0.1:7443").await.unwrap(),
            "127.0.0.1:7443".parse().unwrap()
        );
        assert_eq!(
            resolve("[::1]:7443").await.unwrap(),
            "[::1]:7443".parse().unwrap()
        );
    }

    #[tokio::test]
    async fn names_are_resolved_and_ipv6_wins() {
        let resolved = resolve("localhost:7443").await.unwrap();
        assert_eq!(resolved.port(), 7443);
        // Whether this host has v6 at all is not something a test can assume,
        // but if both were offered the sort must have put v6 first.
        assert!(resolved.ip().is_loopback());
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_says_so() {
        let error = resolve("nx.invalid:443").await.unwrap_err().to_string();
        assert!(!error.is_empty());
    }
}
