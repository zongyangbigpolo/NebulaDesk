//! Relay entry point.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use nebula_relay::{Config, Relay};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "nebula-relay", about = "NebulaDesk media relay")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// UDP address to listen on.
    #[arg(long, env = "NEBULA_RELAY_LISTEN", default_value = "0.0.0.0:7444")]
    listen: SocketAddr,

    /// Pairing secret shared with every gateway in the deployment.
    #[arg(long, env = "NEBULA_PAIR_SECRET")]
    pair_secret: Option<String>,

    /// PEM certificate chain. A development certificate is generated if absent.
    #[arg(long, env = "NEBULA_RELAY_CERT")]
    cert: Option<PathBuf>,

    /// PEM private key.
    #[arg(long, env = "NEBULA_RELAY_KEY")]
    key: Option<PathBuf>,

    /// Names to place in a generated certificate.
    #[arg(long = "san")]
    subject_alt_names: Vec<String>,

    /// Maximum concurrent connections; two per session.
    #[arg(long, env = "NEBULA_RELAY_MAX_CONNECTIONS", default_value_t = 2000)]
    max_connections: usize,

    /// Manager base URL. Without it this relay never reports liveness, and
    /// the manager will stop placing sessions on it.
    #[arg(long, env = "NEBULA_MANAGER_URL")]
    manager_url: Option<String>,

    /// This relay's node credential, issued when it was registered.
    #[arg(long, env = "NEBULA_NODE_CREDENTIAL")]
    node_credential: Option<String>,

    /// Operator bootstrap secret, used once to register this relay.
    ///
    /// Registration has to happen after binding, because the manager needs
    /// the address and certificate pin this process ended up with.
    #[arg(long, env = "NEBULA_BOOTSTRAP_SECRET")]
    bootstrap_secret: Option<String>,

    /// Unique node name; re-registering under it rotates this relay's
    /// credential rather than creating a second entry.
    #[arg(long, env = "NEBULA_RELAY_NAME", default_value = "relay-1")]
    name: String,

    /// The address peers are told to dial, if it differs from `--listen`.
    #[arg(long, env = "NEBULA_RELAY_ADVERTISE", default_value = "")]
    advertise: String,

    /// Deployment region, used to place sessions near the machines serving
    /// them.
    #[arg(long, env = "NEBULA_REGION", default_value = "default")]
    region: String,
}

#[derive(Subcommand)]
enum Command {
    /// Print a fresh pairing secret to configure on gateways and relays.
    GenSecret,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if let Some(Command::GenSecret) = cli.command {
        println!("{}", ndp_signal_secret());
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("NEBULA_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let pair_secret = cli.pair_secret.ok_or_else(|| {
        anyhow::anyhow!("NEBULA_PAIR_SECRET must be set; generate one with `gen-secret`")
    })?;

    let heartbeat = Config::default().heartbeat;
    let relay = Relay::bind(&Config {
        listen: cli.listen,
        pair_secret,
        cert: cli.cert,
        key: cli.key,
        subject_alt_names: cli.subject_alt_names,
        max_connections: cli.max_connections,
        manager_url: cli.manager_url.clone(),
        node_credential: cli.node_credential.clone(),
        ..Config::default()
    })?;

    let relay = match (
        &cli.bootstrap_secret,
        &cli.node_credential,
        &cli.manager_url,
    ) {
        (Some(secret), _, Some(manager_url)) => {
            relay
                .register(
                    manager_url,
                    secret,
                    &cli.name,
                    &cli.advertise,
                    &cli.region,
                    heartbeat,
                )
                .await?
        }
        // Already registered, or deliberately standalone. `bind` will have
        // set liveness up from the credential if one was given.
        _ => {
            if cli.manager_url.is_some() && cli.node_credential.is_none() {
                anyhow::bail!(
                    "a manager URL was given with no way to authenticate to it; \
                     pass --bootstrap-secret to register this relay, or \
                     --node-credential if it is already registered"
                );
            }
            relay
        }
    };

    tokio::select! {
        () = relay.run() => {},
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            relay.shutdown();
        }
    }
    Ok(())
}

fn ndp_signal_secret() -> String {
    ndp_signal::generate_secret()
}
