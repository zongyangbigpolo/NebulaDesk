//! Gateway configuration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Everything a gateway needs to start.
#[derive(Debug, Clone)]
pub struct Config {
    /// UDP address to listen on. One socket serves both agents and clients.
    pub listen: SocketAddr,

    /// The address peers should be told to dial, which differs from `listen`
    /// behind NAT or a load balancer. Empty means "whatever was bound", which
    /// is only useful for local development.
    pub advertised_addr: String,

    /// Unique node name. Re-registering under the same name rotates this
    /// gateway's credential in place rather than orphaning machines that
    /// already point at the old id.
    pub name: String,

    /// Base URL of the manager, e.g. `https://manager.example.com`.
    pub manager_url: String,

    /// The `iss` a ticket must carry, which is the manager's *public* URL.
    ///
    /// Separate from `manager_url` because the gateway usually reaches the
    /// manager over a private address while clients are issued tickets naming
    /// the public one. Conflating the two makes every ticket fail
    /// verification the moment a deployment stops being a single laptop.
    /// `None` means "the same as `manager_url`", which is right for local
    /// development and wrong almost everywhere else.
    pub ticket_issuer: Option<String>,

    /// Operator secret used once at startup to register with the manager.
    /// Omitted when `node_credential` is already provisioned.
    pub bootstrap_secret: Option<String>,

    /// This gateway's own credential, `<uuid>.<secret>`. Obtained from
    /// registration and persisted by the operator.
    pub node_credential: Option<String>,

    /// Deployment region, used by the manager to place relays near machines.
    pub region: String,

    /// Maximum concurrent sessions, advertised to the manager for placement.
    pub capacity: Option<i32>,

    /// The deployment-wide pairing secret, shared with every relay.
    pub pair_secret: String,

    /// Where to persist the QUIC certificate and key. The certificate pin is
    /// published to the manager and baked into tickets, so it must survive a
    /// restart or every client holding an old pin is locked out.
    pub cert_path: Option<PathBuf>,
    /// Private key path, alongside `cert_path`.
    pub key_path: Option<PathBuf>,

    /// How often agents are asked to send a heartbeat over the tunnel.
    pub heartbeat: Duration,

    /// How long to wait for an agent's tunnel handshake before hanging up.
    pub handshake_timeout: Duration,

    /// Maximum concurrent connections, counted from the moment a handshake
    /// starts.
    pub max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "[::]:4433".parse().expect("literal address"),
            advertised_addr: "127.0.0.1:4433".into(),
            name: "gateway-local".into(),
            manager_url: "http://127.0.0.1:8080".into(),
            ticket_issuer: None,
            bootstrap_secret: None,
            node_credential: None,
            region: "default".into(),
            capacity: None,
            pair_secret: String::new(),
            cert_path: None,
            key_path: None,
            heartbeat: Duration::from_secs(20),
            handshake_timeout: Duration::from_secs(10),
            max_connections: 8192,
        }
    }
}
