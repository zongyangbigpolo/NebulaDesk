//! The `nebula-agent` executable.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use nebula_agent::{enroll, Agent, Identity, TestPattern};

/// NebulaDesk server machine agent.
#[derive(Debug, Parser)]
#[command(name = "nebula-agent", version, about)]
struct Cli {
    /// Where the machine's identity is stored.
    #[arg(long, env = "NEBULA_AGENT_STATE", global = true)]
    state: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Enrol this machine with a manager, using a one-time token.
    Enroll {
        /// Base URL of the manager.
        #[arg(long, env = "NEBULA_MANAGER_URL")]
        manager_url: String,
        /// The enrolment token issued by an administrator.
        #[arg(long, env = "NEBULA_ENROLLMENT_TOKEN")]
        token: String,
        /// The name to register under; must match the token if it pinned one.
        #[arg(long)]
        name: String,
    },
    /// Attach to a gateway and serve sessions.
    Run,
    /// Print this machine's identity, without its secrets.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nebula_agent=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let path = cli.state.unwrap_or_else(Identity::default_path);

    match cli.command {
        Command::Enroll {
            manager_url,
            token,
            name,
        } => {
            let identity = enroll(&manager_url, &token, &name, &path).await?;
            println!("enrolled as {}", identity.machine_id);
            println!("identity written to {}", path.display());
            Ok(())
        }

        Command::Run => {
            let identity = Identity::load(&path)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "this machine is not enrolled; run `nebula-agent enroll` first ({})",
                    path.display()
                )
            })?;
            let agent = Agent::new(identity, Arc::new(TestPattern))?;
            tracing::info!(key = %agent.public_key(), "agent starting");
            agent.run().await
        }

        Command::Status => {
            match Identity::load(&path)? {
                Some(identity) => {
                    let agent = Agent::new(identity.clone(), Arc::new(TestPattern))?;
                    println!("machine    {}", identity.machine_id);
                    println!("manager    {}", identity.manager_url);
                    println!("noise key  {}", agent.public_key());
                }
                None => println!("not enrolled ({})", path.display()),
            }
            Ok(())
        }
    }
}
