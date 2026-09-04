//! Talking to the manager.
//!
//! Everything a client knows about the world comes through here: who it is,
//! what it may reach, and how to reach one of them. Notably absent is any
//! notion of a machine. A user picks "Design workstation" or "CAD", and the
//! manager decides what serves it — that indirection is the point of the
//! product, so this module never learns a machine id and never asks for one.

use serde::{Deserialize, Serialize};

/// A signed-in session against one tenant.
#[derive(Debug, Clone)]
pub struct ManagerClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// Something the signed-in user may connect to.
#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    /// Stable identifier, used to open a session.
    pub id: String,
    /// What the user sees in a list.
    pub name: String,
    /// `DESKTOP` or `APP`.
    pub kind: String,
    /// Whether the machine behind it is reachable right now.
    pub machine_status: String,
    /// What this user is allowed to do with it.
    #[serde(default)]
    pub role: Option<String>,
}

impl Resource {
    /// Whether opening this resource can be expected to work.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.machine_status == "ONLINE"
    }
}

/// Everything needed to reach an agent, valid for about a minute.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionTicket {
    /// The session this ticket opens.
    pub session_id: uuid::Uuid,
    /// The signed ticket, presented to the gateway and then to the agent.
    pub ticket: String,
    /// Where to redeem it.
    pub gateway_addr: String,
    /// The gateway's certificate fingerprint.
    pub gateway_pin: String,
    /// The agent's long-term Noise public key, as hex.
    ///
    /// This is the one field that makes the rest of the path untrusted: the
    /// handshake can only complete with the holder of the matching secret, so
    /// a gateway or relay that lied about anything else still cannot read or
    /// alter a byte of the session.
    pub agent_key: String,
    /// What this session is permitted to do.
    ///
    /// Advisory: the ticket carries the authoritative copy and the agent is
    /// what enforces it. This is here so the client does not offer a feature
    /// that would be silently refused.
    #[serde(default = "nebula_common::SessionPolicy::view_only")]
    pub policy: nebula_common::SessionPolicy,
}

#[derive(Serialize)]
struct Login<'a> {
    tenant: &'a str,
    email: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
}

#[derive(Serialize)]
struct OpenSession<'a> {
    resource_id: &'a str,
    client_os: &'a str,
}

impl ManagerClient {
    /// Sign in and keep the resulting access token.
    pub async fn login(
        manager_url: &str,
        tenant: &str,
        email: &str,
        password: &str,
    ) -> anyhow::Result<Self> {
        let base = manager_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .user_agent(concat!("nebula-client/", env!("CARGO_PKG_VERSION")))
            .build()?;

        let response = http
            .post(format!("{base}/v1/auth/login"))
            .json(&Login {
                tenant,
                email,
                password,
            })
            .send()
            .await?;
        // Failing to sign in is the one error an ordinary user will meet, so
        // it says what happened rather than quoting a status code.
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            anyhow::bail!("that email and password were not accepted for tenant '{tenant}'");
        }
        let token: Token = read(response, "sign in").await?;

        Ok(Self {
            http,
            base,
            token: token.access_token,
        })
    }

    /// Everything this user may connect to.
    pub async fn resources(&self) -> anyhow::Result<Vec<Resource>> {
        let response = self
            .http
            .get(format!("{}/v1/resources", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?;
        read(response, "list resources").await
    }

    /// Ask for a ticket to one resource.
    pub async fn open(&self, resource_id: &str) -> anyhow::Result<SessionTicket> {
        let response = self
            .http
            .post(format!("{}/v1/sessions", self.base))
            .bearer_auth(&self.token)
            .json(&OpenSession {
                resource_id,
                client_os: client_os(),
            })
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            anyhow::bail!(
                "that resource is not reachable right now; the machine serving it is offline"
            );
        }
        read(response, "open a session").await
    }
}

/// Decode a response, or explain what failed in terms of what was attempted.
async fn read<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    doing: &str,
) -> anyhow::Result<T> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // The manager reports its own errors as `{"error": "..."}`; use that
        // when it is there and fall back to the raw body when it is not.
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or(body);
        anyhow::bail!("could not {doing}: {status}: {detail}");
    }
    serde_json::from_str(&body)
        .map_err(|error| anyhow::anyhow!("could not {doing}: unexpected reply: {error}"))
}

/// What to tell the manager this client is running on.
///
/// The agent uses it to decide keyboard conventions — a Mac client sends
/// Command where a Windows client sends Control, and only the far side can
/// sensibly translate that.
const fn client_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "MACOS"
    } else if cfg!(target_os = "windows") {
        "WINDOWS"
    } else {
        "LINUX"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_is_only_offered_when_its_machine_is_up() {
        let resource = |status: &str| Resource {
            id: "r".into(),
            name: "Desktop".into(),
            kind: "DESKTOP".into(),
            machine_status: status.into(),
            role: None,
        };
        assert!(resource("ONLINE").is_online());
        assert!(!resource("OFFLINE").is_online());
        // An unknown status must not read as available: a client that offers
        // a dead resource wastes a round trip and looks broken.
        assert!(!resource("DRAINING").is_online());
    }

    #[test]
    fn the_client_reports_the_platform_it_was_built_for() {
        assert!(matches!(client_os(), "MACOS" | "WINDOWS" | "LINUX"));
    }
}
