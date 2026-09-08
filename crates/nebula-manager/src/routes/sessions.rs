//! Session brokering.
//!
//! This is the endpoint every launch goes through. It answers one question —
//! "may this user open this resource right now?" — and, if so, hands back a
//! short-lived signed ticket plus the addresses to connect to. After this
//! call the manager is out of the data path entirely.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use nebula_common::{ResourceId, SessionId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{Entry, Outcome};
use crate::auth::extract::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::routes::resources::resolve_grant;
use crate::routes::ClientIp;
use crate::state::AppState;

/// A request to open a session.
#[derive(Debug, Deserialize)]
pub struct CreateSession {
    /// The published resource to launch.
    pub resource_id: Uuid,
    /// The client's operating system, recorded for support and key mapping.
    #[serde(default)]
    pub client_os: Option<String>,
}

/// Everything a client needs to establish the session.
#[derive(Debug, Serialize)]
pub struct SessionTicket {
    /// The session identifier, also the ticket's `jti`.
    pub session_id: SessionId,
    /// The resource being launched.
    pub resource_id: ResourceId,
    /// The signed ticket, presented to the gateway.
    pub ticket: String,
    /// Seconds until the ticket expires.
    pub expires_in: i64,
    /// QUIC address of the gateway to connect to.
    pub gateway_addr: String,
    /// Certificate pin for the gateway.
    pub gateway_pin: String,
    /// QUIC address of the relay carrying the media.
    pub relay_addr: String,
    /// Certificate pin for the relay.
    pub relay_pin: String,
    /// The agent's Noise static public key, hex encoded.
    pub agent_key: String,
    /// What this session is permitted to do.
    ///
    /// The ticket carries the authoritative copy and the agent enforces it;
    /// this is here so the client knows not to offer the user a feature the
    /// session will silently refuse.
    pub policy: nebula_common::SessionPolicy,
}

/// `POST /v1/sessions`
pub async fn create(
    State(state): State<AppState>,
    caller: AuthUser,
    ClientIp(ip): ClientIp,
    Json(req): Json<CreateSession>,
) -> ApiResult<(StatusCode, Json<SessionTicket>)> {
    let deny = |reason: &'static str| {
        let state = state.clone();
        let tenant = caller.tenant;
        let actor = caller.id;
        let resource = req.resource_id;
        let ip = ip.clone();
        async move {
            Entry::new("session.create", Outcome::Deny)
                .tenant(tenant)
                .actor(actor)
                .target("resource", resource)
                .detail(serde_json::json!({ "reason": reason }))
                .ip(ip)
                .write(&state.db)
                .await;
        }
    };

    // Entitlement first: a user who may not see a resource must not be able
    // to learn whether it exists, or whether its machine happens to be up.
    let Some(grant) = resolve_grant(&state, &caller, req.resource_id).await? else {
        deny("not_entitled").await;
        return Err(ApiError::NotFound("resource"));
    };

    if grant.kind != "DESKTOP" {
        deny("app_streaming_unsupported").await;
        return Err(ApiError::Conflict(
            "isolated APP streaming is not supported; desktop fallback is forbidden".into(),
        ));
    }

    if grant.machine_status != "ONLINE" {
        deny("machine_offline").await;
        return Err(ApiError::Conflict(
            "the machine serving this resource is not online".into(),
        ));
    }

    let (role, policy) = grant.policy()?;

    // Placement: least-loaded node in the machine's region. The gateway is
    // where the agent's control tunnel already terminates when we know it,
    // because routing the client elsewhere would mean a gateway-to-gateway
    // hop for no benefit.
    let gateway = pick_gateway(&state, grant.machine_id).await?;
    let relay = pick_relay(&state, &gateway.region).await?;

    let session = SessionId::new();
    let claims = state.signer.claims(
        session,
        caller.tenant,
        caller.id,
        nebula_common::MachineId::from_uuid(grant.machine_id),
        ResourceId::from_uuid(req.resource_id),
        role,
        policy,
        hex::encode(&grant.noise_public_key),
        relay.quic_addr.clone(),
        relay.cert_pin.clone(),
    );
    let ticket = state.signer.issue(&claims)?;

    // Recording the session before returning the ticket is what makes the
    // `jti` unique constraint meaningful: a ticket that was never persisted
    // could otherwise be replayed against a stateless gateway.
    sqlx::query(
        "INSERT INTO sessions
           (id, tenant_id, resource_id, machine_id, user_id, gateway_id, relay_id,
            role, ticket_jti, client_ip, client_os)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(session.as_uuid())
    .bind(caller.tenant.as_uuid())
    .bind(req.resource_id)
    .bind(grant.machine_id)
    .bind(caller.id.as_uuid())
    .bind(gateway.id)
    .bind(relay.id)
    .bind(role.as_db())
    .bind(&claims.jti)
    .bind(ip.as_deref())
    .bind(req.client_os.as_deref())
    .execute(&state.db)
    .await?;

    Entry::new("session.create", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("session", session.as_uuid())
        .detail(serde_json::json!({
            "resource_id": req.resource_id,
            "machine_id": grant.machine_id,
            "role": role.as_db(),
            "gateway_id": gateway.id,
            "relay_id": relay.id,
        }))
        .ip(ip)
        .write(&state.db)
        .await;

    Ok((
        StatusCode::CREATED,
        Json(SessionTicket {
            session_id: session,
            resource_id: ResourceId::from_uuid(req.resource_id),
            expires_in: claims.remaining_secs(OffsetDateTime::now_utc()),
            ticket,
            gateway_addr: gateway.quic_addr,
            gateway_pin: gateway.cert_pin,
            relay_addr: relay.quic_addr,
            relay_pin: relay.cert_pin,
            agent_key: claims.agent_key,
            policy,
        }),
    ))
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: Uuid,
    quic_addr: String,
    cert_pin: String,
    region: String,
}

async fn pick_gateway(state: &AppState, machine: Uuid) -> ApiResult<NodeRow> {
    // Prefer the gateway the machine's control tunnel is already attached to;
    // fall back to the least loaded one that has headroom.
    let row = sqlx::query_as::<_, NodeRow>(&format!(
        "SELECT g.id, g.quic_addr, g.cert_pin, g.region
         FROM machines me
         JOIN gateways g ON g.load < g.capacity AND {alive}
         LEFT JOIN machines m ON m.gateway_id = g.id AND m.id = me.id
         WHERE me.id = $1
         ORDER BY (m.id IS NULL), (g.region <> me.region), g.load ASC
         LIMIT 1",
        alive = super::nodes::alive("g"),
    ))
    .bind(machine)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| ApiError::Conflict("no gateway has capacity right now".into()))
}

async fn pick_relay(state: &AppState, region: &str) -> ApiResult<NodeRow> {
    // Same region first: a relay in another continent turns a 20 ms session
    // into a 200 ms one, which no amount of encoder tuning recovers.
    let row = sqlx::query_as::<_, NodeRow>(&format!(
        "SELECT r.id, r.quic_addr, r.cert_pin, r.region
         FROM relays r
         WHERE r.load < r.capacity_mbps AND {alive}
         ORDER BY (r.region <> $1), r.load ASC
         LIMIT 1",
        alive = super::nodes::alive("r"),
    ))
    .bind(region)
    .fetch_optional(&state.db)
    .await?;
    row.ok_or_else(|| ApiError::Conflict("no relay has capacity right now".into()))
}

/// A session as listed in the console.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct SessionRow {
    /// Identifier.
    pub id: Uuid,
    /// The resource launched.
    pub resource_id: Uuid,
    /// The machine serving it.
    pub machine_id: Uuid,
    /// The user who opened it.
    pub user_id: Uuid,
    /// Lifecycle state.
    pub state: String,
    /// Granted role.
    pub role: String,
    /// Where the client connected from.
    pub client_ip: Option<String>,
    /// Bytes sent by the client.
    pub bytes_up: i64,
    /// Bytes sent to the client.
    pub bytes_down: i64,
    /// Why it ended.
    pub close_reason: Option<String>,
    /// When it was requested.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When media started flowing.
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    /// When it ended.
    #[serde(with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
}

/// Filters for the session list.
#[derive(Debug, Deserialize)]
pub struct SessionFilter {
    /// Restrict to one lifecycle state.
    #[serde(default)]
    pub state: Option<String>,
    /// Maximum rows to return.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /v1/sessions`
///
/// Administrators see the whole tenant; everyone else sees only their own
/// sessions.
pub async fn list(
    State(state): State<AppState>,
    caller: AuthUser,
    Query(filter): Query<SessionFilter>,
) -> ApiResult<Json<Vec<SessionRow>>> {
    let limit = filter.limit.unwrap_or(100).clamp(1, 1000);
    let scope_user = if caller.role.is_admin() {
        None
    } else {
        Some(caller.id.as_uuid())
    };
    let rows = sqlx::query_as::<_, SessionRow>(
        "SELECT id, resource_id, machine_id, user_id, state, role, client_ip,
                bytes_up, bytes_down, close_reason, created_at, started_at, ended_at
         FROM sessions
         WHERE tenant_id = $1
           AND ($2::uuid IS NULL OR user_id = $2)
           AND ($3::text IS NULL OR state = $3)
         ORDER BY created_at DESC
         LIMIT $4",
    )
    .bind(caller.tenant.as_uuid())
    .bind(scope_user)
    .bind(filter.state.as_deref())
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `DELETE /v1/sessions/{id}`
///
/// Marks the session closed in the control-plane record. This does not yet
/// notify the gateway or terminate an already established data path.
pub async fn close(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let scope_user = if caller.role.is_admin() {
        None
    } else {
        Some(caller.id.as_uuid())
    };
    let result = sqlx::query(
        "UPDATE sessions
         SET state = 'CLOSED', ended_at = now(), close_reason = 'closed_by_user'
         WHERE id = $1 AND tenant_id = $2
           AND ($3::uuid IS NULL OR user_id = $3)
           AND state IN ('PENDING', 'ACTIVE')",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .bind(scope_user)
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("session"));
    }
    Entry::new("session.close", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("session", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}
