//! The agent's calls to the manager.
//!
//! Deliberately few: enrol once, ask which gateway to attach to, and send a
//! heartbeat. Everything about a session arrives down the control tunnel
//! instead, so a manager outage cannot stop a running session or even, for
//! the length of a gateway's cached key set, stop new ones.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Where the agent should attach its control tunnel.
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayAssignment {
    /// The gateway's node id.
    pub id: Uuid,
    /// QUIC address to dial.
    pub quic_addr: String,
    /// Certificate pin, hex SHA-256 of the DER certificate.
    pub cert_pin: String,
}

/// The manager's answer to a successful enrolment.
#[derive(Debug, Clone, Deserialize)]
pub struct Enrolled {
    /// The new machine's id.
    pub machine_id: Uuid,
    /// The credential to keep, shown exactly once.
    pub credential: String,
}

#[derive(Debug, Serialize)]
struct EnrollRequest<'a> {
    token: &'a str,
    name: &'a str,
    os: &'a str,
    os_version: &'a str,
    arch: &'a str,
    agent_version: &'a str,
    noise_public_key: &'a str,
}

/// A thin client for the manager's agent-facing endpoints.
#[derive(Debug, Clone)]
pub struct ManagerClient {
    http: reqwest::Client,
    base: String,
    credential: Option<String>,
}

impl ManagerClient {
    /// Build a client for the manager at `base`.
    pub fn new(base: &str, credential: Option<String>) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
            base: base.trim_end_matches('/').to_string(),
            credential,
        })
    }

    /// Redeem an enrolment token, registering this machine.
    #[allow(clippy::too_many_arguments)]
    pub async fn enroll(
        &self,
        token: &str,
        name: &str,
        noise_public_key: &str,
    ) -> anyhow::Result<Enrolled> {
        let response = self
            .http
            .post(format!("{}/v1/machines/enroll", self.base))
            .json(&EnrollRequest {
                token,
                name,
                os: host_os(),
                os_version: &host_os_version(),
                arch: std::env::consts::ARCH,
                agent_version: env!("CARGO_PKG_VERSION"),
                noise_public_key,
            })
            .send()
            .await?;
        parse(response).await
    }

    /// Ask which gateway to attach to.
    pub async fn gateway(&self) -> anyhow::Result<GatewayAssignment> {
        let response = self
            .http
            .get(format!("{}/v1/machines/self/gateway", self.base))
            .header("Authorization", format!("Machine {}", self.credential()?))
            .send()
            .await?;
        parse(response).await
    }

    /// Report liveness and, optionally, which gateway now holds the tunnel.
    pub async fn heartbeat(&self, status: &str, gateway: Option<Uuid>) -> anyhow::Result<()> {
        let response = self
            .http
            .post(format!("{}/v1/machines/heartbeat", self.base))
            .header("Authorization", format!("Machine {}", self.credential()?))
            .json(&serde_json::json!({ "status": status, "gateway_id": gateway }))
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(fail(response).await)
    }

    fn credential(&self) -> anyhow::Result<&str> {
        self.credential
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("this machine has not been enrolled"))
    }
}

async fn parse<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    if !response.status().is_success() {
        return Err(fail(response).await);
    }
    Ok(response.json().await?)
}

async fn fail(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let mut body = response.text().await.unwrap_or_default();
    body.truncate(512);
    anyhow::anyhow!("the manager returned {status}: {body}")
}

/// The manager's name for this platform.
fn host_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "MACOS",
        "windows" => "WINDOWS",
        _ => "LINUX",
    }
}

fn host_os_version() -> String {
    std::env::var("NEBULA_OS_VERSION").unwrap_or_else(|_| "unknown".into())
}
