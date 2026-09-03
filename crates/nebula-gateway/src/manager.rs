//! The gateway's client for the manager's control-plane API.
//!
//! Three things happen over HTTP, and nothing else does. Ticket verification
//! is deliberately absent: it is done offline against a cached key set, so a
//! manager outage stops new *authorisation* but not new *connections* against
//! tickets already issued, and never adds a round trip to the hot path.

use std::time::Duration;

use nebula_common::MachineId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A failure talking to the manager.
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    /// The request could not be made or the response could not be read.
    #[error("manager request failed: {0}")]
    Transport(#[from] reqwest::Error),

    /// The manager rejected the credential.
    #[error("manager rejected the credential")]
    Unauthorized,

    /// The manager answered, but not with success.
    #[error("manager returned {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body, truncated.
        body: String,
    },

    /// This gateway has no credential configured or issued.
    #[error("no node credential is configured")]
    NoCredential,
}

/// Result alias for manager calls.
pub type Result<T> = std::result::Result<T, ManagerError>;

/// A talking-to-the-manager handle.
#[derive(Debug, Clone)]
pub struct ManagerClient {
    http: reqwest::Client,
    base: String,
    credential: Option<String>,
}

#[derive(Debug, Serialize)]
struct RegisterGateway<'a> {
    name: &'a str,
    public_url: &'a str,
    quic_addr: &'a str,
    cert_pin: &'a str,
    region: &'a str,
    capacity: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct NodeRegistered {
    id: Uuid,
    credential: String,
}

/// How this gateway is identified to the manager after registration.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    /// The gateway's node id, reported by agents in their heartbeats.
    pub id: Uuid,
    /// The credential used to authenticate subsequent calls.
    pub credential: String,
}

#[derive(Debug, Serialize)]
struct Heartbeat<'a> {
    status: &'a str,
    gateway_id: Uuid,
}

#[derive(Debug, Serialize)]
struct SessionReport<'a> {
    state: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_up: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_down: Option<i64>,
}

impl ManagerClient {
    /// Build a client for the manager at `base`.
    pub fn new(base: &str, credential: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            credential,
        })
    }

    /// The credential currently in use, if any.
    #[must_use]
    pub fn credential(&self) -> Option<&str> {
        self.credential.as_deref()
    }

    /// Register this gateway, or rotate its credential if the name is known.
    ///
    /// Registration is idempotent by name so that a redeployed gateway keeps
    /// its node id; agents and sessions reference that id, and a fresh row
    /// would strand every one of them.
    pub async fn register(
        &mut self,
        bootstrap_secret: &str,
        name: &str,
        advertised_addr: &str,
        cert_pin: &str,
        region: &str,
        capacity: Option<i32>,
    ) -> Result<NodeIdentity> {
        let body = RegisterGateway {
            name,
            public_url: &format!("https://{advertised_addr}"),
            quic_addr: advertised_addr,
            cert_pin,
            region,
            capacity,
        };
        let response = self
            .http
            .post(format!("{}/v1/gateways", self.base))
            .header("Authorization", format!("Bearer {bootstrap_secret}"))
            .json(&body)
            .send()
            .await?;
        let registered: NodeRegistered = parse(response).await?;
        self.credential = Some(registered.credential.clone());
        Ok(NodeIdentity {
            id: registered.id,
            credential: registered.credential,
        })
    }

    /// Prove a machine credential is genuine, and mark the machine as
    /// attached to this gateway.
    ///
    /// The heartbeat endpoint authenticates with the machine's own
    /// credential, which makes it exactly the check wanted here: it succeeds
    /// only for the machine that owns the secret, and it cannot be used to
    /// assert anything about any other machine. Verifying and recording
    /// attachment in one call also removes a window where a machine is
    /// tunnelled but not yet placeable.
    pub async fn verify_machine(
        &self,
        machine: MachineId,
        credential: &str,
        gateway_id: Uuid,
    ) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/v1/machines/heartbeat", self.base))
            .header("Authorization", format!("Machine {credential}"))
            .json(&Heartbeat {
                status: "ONLINE",
                gateway_id,
            })
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ManagerError::Unauthorized);
        }
        expect_success(response, machine).await
    }

    /// Report that a machine's tunnel has gone, so its resources stop being
    /// offered before the liveness grace period would notice.
    pub async fn mark_machine_draining(&self, machine: MachineId, credential: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/v1/machines/heartbeat", self.base))
            .header("Authorization", format!("Machine {credential}"))
            .json(&serde_json::json!({ "status": "DRAINING" }))
            .send()
            .await?;
        expect_success(response, machine).await
    }

    /// Report a session's progress.
    pub async fn report_session(
        &self,
        session: Uuid,
        state: &str,
        reason: Option<&str>,
        bytes_up: Option<i64>,
        bytes_down: Option<i64>,
    ) -> Result<()> {
        let credential = self
            .credential
            .as_deref()
            .ok_or(ManagerError::NoCredential)?;
        let response = self
            .http
            .post(format!("{}/v1/sessions/{session}/report", self.base))
            .header("Authorization", format!("Node {credential}"))
            .json(&SessionReport {
                state,
                reason,
                bytes_up,
                bytes_down,
            })
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(status_error(response).await)
    }

    /// Fetch the manager's published key set.
    pub async fn jwks(&self) -> Result<nebula_common::Jwks> {
        let response = self
            .http
            .get(format!("{}/.well-known/jwks.json", self.base))
            .send()
            .await?;
        parse(response).await
    }
}

async fn expect_success(response: reqwest::Response, machine: MachineId) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    let err = status_error(response).await;
    tracing::debug!(%machine, error = %err, "manager call failed");
    Err(err)
}

async fn parse<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    if !response.status().is_success() {
        return Err(status_error(response).await);
    }
    Ok(response.json().await?)
}

async fn status_error(response: reqwest::Response) -> ManagerError {
    let status = response.status().as_u16();
    if status == 401 {
        return ManagerError::Unauthorized;
    }
    let mut body = response.text().await.unwrap_or_default();
    body.truncate(512);
    ManagerError::Status { status, body }
}
