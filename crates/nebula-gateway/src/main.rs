//! The `nebula-gateway` executable.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use nebula_gateway::{Config, Gateway};

/// NebulaDesk edge gateway.
#[derive(Debug, Parser)]
#[command(name = "nebula-gateway", version, about)]
struct Cli {
    /// UDP address to listen on; one socket serves both agents and clients.
    #[arg(long, env = "NEBULA_GATEWAY_LISTEN", default_value = "[::]:4433")]
    listen: SocketAddr,

    /// The address peers are told to dial, if it differs from `--listen`.
    #[arg(long, env = "NEBULA_GATEWAY_ADVERTISE")]
    advertise: String,

    /// Unique node name; re-registering under it rotates this gateway's
    /// credential rather than orphaning the machines pointing at it.
    #[arg(long, env = "NEBULA_GATEWAY_NAME", default_value = "gateway-1")]
    name: String,

    /// Base URL of the manager.
    #[arg(long, env = "NEBULA_MANAGER_URL")]
    manager_url: String,

    /// The manager's public URL, as it appears in the `iss` of a ticket.
    /// Defaults to `--manager-url`, which is only right when the gateway
    /// reaches the manager at the same address clients do.
    #[arg(long, env = "NEBULA_TICKET_ISSUER")]
    ticket_issuer: Option<String>,

    /// Operator bootstrap secret, used once to register.
    #[arg(long, env = "NEBULA_BOOTSTRAP_SECRET")]
    bootstrap_secret: Option<String>,

    /// This gateway's credential, `<uuid>.<secret>`, from a prior
    /// registration.
    #[arg(long, env = "NEBULA_NODE_CREDENTIAL")]
    node_credential: Option<String>,

    /// Deployment region, used to place relays near machines.
    #[arg(long, env = "NEBULA_REGION", default_value = "default")]
    region: String,

    /// Maximum concurrent sessions to advertise.
    #[arg(long, env = "NEBULA_GATEWAY_CAPACITY")]
    capacity: Option<i32>,

    /// The deployment-wide pairing secret, shared with every relay.
    #[arg(long, env = "NEBULA_PAIR_SECRET")]
    pair_secret: String,

    /// Certificate path. The pin is baked into tickets, so it must survive a
    /// restart or every peer holding the old pin is locked out.
    #[arg(long, env = "NEBULA_GATEWAY_CERT")]
    cert: Option<PathBuf>,

    /// Private key path, alongside `--cert`.
    #[arg(long, env = "NEBULA_GATEWAY_KEY")]
    key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nebula_gateway=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let gateway = Gateway::start(Config {
        listen: cli.listen,
        advertised_addr: cli.advertise,
        name: cli.name,
        manager_url: cli.manager_url,
        ticket_issuer: cli.ticket_issuer,
        bootstrap_secret: cli.bootstrap_secret,
        node_credential: cli.node_credential,
        region: cli.region,
        capacity: cli.capacity,
        pair_secret: cli.pair_secret,
        cert_path: cli.cert,
        key_path: cli.key,
        ..Config::default()
    })
    .await?;

    let stopping = gateway.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            stopping.shutdown();
        }
    });

    gateway.run().await;
    Ok(())
}
