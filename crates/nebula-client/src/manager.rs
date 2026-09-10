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
#[derive(Clone, Deserialize)]
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
    /// Non-sensitive expected-mode hint. Missing means legacy desktop only.
    #[serde(default)]
    pub application_windows: bool,
}

impl std::fmt::Debug for SessionTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionTicket")
            .field("session_id", &self.session_id)
            .field("policy", &self.policy)
            .field("application_windows", &self.application_windows)
            .finish_non_exhaustive()
    }
}

impl TryFrom<nebula_desktop_protocol::LaunchTicket> for SessionTicket {
    type Error = anyhow::Error;

    fn try_from(ticket: nebula_desktop_protocol::LaunchTicket) -> anyhow::Result<Self> {
        Ok(Self {
            session_id: ticket
                .session_id
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid session id"))?,
            ticket: ticket.ticket,
            gateway_addr: ticket.gateway_addr,
            gateway_pin: ticket.gateway_pin,
            agent_key: ticket.agent_key,
            policy: nebula_common::SessionPolicy {
                input: ticket.policy.input,
                audio: ticket.policy.audio,
                clipboard: ticket.policy.clipboard,
                file_transfer: ticket.policy.file_transfer,
            },
            application_windows: false,
        })
    }
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
    application_windows: bool,
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
            .query(&[("application_windows", true)])
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
                application_windows: true,
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
/// Diagnostic metadata, not permission to globally remap keyboard modifiers.
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
    fn manager_mode_hint_defaults_to_desktop_without_copying_launch_configuration() {
        let mut value = serde_json::json!({
            "session_id": uuid::Uuid::nil(), "ticket": "opaque-bearer",
            "gateway_addr": "localhost:443", "gateway_pin": "pin", "agent_key": "key"
        });
        let desktop: SessionTicket = serde_json::from_value(value.clone()).unwrap();
        assert!(!desktop.application_windows);
        value["application_windows"] = serde_json::json!(true);
        let application: SessionTicket = serde_json::from_value(value).unwrap();
        assert!(application.application_windows);
        assert!(!format!("{application:?}").contains("opaque-bearer"));
    }

    #[tokio::test]
    async fn resource_listing_advertises_native_application_support() {
        async fn resources(
            axum::extract::Query(query): axum::extract::Query<
                std::collections::HashMap<String, String>,
            >,
        ) -> axum::Json<Vec<serde_json::Value>> {
            assert_eq!(
                query.get("application_windows").map(String::as_str),
                Some("true")
            );
            axum::Json(Vec::new())
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/v1/resources", axum::routing::get(resources)),
            )
            .await
            .unwrap();
        });
        let client = ManagerClient {
            http: reqwest::Client::new(),
            base: format!("http://{address}"),
            token: "test-only".into(),
        };
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), client.resources()).await;
        server.abort();
        assert!(result.unwrap().unwrap().is_empty());
    }

    #[test]
    fn launch_ticket_preserves_authorization_and_redacts_debug() {
        let ticket = SessionTicket::try_from(nebula_desktop_protocol::LaunchTicket {
            session_id: uuid::Uuid::nil().to_string(),
            ticket: "SECRET_BEARER".into(),
            gateway_addr: "localhost:1".into(),
            gateway_pin: "pin".into(),
            agent_key: "key".into(),
            policy: nebula_desktop_protocol::Policy {
                input: false,
                audio: true,
                clipboard: false,
                file_transfer: true,
            },
        })
        .unwrap();
        assert_eq!(ticket.ticket, "SECRET_BEARER");
        assert!(ticket.policy.audio && ticket.policy.file_transfer);
        assert!(!ticket.policy.input && !ticket.policy.clipboard);
        assert!(!format!("{ticket:?}").contains("SECRET_BEARER"));
    }

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
