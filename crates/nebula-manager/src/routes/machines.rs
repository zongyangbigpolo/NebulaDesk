//! Machine enrolment and lifecycle.
//!
//! An agent never uses a human's credentials. It enrols once with a
//! single-use token and receives its own machine credential, which is what it
//! presents thereafter. Revoking a machine is deleting its row; there is no
//! long-lived shared secret to rotate across a fleet.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nebula_common::{MachineId, TenantId};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::audit::{Entry, Outcome};
use crate::auth::extract::{AuthMachine, AuthUser};
use crate::auth::tokens;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// How long an enrolment token stays usable if the caller does not say.
const DEFAULT_ENROLLMENT_TTL: Duration = Duration::hours(24);

/// An enrolment token that has just been consumed.
#[derive(Debug, sqlx::FromRow)]
struct ClaimedToken {
    id: Uuid,
    tenant_id: Uuid,
    machine_name: Option<String>,
    owner_user_id: Option<Uuid>,
    region: String,
}

/// Request for a new enrolment token.
#[derive(Debug, Deserialize)]
pub struct CreateEnrollmentToken {
    /// Pin the machine name the token may claim.
    ///
    /// Strongly recommended: without it, anyone who intercepts the token can
    /// enrol a machine under any name, including one that looks legitimate.
    #[serde(default)]
    pub machine_name: Option<String>,
    /// Assign the enrolled machine to a user.
    #[serde(default)]
    pub owner_user_id: Option<Uuid>,
    /// Validity in seconds.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    /// Which region the enrolled machine belongs to.
    ///
    /// Set by the operator rather than the agent: a machine that could pick
    /// its own region could pull sessions onto a gateway of its choosing.
    #[serde(default)]
    pub region: Option<String>,
}

/// A newly minted enrolment token. The secret is shown exactly once.
#[derive(Debug, Serialize)]
pub struct EnrollmentTokenCreated {
    /// Token identifier, for revocation.
    pub id: Uuid,
    /// The secret to hand to the agent. Not recoverable later.
    pub token: String,
    /// When it stops working.
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// `POST /v1/machines/enrollment-tokens`
pub async fn create_enrollment_token(
    State(state): State<AppState>,
    caller: AuthUser,
    Json(mut req): Json<CreateEnrollmentToken>,
) -> ApiResult<(StatusCode, Json<EnrollmentTokenCreated>)> {
    if !caller.role.is_admin() {
        if req
            .owner_user_id
            .is_some_and(|id| id != caller.id.as_uuid())
            || req.region.is_some()
        {
            return Err(ApiError::Forbidden(
                "self-service enrollment cannot choose another owner or a region".into(),
            ));
        }
        if req
            .machine_name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            return Err(ApiError::BadRequest(
                "self-service enrollment requires a nonempty machine_name".into(),
            ));
        }
        req.owner_user_id = Some(caller.id.as_uuid());
    }
    if let Some(owner) = req.owner_user_id {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM users
             WHERE tenant_id = $1 AND id = $2 AND NOT disabled)",
        )
        .bind(caller.tenant.as_uuid())
        .bind(owner)
        .fetch_one(&state.db)
        .await?;
        if !exists {
            return Err(ApiError::NotFound("owner"));
        }
    }
    let ttl = match req.ttl_secs {
        Some(s) if s > 0 && s <= 30 * 24 * 3600 => Duration::seconds(s),
        Some(_) => {
            return Err(ApiError::BadRequest(
                "ttl_secs must be between 1 second and 30 days".into(),
            ))
        }
        None => DEFAULT_ENROLLMENT_TTL,
    };

    let token = tokens::generate_opaque();
    let id = Uuid::now_v7();
    let expires_at = OffsetDateTime::now_utc() + ttl;

    sqlx::query(
        "INSERT INTO enrollment_tokens
           (id, tenant_id, token_hash, machine_name, owner_user_id, expires_at,
            created_by, region)
         VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, 'default'))",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .bind(&token.hash)
    .bind(req.machine_name.as_deref())
    .bind(req.owner_user_id)
    .bind(expires_at)
    .bind(caller.id.as_uuid())
    .bind(req.region.as_deref())
    .execute(&state.db)
    .await?;

    Entry::new("machine.enrollment_token.create", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("enrollment_token", id)
        .write(&state.db)
        .await;

    Ok((
        StatusCode::CREATED,
        Json(EnrollmentTokenCreated {
            id,
            token: token.secret,
            expires_at,
        }),
    ))
}

/// What an agent sends when enrolling.
#[derive(Debug, Deserialize)]
pub struct EnrollRequest {
    /// The single-use token.
    pub token: String,
    /// The name to register under.
    pub name: String,
    /// Operating system: `WINDOWS`, `MACOS` or `LINUX`.
    pub os: String,
    /// OS version string.
    #[serde(default)]
    pub os_version: String,
    /// CPU architecture.
    #[serde(default)]
    pub arch: String,
    /// Agent build version.
    #[serde(default)]
    pub agent_version: String,
    /// The agent's Noise static public key, hex encoded.
    pub noise_public_key: String,
    /// Capability advertisement, stored verbatim.
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// What the agent gets back.
#[derive(Debug, Serialize)]
pub struct EnrollResponse {
    /// The machine's identifier.
    pub machine_id: MachineId,
    /// Tenant it belongs to.
    pub tenant_id: TenantId,
    /// The credential to present on subsequent requests, `<id>.<secret>`.
    pub credential: String,
    /// Gateways the agent should maintain a control tunnel to.
    pub gateways: Vec<GatewayView>,
}

/// A gateway an agent or client may connect to.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GatewayView {
    /// Identifier.
    pub id: Uuid,
    /// QUIC address.
    pub quic_addr: String,
    /// Certificate pin, hex SHA-256.
    pub cert_pin: String,
    /// Deployment region.
    pub region: String,
}

/// `POST /v1/machines/enroll`
///
/// Unauthenticated by design: the token *is* the credential. It is consumed
/// atomically, so a token that leaks after use is worthless and a race between
/// two agents produces exactly one machine.
pub async fn enroll(
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> ApiResult<(StatusCode, Json<EnrollResponse>)> {
    let noise_key = hex::decode(req.noise_public_key.trim())
        .map_err(|_| ApiError::BadRequest("noise_public_key must be hex".into()))?;
    if noise_key.len() != 32 {
        return Err(ApiError::BadRequest(
            "noise_public_key must be 32 bytes".into(),
        ));
    }
    let os = match req.os.to_ascii_uppercase().as_str() {
        os @ ("WINDOWS" | "MACOS" | "LINUX") => os.to_string(),
        other => return Err(ApiError::BadRequest(format!("unsupported os {other}"))),
    };

    let mut tx = state.db.begin().await?;

    // Claim the token and read its constraints in one statement: two agents
    // presenting the same token cannot both win.
    let claimed: Option<ClaimedToken> = sqlx::query_as(
        "UPDATE enrollment_tokens
         SET used_at = now()
         WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
         RETURNING id, tenant_id, machine_name, owner_user_id, region",
    )
    .bind(tokens::hash_token(&req.token))
    .fetch_optional(&mut *tx)
    .await?;

    let Some(ClaimedToken {
        id: token_id,
        tenant_id: tenant,
        machine_name: pinned_name,
        owner_user_id: owner,
        region,
    }) = claimed
    else {
        return Err(ApiError::Unauthorized);
    };

    // A token pinned to a name may only enrol that name.
    let name = match pinned_name {
        Some(pinned) if pinned != req.name => {
            return Err(ApiError::Forbidden(
                "this enrolment token is bound to a different machine name".into(),
            ))
        }
        Some(pinned) => pinned,
        None => req.name.clone(),
    };

    let machine = MachineId::new();
    let credential = tokens::generate_opaque();

    sqlx::query(
        "INSERT INTO machines
           (id, tenant_id, name, os, os_version, arch, agent_version,
            noise_public_key, credential_hash, owner_user_id, capabilities, region)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(machine.as_uuid())
    .bind(tenant)
    .bind(&name)
    .bind(&os)
    .bind(&req.os_version)
    .bind(&req.arch)
    .bind(&req.agent_version)
    .bind(&noise_key)
    .bind(&credential.hash)
    .bind(owner)
    .bind(if req.capabilities.is_null() {
        serde_json::json!({})
    } else {
        req.capabilities.clone()
    })
    .bind(&region)
    .execute(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("a machine with that name or key already exists".into())
        }
        _ => ApiError::Database(e),
    })?;

    sqlx::query("UPDATE enrollment_tokens SET used_by = $2 WHERE id = $1")
        .bind(token_id)
        .bind(machine.as_uuid())
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    let gateways = sqlx::query_as::<_, GatewayView>(
        "SELECT id, quic_addr, cert_pin, region FROM gateways ORDER BY load ASC LIMIT 4",
    )
    .fetch_all(&state.db)
    .await?;

    let tenant = TenantId::from_uuid(tenant);
    Entry::new("machine.enroll", Outcome::Allow)
        .tenant(tenant)
        .target("machine", machine.as_uuid())
        .detail(serde_json::json!({ "name": name, "os": os }))
        .write(&state.db)
        .await;

    Ok((
        StatusCode::CREATED,
        Json(EnrollResponse {
            machine_id: machine,
            tenant_id: tenant,
            credential: format!("{machine}.{}", credential.secret),
            gateways,
        }),
    ))
}

/// A machine as seen by its owner or an administrator.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct MachineRow {
    /// Identifier.
    pub id: Uuid,
    /// Name.
    pub name: String,
    /// Operating system.
    pub os: String,
    /// OS version.
    pub os_version: String,
    /// Architecture.
    pub arch: String,
    /// Agent build.
    pub agent_version: String,
    /// Connection status.
    pub status: String,
    /// Last contact.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    /// Owning user, if assigned.
    pub owner_user_id: Option<Uuid>,
    /// Advertised capabilities.
    pub capabilities: serde_json::Value,
    /// Enrolment time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// `GET /v1/machines`
pub async fn list_machines(
    State(state): State<AppState>,
    caller: AuthUser,
) -> ApiResult<Json<Vec<MachineRow>>> {
    let rows = sqlx::query_as::<_, MachineRow>(
        "SELECT id, name, os, os_version, arch, agent_version, status,
                last_seen_at, owner_user_id, capabilities, created_at
         FROM machines WHERE tenant_id = $1 AND ($2 OR owner_user_id = $3)
         ORDER BY name",
    )
    .bind(caller.tenant.as_uuid())
    .bind(caller.role.is_admin())
    .bind(caller.id.as_uuid())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Fields owners may change without changing device identity or placement.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameMachine {
    /// New display name.
    pub name: String,
}

/// `PATCH /v1/machines/{id}`
pub async fn rename_machine(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
    Json(req): Json<RenameMachine>,
) -> ApiResult<StatusCode> {
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    let result = sqlx::query(
        "UPDATE machines SET name = $4
         WHERE id = $1 AND tenant_id = $2 AND ($3 OR owner_user_id = $5)",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .bind(caller.role.is_admin())
    .bind(name)
    .bind(caller.id.as_uuid())
    .execute(&state.db)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("a machine with that name already exists".into())
        }
        _ => ApiError::Database(e),
    })?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("machine"));
    }
    Entry::new("machine.rename", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("machine", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /v1/machines/{id}`
pub async fn delete_machine(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let result = sqlx::query(
        "DELETE FROM machines WHERE id = $1 AND tenant_id = $2
         AND ($3 OR owner_user_id = $4)",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .bind(caller.role.is_admin())
    .bind(caller.id.as_uuid())
    .execute(&state.db)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("machine"));
    }
    Entry::new("machine.delete", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("machine", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// A status report from a running agent.
#[derive(Debug, Deserialize)]
pub struct Heartbeat {
    /// `ONLINE` or `DRAINING`. An agent never reports itself offline; a
    /// missing heartbeat is what makes a machine offline.
    #[serde(default)]
    pub status: Option<String>,
    /// The gateway the agent's control tunnel is attached to.
    #[serde(default)]
    pub gateway_id: Option<Uuid>,
    /// Updated capability advertisement.
    #[serde(default)]
    pub capabilities: Option<serde_json::Value>,
    /// Updated agent version, after a self-update.
    #[serde(default)]
    pub agent_version: Option<String>,
}

/// `POST /v1/machines/heartbeat`
///
/// Authenticated with the machine credential, so an agent can only ever
/// report on itself.
pub async fn heartbeat(
    State(state): State<AppState>,
    machine: AuthMachine,
    Json(req): Json<Heartbeat>,
) -> ApiResult<StatusCode> {
    if let Some(status) = req.status.as_deref() {
        if !matches!(status, "ONLINE" | "DRAINING") {
            return Err(ApiError::BadRequest(
                "status must be ONLINE or DRAINING".into(),
            ));
        }
    }
    sqlx::query(
        "UPDATE machines SET
           status        = COALESCE($2, 'ONLINE'),
           gateway_id    = COALESCE($3, gateway_id),
           capabilities  = COALESCE($4, capabilities),
           agent_version = COALESCE($5, agent_version),
           last_seen_at  = now()
         WHERE id = $1",
    )
    .bind(machine.id.as_uuid())
    .bind(req.status.as_deref())
    .bind(req.gateway_id)
    .bind(req.capabilities.as_ref())
    .bind(req.agent_version.as_deref())
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The gateway an agent should attach its control tunnel to.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GatewayAssignment {
    /// The gateway's node id.
    pub id: Uuid,
    /// QUIC address to dial.
    pub quic_addr: String,
    /// Certificate pin, hex SHA-256 of the DER certificate.
    pub cert_pin: String,
}

/// `GET /v1/machines/session-authority`
///
/// An agent fetches this over its enrolled trusted manager connection, not a
/// gateway-supplied URL. Machine authentication prevents an accidental fetch
/// against a different manager deployment from silently becoming authority.
pub async fn session_authority(
    State(state): State<AppState>,
    _machine: AuthMachine,
) -> Json<nebula_common::TicketAuthority> {
    Json(nebula_common::TicketAuthority {
        issuer: state.signer.issuer().to_owned(),
        jwks: state.signer.jwks(),
    })
}

/// `GET /v1/machines/self/gateway`
///
/// An agent cannot be configured with a gateway address: gateways are
/// infrastructure that gets replaced, and a machine on someone's desk is the
/// last place to keep that knowledge. It asks instead, authenticated as
/// itself, and reconnects through the same call when its gateway goes away.
pub async fn my_gateway(
    State(state): State<AppState>,
    machine: AuthMachine,
) -> ApiResult<Json<GatewayAssignment>> {
    // Stickiness first: sessions already placed on this machine's gateway
    // would have to be torn down if the agent wandered to another one.
    let row = sqlx::query_as::<_, GatewayAssignment>(&format!(
        "SELECT g.id, g.quic_addr, g.cert_pin
         FROM machines me
         JOIN gateways g ON g.load < g.capacity AND {alive}
         LEFT JOIN machines m ON m.gateway_id = g.id AND m.id = me.id
         WHERE me.id = $1
         ORDER BY (g.region <> me.region), (m.id IS NULL), g.load ASC
         LIMIT 1",
        alive = super::nodes::alive("g"),
    ))
    .bind(machine.id.as_uuid())
    .fetch_optional(&state.db)
    .await?;
    row.map(Json)
        .ok_or_else(|| ApiError::Conflict("no gateway has capacity right now".into()))
}
