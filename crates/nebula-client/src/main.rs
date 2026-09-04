//! The workspace app's command line.
//!
//! Sign in, see what you may reach, connect to one of them. The resource is
//! named, never the machine — a user should not have to know, or care, which
//! computer is on the other end.

use clap::{Parser, Subcommand};
use nebula_client::{session, ManagerClient};

#[derive(Parser)]
#[command(
    name = "nebula-client",
    version,
    about = "Connect to a NebulaDesk resource"
)]
struct Cli {
    /// The manager to sign in to.
    #[arg(long, env = "NEBULA_MANAGER_URL")]
    manager_url: String,

    /// Tenant slug.
    #[arg(long, env = "NEBULA_TENANT")]
    tenant: String,

    /// Email address to sign in with.
    #[arg(long, env = "NEBULA_EMAIL")]
    email: String,

    /// Password. Prefer the prompt or the environment over the command line,
    /// where it would be visible to every other process on the machine.
    #[arg(long, env = "NEBULA_PASSWORD", hide_env_values = true)]
    password: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show everything this account may connect to.
    List,
    /// Open a resource in a window.
    Connect {
        /// The resource's id, or enough of its name to identify it.
        resource: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nebula_client=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let password = match cli.password {
        Some(password) => password,
        None => rpassword::prompt_password(format!("Password for {}: ", cli.email))?,
    };

    // The window has to own the main thread on macOS and Windows, so the
    // sign-in work runs on a runtime that is finished with before the event
    // loop starts.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let (manager, resources) = runtime.block_on(async {
        let manager =
            ManagerClient::login(&cli.manager_url, &cli.tenant, &cli.email, &password).await?;
        let resources = manager.resources().await?;
        anyhow::Ok((manager, resources))
    })?;

    match cli.command {
        Command::List => {
            if resources.is_empty() {
                println!("Nothing has been shared with this account yet.");
                return Ok(());
            }
            for resource in &resources {
                println!(
                    "{:<38}  {:<8}  {:<8}  {}",
                    resource.id, resource.kind, resource.machine_status, resource.name
                );
            }
            Ok(())
        }

        Command::Connect { resource } => {
            let chosen = pick(&resources, &resource)?;
            if !chosen.is_online() {
                anyhow::bail!(
                    "'{}' is {} — the machine serving it is not connected right now",
                    chosen.name,
                    chosen.machine_status.to_lowercase()
                );
            }
            let ticket = runtime.block_on(manager.open(&chosen.id))?;
            // The runtime is done; the session builds its own, because the
            // event loop is about to take this thread for good.
            drop(runtime);
            session::run(ticket, &chosen.name)
        }
    }
}

/// Resolve what the user typed to exactly one resource.
///
/// Ambiguity is an error rather than a guess: connecting to the wrong machine
/// is not something a user can be expected to notice straight away.
fn pick<'a>(
    resources: &'a [nebula_client::Resource],
    wanted: &str,
) -> anyhow::Result<&'a nebula_client::Resource> {
    if let Some(exact) = resources.iter().find(|r| r.id == wanted) {
        return Ok(exact);
    }
    let wanted_lower = wanted.to_lowercase();
    let matches: Vec<_> = resources
        .iter()
        .filter(|r| r.name.to_lowercase().contains(&wanted_lower))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => anyhow::bail!(
            "no resource here is called '{wanted}'. Run `nebula-client list` to see what there is"
        ),
        many => {
            let names: Vec<_> = many.iter().map(|r| r.name.as_str()).collect();
            anyhow::bail!(
                "'{wanted}' matches more than one resource ({}); use the id instead",
                names.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_client::Resource;

    fn resource(id: &str, name: &str) -> Resource {
        Resource {
            id: id.into(),
            name: name.into(),
            kind: "DESKTOP".into(),
            machine_status: "ONLINE".into(),
            role: None,
        }
    }

    #[test]
    fn an_id_wins_over_a_name_that_would_also_match() {
        // Otherwise a resource whose *name* contains another's id would be
        // able to shadow it.
        let all = vec![resource("abc", "Design"), resource("xyz", "abc studio")];
        assert_eq!(pick(&all, "abc").unwrap().id, "abc");
    }

    #[test]
    fn a_partial_name_is_enough_when_it_is_unambiguous() {
        let all = vec![resource("1", "Design workstation"), resource("2", "CAD")];
        assert_eq!(pick(&all, "design").unwrap().id, "1");
        assert_eq!(pick(&all, "CAD").unwrap().id, "2");
    }

    #[test]
    fn an_ambiguous_name_is_refused_rather_than_guessed() {
        let all = vec![resource("1", "Design one"), resource("2", "Design two")];
        let error = pick(&all, "design").unwrap_err().to_string();
        assert!(error.contains("more than one"), "{error}");
    }

    #[test]
    fn an_unknown_name_says_how_to_find_the_right_one() {
        let all = vec![resource("1", "Design")];
        let error = pick(&all, "nothing").unwrap_err().to_string();
        assert!(error.contains("list"), "{error}");
    }
}
