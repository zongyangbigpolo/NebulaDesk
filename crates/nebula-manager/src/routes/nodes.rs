//! Gateway and relay registration, and the gateway's session callbacks.
//!
//! Nodes are infrastructure, not tenants: they are registered once by an
//! operator holding the bootstrap secret and thereafter authenticate with
//! their own credential. A node credential grants nothing except the ability
//! to report on sessions it is actually carrying.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::extract::BootstrapAuth;
use crate::auth::tokens;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Which kind of node a credential belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// An edge gateway.
    Gateway,
    /// A media relay.
    Relay,
}

impl NodeKind {
    const fn table(self) -> &'static str {
        match self {
            NodeKind::Gateway => "gateways",
            NodeKind::Relay => "relays",
        }
    }
}

/// An authenticated gateway or relay process.
#[derive(Debug, Clone, Copy)]
pub struct AuthNode {
    /// The node's id.
    pub id: Uuid,
    /// Whether it is a gateway or a relay.
    pub kind: NodeKind,
}

impl FromRequestParts<AppState> for AuthNode {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw = parts
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Node"))
            .map(|(_, value)| value.trim())
            .ok_or(ApiError::Unauthorized)?;
        let (id, secret) = tokens::parse_machine_credential(raw)?;
        let hash = tokens::hash_token(secret);

        // Try both tables: the credential carries the id, so at most one row
        // can match, and the caller need not say which kind it is.
        for kind in [NodeKind::Gateway, NodeKind::Relay] {
            let found: Option<(bool,)> = sqlx::query_as(&format!(
                "SELECT secret_hash = $2 FROM {} WHERE id = $1",
                kind.table()
            ))
            .bind(id)
            .bind(&hash)
            .fetch_optional(&state.db)
            .await?;
            if let Some((true,)) = found {
                // Registration is rare and heartbeats are frequent; recording
                // liveness here means a node that keeps calling stays visible
                // without a separate heartbeat endpoint.
                let _ = sqlx::query(&format!(
                    "UPDATE {} SET last_seen_at = now() WHERE id = $1",
                    kind.table()
                ))
                .bind(id)
                .execute(&state.db)
                .await;
                return Ok(Self { id, kind });
            }
        }
        Err(ApiError::Unauthorized)
    }
}

/// Registration details for a gateway.
#[derive(Debug, Deserialize)]
pub struct RegisterGateway {
    /// Unique node name.
    pub name: String,
    /// HTTPS base URL, for operator tooling.
    pub public_url: String,
    /// QUIC address clients and agents dial.
    pub quic_addr: String,
    /// Hex SHA-256 of the node's certificate, in DER form.
    pub cert_pin: String,
    /// Deployment region.
    #[serde(default = "default_region")]
    pub region: String,
    /// Maximum concurrent sessions.
    #[serde(default)]
    pub capacity: Option<i32>,
}

/// Registration details for a relay.
#[derive(Debug, Deserialize)]
pub struct RegisterRelay {
    /// Unique node name.
    pub name: String,
    /// QUIC address the gateway and agents dial.
    pub quic_addr: String,
    /// Hex SHA-256 of the node's certificate, in DER form.
    pub cert_pin: String,
    /// Deployment region.
    #[serde(default = "default_region")]
    pub region: String,
    /// Throughput budget in megabits per second.
    #[serde(default)]
    pub capacity_mbps: Option<i32>,
}

fn default_region() -> String {
    "default".into()
}

/// A freshly registered node's credential, shown once.
#[derive(Debug, Serialize)]
pub struct NodeRegistered {
    /// The node's identifier.
    pub id: Uuid,
    /// The credential to configure on the node, `<id>.<secret>`.
    pub credential: String,
}

/// `POST /v1/gateways`
pub async fn register_gateway(
    State(state): State<AppState>,
    _auth: BootstrapAuth,
    Json(req): Json<RegisterGateway>,
) -> ApiResult<(StatusCode, Json<NodeRegistered>)> {
    validate_pin(&req.cert_pin)?;
    let id = Uuid::now_v7();
    let credential = tokens::generate_opaque();

    // Re-registering by name rotates the credential in place, which is what a
    // redeployed node needs: a new row would strand every machine pointing at
    // the old id.
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO gateways
           (id, name, public_url, quic_addr, cert_pin, region, capacity, secret_hash, last_seen_at)
         VALUES ($1, $2, $3, $4, $5, $6, COALESCE($7, 1000), $8, now())
         ON CONFLICT (name) DO UPDATE SET
           public_url   = EXCLUDED.public_url,
           quic_addr    = EXCLUDED.quic_addr,
           cert_pin     = EXCLUDED.cert_pin,
           region       = EXCLUDED.region,
           capacity     = EXCLUDED.capacity,
           secret_hash  = EXCLUDED.secret_hash,
           last_seen_at = now()
         RETURNING id",
    )
    .bind(id)
    .bind(&req.name)
    .bind(&req.public_url)
    .bind(&req.quic_addr)
    .bind(req.cert_pin.to_ascii_lowercase())
    .bind(&req.region)
    .bind(req.capacity)
    .bind(&credential.hash)
    .fetch_one(&state.db)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(NodeRegistered {
            id,
            credential: format!("{id}.{}", credential.secret),
        }),
    ))
}

/// `POST /v1/relays`
pub async fn register_relay(
    State(state): State<AppState>,
    _auth: BootstrapAuth,
    Json(req): Json<RegisterRelay>,
) -> ApiResult<(StatusCode, Json<NodeRegistered>)> {
    validate_pin(&req.cert_pin)?;
    let id = Uuid::now_v7();
    let credential = tokens::generate_opaque();

    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO relays
           (id, name, quic_addr, cert_pin, region, capacity_mbps, secret_hash, last_seen_at)
         VALUES ($1, $2, $3, $4, $5, COALESCE($6, 1000), $7, now())
         ON CONFLICT (name) DO UPDATE SET
           quic_addr     = EXCLUDED.quic_addr,
           cert_pin      = EXCLUDED.cert_pin,
           region        = EXCLUDED.region,
           capacity_mbps = EXCLUDED.capacity_mbps,
           secret_hash   = EXCLUDED.secret_hash,
           last_seen_at  = now()
         RETURNING id",
    )
    .bind(id)
    .bind(&req.name)
    .bind(&req.quic_addr)
    .bind(req.cert_pin.to_ascii_lowercase())
    .bind(&req.region)
    .bind(req.capacity_mbps)
    .bind(&credential.hash)
    .fetch_one(&state.db)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(NodeRegistered {
            id,
            credential: format!("{id}.{}", credential.secret),
        }),
    ))
}

/// A node's view of a session's progress.
#[derive(Debug, Deserialize)]
pub struct SessionReport {
    /// New lifecycle state: `ACTIVE`, `CLOSED` or `FAILED`.
    pub state: String,
    /// Why it ended, when it did.
    #[serde(default)]
    pub reason: Option<String>,
    /// Bytes received from the client so far.
    #[serde(default)]
    pub bytes_up: Option<i64>,
    /// Bytes sent to the client so far.
    #[serde(default)]
    pub bytes_down: Option<i64>,
}

/// `POST /v1/sessions/{id}/report`
///
/// Called by the gateway as a session starts, ends or fails. A node may only
/// report on sessions it was assigned, so a compromised relay cannot rewrite
/// another node's accounting.
pub async fn report_session(
    State(state): State<AppState>,
    node: AuthNode,
    Path(id): Path<Uuid>,
    body: Result<Json<SessionReport>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Json(req) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    if !matches!(req.state.as_str(), "ACTIVE" | "CLOSED" | "FAILED") {
        return Err(ApiError::BadRequest(
            "state must be ACTIVE, CLOSED or FAILED".into(),
        ));
    }
    let column = match node.kind {
        NodeKind::Gateway => "gateway_id",
        NodeKind::Relay => "relay_id",
    };

    // Byte counters are cumulative from the node's point of view, so taking
    // the maximum keeps a retried or out-of-order report from moving them
    // backwards.
    let result = sqlx::query(&format!(
        "UPDATE sessions SET
           state        = $3,
           close_reason = COALESCE($4, close_reason),
           bytes_up     = GREATEST(bytes_up, COALESCE($5, 0)),
           bytes_down   = GREATEST(bytes_down, COALESCE($6, 0)),
           started_at   = CASE WHEN $3 = 'ACTIVE' AND started_at IS NULL
                               THEN now() ELSE started_at END,
           ended_at     = CASE WHEN $3 IN ('CLOSED', 'FAILED')
                               THEN now() ELSE ended_at END
         WHERE id = $1 AND {column} = $2 AND state IN ('PENDING', 'ACTIVE')"
    ))
    .bind(id)
    .bind(node.id)
    .bind(&req.state)
    .bind(req.reason.as_deref())
    .bind(req.bytes_up)
    .bind(req.bytes_down)
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("session"));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validate_pin(pin: &str) -> ApiResult<()> {
    if pin.len() == 64 && pin.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "cert_pin must be a hex SHA-256 digest".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_must_be_full_sha256_digests() {
        assert!(validate_pin(&"ab".repeat(32)).is_ok());
        assert!(validate_pin(&"AB".repeat(32)).is_ok());
        // A truncated pin would silently weaken certificate binding.
        assert!(validate_pin(&"ab".repeat(16)).is_err());
        assert!(validate_pin("").is_err());
        assert!(validate_pin(&"zz".repeat(32)).is_err());
    }
}
