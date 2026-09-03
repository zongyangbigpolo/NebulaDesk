//! The NebulaDesk media relay.
//!
//! A relay does one thing: it lets a client and an agent that cannot reach
//! each other directly meet at a public address, and it copies bytes between
//! them. It performs no authorisation of its own — the gateway already did
//! that — and it can decrypt nothing, because the two peers run a Noise
//! handshake end to end across it.
//!
//! That narrowness is deliberate. The relay is the only component that must
//! scale with aggregate bandwidth, so it holds no database, no user state and
//! no durable session record. Losing one costs the sessions it was carrying
//! and nothing else.

#![warn(missing_docs)]

pub mod rendezvous;
pub mod splice;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ndp_signal::{PairKey, RelayHello, RelayHelloAck, TokenError};
use ndp_transport::{server_endpoint, ServerCredentials, TransportConfig, ALPN_RELAY};
use quinn::{Connection, Endpoint, VarInt};
use tracing::{debug, info, warn};

use crate::rendezvous::{Outcome, Rendezvous};

/// Relay configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// UDP address to listen on.
    pub listen: SocketAddr,
    /// The deployment-wide pairing secret, shared with every gateway.
    pub pair_secret: String,
    /// PEM certificate chain. Generated on the fly when absent.
    pub cert: Option<PathBuf>,
    /// PEM private key.
    pub key: Option<PathBuf>,
    /// Names to put in a generated certificate.
    pub subject_alt_names: Vec<String>,
    /// Refuse new sessions beyond this many concurrent connections.
    pub max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:7444".parse().expect("literal address"),
            pair_secret: String::new(),
            cert: None,
            key: None,
            subject_alt_names: Vec::new(),
            // Two connections per session, so this is a thousand sessions.
            max_connections: 2000,
        }
    }
}

/// A running relay.
pub struct Relay {
    endpoint: Endpoint,
    pair_key: PairKey,
    rendezvous: Arc<Rendezvous>,
    max_connections: usize,
    /// The certificate fingerprint peers must pin.
    pub fingerprint: String,
}

impl std::fmt::Debug for Relay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relay")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

impl Relay {
    /// Bind the relay's endpoint.
    pub fn bind(config: &Config) -> anyhow::Result<Self> {
        let pair_key = PairKey::new(config.pair_secret.as_bytes()).map_err(|_| {
            anyhow::anyhow!(
                "the pairing secret must be at least {} bytes; \
                 generate one with `nebula-relay gen-secret`",
                ndp_signal::pairing::MIN_SECRET_LEN
            )
        })?;
        let creds = ServerCredentials::load_or_generate(
            config.cert.as_deref(),
            config.key.as_deref(),
            &config.subject_alt_names,
        )?;
        let fingerprint = creds.fingerprint.to_hex();
        let endpoint = server_endpoint(
            config.listen,
            &creds,
            &[ALPN_RELAY],
            &TransportConfig::default(),
        )?;
        Ok(Self {
            endpoint,
            pair_key,
            rendezvous: Arc::new(Rendezvous::default()),
            max_connections: config.max_connections,
            fingerprint,
        })
    }

    /// The address actually bound, which matters when the port was zero.
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Accept connections until the endpoint is closed.
    pub async fn run(&self) {
        info!(
            addr = ?self.endpoint.local_addr().ok(),
            fingerprint = %self.fingerprint,
            "relay listening"
        );
        while let Some(incoming) = self.endpoint.accept().await {
            // Counted before the handshake completes, because the handshake
            // itself is what an attacker would try to flood.
            if self.endpoint.open_connections() > self.max_connections {
                warn!("at connection capacity; refusing");
                incoming.refuse();
                continue;
            }
            let pair_key = self.pair_key.clone();
            let rendezvous = self.rendezvous.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => handle(connection, pair_key, rendezvous).await,
                    Err(e) => debug!(error = %e, "handshake failed"),
                }
            });
        }
    }

    /// Stop accepting and close every live connection.
    pub fn shutdown(&self) {
        self.endpoint.close(VarInt::from_u32(0), b"shutting down");
    }
}

/// Authenticate one peer and hand it to the rendezvous table.
async fn handle(connection: Connection, pair_key: PairKey, rendezvous: Arc<Rendezvous>) {
    let peer = connection.remote_address();
    let (mut send, mut recv) = match connection.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            debug!(%peer, error = %e, "peer opened no control stream");
            return;
        }
    };

    let hello: RelayHello = match ndp_signal::read_message(&mut recv).await {
        Ok(hello) => hello,
        Err(e) => {
            debug!(%peer, error = %e, "unreadable relay hello");
            connection.close(VarInt::from_u32(0x01), b"bad hello");
            return;
        }
    };

    let token = match pair_key.verify(&hello.pair_token) {
        Ok(token) => token,
        Err(e) => {
            // Deliberately terse: an unauthenticated peer learns only that it
            // was refused, never whether the session exists.
            warn!(%peer, reason = %e, "rejected an unauthenticated peer");
            let _ = reject(&mut send, "unauthorised").await;
            connection.close(VarInt::from_u32(0x02), b"unauthorised");
            debug_assert!(matches!(
                e,
                TokenError::Malformed | TokenError::BadSignature | TokenError::Expired
            ));
            return;
        }
    };

    debug!(%peer, session = %token.session, side = ?token.side, "peer authenticated");
    let outcome = rendezvous
        .join(token.session, token.side, connection.clone())
        .await;

    match outcome {
        Outcome::Spliced => {
            if ndp_signal::write_message(&mut send, &RelayHelloAck::Spliced)
                .await
                .is_err()
            {
                return;
            }
            let _ = send.finish();
            // The pumps own the connection from here; this task's only
            // remaining job is to keep the acknowledgement stream alive.
            connection.closed().await;
        }
        other => {
            let reason = match other {
                Outcome::TimedOut => "counterpart did not arrive",
                Outcome::Duplicate => "session already has this side",
                _ => "peer went away",
            };
            let _ = reject(&mut send, reason).await;
            connection.close(VarInt::from_u32(0x03), reason.as_bytes());
        }
    }
}

async fn reject(send: &mut quinn::SendStream, reason: &str) -> Result<(), ndp_signal::SignalError> {
    ndp_signal::write_message(
        send,
        &RelayHelloAck::Rejected {
            reason: reason.to_string(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_relay_refuses_to_start_with_a_weak_pairing_secret() {
        // Starting anyway would let anyone who can guess the secret splice
        // themselves into a session.
        let config = Config {
            pair_secret: "short".into(),
            ..Config::default()
        };
        assert!(Relay::bind(&config).is_err());
    }

    #[tokio::test]
    async fn a_relay_binds_and_publishes_a_fingerprint() {
        let config = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            pair_secret: ndp_signal::generate_secret(),
            ..Config::default()
        };
        let relay = Relay::bind(&config).unwrap();
        assert_ne!(relay.local_addr().unwrap().port(), 0);
        assert_eq!(relay.fingerprint.len(), 64);
        relay.shutdown();
    }
}
