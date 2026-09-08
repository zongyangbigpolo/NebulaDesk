//! Published resources and entitlements.
//!
//! This is where "publish an app to a specific user" is expressed. A machine
//! offers *resources*: either its whole desktop, or one named application.
//! An *entitlement* grants a user or a group access to one resource with a
//! specific role and policy. A user's resource list is therefore the join of
//! the two, which is exactly what the client renders as icons. Applications
//! hide their backing machines; desktops expose device details.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nebula_common::{ResourceId, SessionPolicy, SessionRole};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{Entry, Outcome};
use crate::auth::extract::AuthUser;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

// Hold the machine lock until the mutation commits: an ownership transfer must
// not race a former owner's publication or entitlement change.
async fn lock_managed_machine(
    db: &mut sqlx::PgConnection,
    caller: &AuthUser,
    machine: Uuid,
) -> ApiResult<()> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM machines WHERE tenant_id = $1 AND id = $2
         AND ($3 OR owner_user_id = $4) FOR SHARE",
    )
    .bind(caller.tenant.as_uuid())
    .bind(machine)
    .bind(caller.role.is_admin())
    .bind(caller.id.as_uuid())
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound("machine"))?;
    Ok(())
}

async fn lock_managed_resource(
    db: &mut sqlx::PgConnection,
    caller: &AuthUser,
    resource: Uuid,
) -> ApiResult<()> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT r.id FROM published_resources r
         JOIN machines m ON m.id = r.machine_id AND m.tenant_id = r.tenant_id
         WHERE r.tenant_id = $1 AND r.id = $2 AND ($3 OR m.owner_user_id = $4)
         FOR SHARE OF m FOR UPDATE OF r",
    )
    .bind(caller.tenant.as_uuid())
    .bind(resource)
    .bind(caller.role.is_admin())
    .bind(caller.id.as_uuid())
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound("resource"))?;
    Ok(())
}

/// Request to publish a resource from a machine.
#[derive(Debug, Deserialize)]
pub struct CreateResource {
    /// `DESKTOP` or `APP`.
    pub kind: String,
    /// Display name, unique per machine.
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: String,
    /// Executable to launch. Required for `APP`, forbidden for `DESKTOP`.
    #[serde(default)]
    pub launch_path: Option<String>,
    /// Arguments passed to the executable.
    #[serde(default)]
    pub launch_args: Vec<String>,
    /// Working directory for the launched process.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// How the agent recognises the application's windows.
    #[serde(default)]
    pub window_match: serde_json::Value,
}

/// A published resource.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ResourceRow {
    /// Identifier.
    pub id: Uuid,
    /// Owning machine.
    pub machine_id: Uuid,
    /// `DESKTOP` or `APP`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Executable, for `APP`.
    pub launch_path: Option<String>,
    /// Arguments.
    pub launch_args: Vec<String>,
    /// Working directory.
    pub working_dir: Option<String>,
    /// Window matcher.
    pub window_match: serde_json::Value,
    /// Whether it can be launched.
    pub enabled: bool,
    /// Publication time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// `POST /v1/machines/{id}/resources`
pub async fn publish(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(machine): Path<Uuid>,
    Json(req): Json<CreateResource>,
) -> ApiResult<(StatusCode, Json<ResourceRow>)> {
    let kind = req.kind.to_ascii_uppercase();
    // The schema enforces this too, but a clear 400 beats a constraint
    // violation surfacing as a 409.
    match kind.as_str() {
        "APP" if req.launch_path.as_deref().unwrap_or("").is_empty() => {
            return Err(ApiError::BadRequest(
                "an APP resource needs a launch_path".into(),
            ))
        }
        "DESKTOP" if req.launch_path.is_some() => {
            return Err(ApiError::BadRequest(
                "a DESKTOP resource must not have a launch_path".into(),
            ))
        }
        "APP" | "DESKTOP" => {}
        other => return Err(ApiError::BadRequest(format!("unknown kind {other}"))),
    }

    let mut tx = state.db.begin().await?;
    lock_managed_machine(&mut tx, &caller, machine).await?;
    let id = ResourceId::new();
    let row = sqlx::query_as::<_, ResourceRow>(
        "INSERT INTO published_resources
           (id, tenant_id, machine_id, kind, name, description,
            launch_path, launch_args, working_dir, window_match)
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10
         WHERE EXISTS (SELECT 1 FROM machines WHERE id = $3 AND tenant_id = $2)
         RETURNING id, machine_id, kind, name, description, launch_path,
                   launch_args, working_dir, window_match, enabled, created_at",
    )
    .bind(id.as_uuid())
    .bind(caller.tenant.as_uuid())
    .bind(machine)
    .bind(&kind)
    .bind(&req.name)
    .bind(&req.description)
    .bind(req.launch_path.as_deref())
    .bind(&req.launch_args)
    .bind(req.working_dir.as_deref())
    .bind(if req.window_match.is_null() {
        serde_json::json!({})
    } else {
        req.window_match.clone()
    })
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict("that machine already publishes a resource with this name".into())
        }
        _ => ApiError::Database(e),
    })?
    // The `WHERE EXISTS` guard returns no row when the machine is not ours,
    // which keeps another tenant's machine ids unguessable.
    .ok_or(ApiError::NotFound("machine"))?;
    tx.commit().await?;

    Entry::new("resource.publish", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("resource", row.id)
        .detail(serde_json::json!({ "kind": kind, "name": req.name }))
        .write(&state.db)
        .await;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /v1/machines/{id}/resources`
pub async fn list_for_machine(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(machine): Path<Uuid>,
) -> ApiResult<Json<Vec<ResourceRow>>> {
    let rows = sqlx::query_as::<_, ResourceRow>(
        "SELECT id, machine_id, kind, name, description, launch_path,
                launch_args, working_dir, window_match, enabled, created_at
         FROM published_resources
         WHERE tenant_id = $1 AND machine_id = $2
           AND ($3 OR EXISTS (SELECT 1 FROM machines m
                WHERE m.id = machine_id AND m.tenant_id = $1 AND m.owner_user_id = $4))
         ORDER BY name",
    )
    .bind(caller.tenant.as_uuid())
    .bind(machine)
    .bind(caller.role.is_admin())
    .bind(caller.id.as_uuid())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Fields that may be changed on a resource.
#[derive(Debug, Deserialize)]
pub struct UpdateResource {
    /// New display name.
    #[serde(default)]
    pub name: Option<String>,
    /// New description.
    #[serde(default)]
    pub description: Option<String>,
    /// Enable or disable launching.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New window matcher.
    #[serde(default)]
    pub window_match: Option<serde_json::Value>,
    /// Updated executable for an APP resource.
    #[serde(default)]
    pub launch_path: Option<String>,
    /// Updated executable arguments.
    #[serde(default)]
    pub launch_args: Option<Vec<String>>,
    /// Updated working directory.
    #[serde(default)]
    pub working_dir: Option<String>,
}

/// `PATCH /v1/resources/{id}`
pub async fn update(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateResource>,
) -> ApiResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    lock_managed_resource(&mut tx, &caller, id).await?;
    if let Some(path) = &req.launch_path {
        let kind: String = sqlx::query_scalar(
            "SELECT kind FROM published_resources WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id)
        .bind(caller.tenant.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if kind != "APP" || path.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "launch_path must be nonempty and is only valid for APP resources".into(),
            ));
        }
    }
    let result = sqlx::query(
        "UPDATE published_resources SET
           name         = COALESCE($3, name),
           description  = COALESCE($4, description),
           enabled      = COALESCE($5, enabled),
           window_match = COALESCE($6, window_match),
           launch_path  = COALESCE($7, launch_path),
           launch_args  = COALESCE($8, launch_args),
           working_dir  = COALESCE($9, working_dir)
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .bind(req.name.as_deref())
    .bind(req.description.as_deref())
    .bind(req.enabled)
    .bind(req.window_match.as_ref())
    .bind(req.launch_path.as_deref())
    .bind(req.launch_args.as_ref())
    .bind(req.working_dir.as_deref())
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("resource"));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /v1/resources/{id}`
pub async fn delete(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    lock_managed_resource(&mut tx, &caller, id).await?;
    let result = sqlx::query("DELETE FROM published_resources WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(caller.tenant.as_uuid())
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("resource"));
    }
    tx.commit().await?;
    Entry::new("resource.delete", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("resource", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// One entry in a user's own resource list.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct EntitledResource {
    /// Resource identifier.
    pub id: Uuid,
    /// `DESKTOP` or `APP`.
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
    /// Whether the machine is reachable right now.
    pub machine_status: String,
    /// The machine's operating system, so the client can hint at key mapping.
    pub machine_os: Option<String>,
    /// Granted role.
    pub role: String,
    /// Whether clipboard sync is permitted.
    pub allow_clipboard: bool,
    /// Whether file transfer is permitted.
    pub allow_file_transfer: bool,
    /// Whether audio is permitted.
    pub allow_audio: bool,
    /// Display name of the publisher's current owner.
    pub owner_name: Option<String>,
    /// Only a desktop consumer is shown device ownership.
    pub owned: bool,
    /// Device metadata is deliberately absent for APP consumers.
    pub machine_id: Option<Uuid>,
    /// Desktop operating system.
    pub os: Option<String>,
    /// Desktop operating system version.
    pub os_version: Option<String>,
    /// Desktop agent's last contact.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen_at: Option<OffsetDateTime>,
    /// Whether isolated streaming for this resource kind is implemented.
    pub launch_supported: bool,
}

/// Consumer metadata plus the exact role-clamped admission policy.
#[derive(Debug, Serialize)]
pub struct ResourceView {
    /// Backward-compatible resource fields.
    #[serde(flatten)]
    pub resource: EntitledResource,
    /// Authoritative effective policy.
    pub policy: SessionPolicy,
}

impl EntitledResource {
    fn into_view(mut self) -> ApiResult<ResourceView> {
        let (_, policy) = effective_policy(
            &self.role,
            self.allow_clipboard,
            self.allow_file_transfer,
            self.allow_audio,
        )?;
        self.allow_clipboard = policy.clipboard;
        self.allow_file_transfer = policy.file_transfer;
        self.allow_audio = policy.audio;
        Ok(ResourceView {
            resource: self,
            policy,
        })
    }
}

/// How long a machine may go unheard-from before it counts as offline.
///
/// A machine's stored status is whatever its agent last claimed, and an agent
/// that crashes never gets to retract "ONLINE". Its gateway reports the
/// machine as draining when the control tunnel drops, but a gateway can crash
/// too, so liveness is also derived from the clock. Without this, one killed
/// process would leave a machine advertised as reachable indefinitely.
const LIVENESS_GRACE: &str = "90 seconds";

/// The status a machine actually has, as opposed to the one it last claimed.
fn live_status() -> String {
    format!(
        "CASE WHEN m.status = 'ONLINE'
                   AND (m.last_seen_at IS NULL OR
                        m.last_seen_at < now() - interval '{LIVENESS_GRACE}')
              THEN 'OFFLINE' ELSE m.status END"
    )
}

/// Every resource one user may launch, and the terms they may launch it on.
///
/// Kept in one place because both the resource list and the session-creation
/// authorisation check must agree exactly; two subtly different queries here
/// would be a privilege escalation waiting to happen.
///
/// Current machine owners have a full ADMIN session grant, not a directory
/// role. Otherwise choose one complete grant: strongest role, direct before
/// group, then stable id. Never assemble broader permissions from several rows.
fn entitled_resources_sql() -> String {
    format!(
        "
    SELECT r.id, r.kind, r.name, r.description, {} AS machine_status,
           CASE WHEN r.kind = 'DESKTOP' THEN m.os END AS machine_os,
           e.role, e.allow_clipboard, e.allow_file_transfer, e.allow_audio,
           u.display_name AS owner_name,
           (r.kind = 'DESKTOP' AND COALESCE(m.owner_user_id = $2, false)) AS owned,
           CASE WHEN r.kind = 'DESKTOP' THEN m.id END AS machine_id,
           CASE WHEN r.kind = 'DESKTOP' THEN m.os END AS os,
           CASE WHEN r.kind = 'DESKTOP' THEN m.os_version END AS os_version,
           CASE WHEN r.kind = 'DESKTOP' THEN m.last_seen_at END AS last_seen_at,
           (r.kind = 'DESKTOP') AS launch_supported
    FROM published_resources r
    JOIN machines m ON m.id = r.machine_id AND m.tenant_id = r.tenant_id
    LEFT JOIN users u ON u.id = m.owner_user_id AND u.tenant_id = m.tenant_id
    JOIN LATERAL (
        SELECT grants.* FROM (
            SELECT e.id, e.role, e.allow_clipboard, e.allow_file_transfer, e.allow_audio,
                   CASE WHEN e.subject_kind = 'USER' THEN 1 ELSE 2 END AS priority
            FROM entitlements e
            WHERE e.tenant_id = $1 AND e.resource_id = r.id
              AND e.revoked_at IS NULL
              AND (e.expires_at IS NULL OR e.expires_at > now())
              AND ((e.subject_kind = 'USER' AND e.subject_id = $2)
                OR (e.subject_kind = 'GROUP' AND e.subject_id IN (
                    SELECT group_id FROM user_group_members
                    WHERE tenant_id = $1 AND user_id = $2)))
            UNION ALL
            SELECT m.id, 'ADMIN', true, true, true, 0
            WHERE m.owner_user_id = $2
        ) grants
        ORDER BY CASE grants.role WHEN 'ADMIN' THEN 0 WHEN 'CONTROLLER' THEN 1 ELSE 2 END,
                 grants.priority, grants.id
        LIMIT 1
    ) e ON true
    WHERE r.tenant_id = $1 AND r.enabled
",
        live_status()
    )
}

/// `GET /v1/resources`
///
/// The client's home screen: entitled resources, including unsupported APP
/// publications clearly marked as not launchable.
pub async fn list_mine(
    State(state): State<AppState>,
    caller: AuthUser,
) -> ApiResult<Json<Vec<ResourceView>>> {
    let rows = sqlx::query_as::<_, EntitledResource>(&format!(
        "{} ORDER BY r.name",
        entitled_resources_sql()
    ))
    .bind(caller.tenant.as_uuid())
    .bind(caller.id.as_uuid())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(EntitledResource::into_view)
            .collect::<ApiResult<Vec<_>>>()?,
    ))
}

/// `GET /v1/resources/{id}`. Management metadata uses the separate owner API.
pub async fn detail(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ResourceView>> {
    let row = sqlx::query_as::<_, EntitledResource>(&format!(
        "{} AND r.id = $3",
        entitled_resources_sql()
    ))
    .bind(caller.tenant.as_uuid())
    .bind(caller.id.as_uuid())
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound("resource"))?;
    Ok(Json(row.into_view()?))
}

/// The authorisation a user holds over one resource.
#[derive(Debug, sqlx::FromRow)]
pub struct ResolvedGrant {
    /// Unsupported resource kinds must never fall back to desktop streaming.
    pub kind: String,
    /// The machine serving the resource.
    pub machine_id: Uuid,
    /// Its connection status.
    pub machine_status: String,
    /// The agent's Noise static public key.
    pub noise_public_key: Vec<u8>,
    /// Granted role.
    pub role: String,
    /// Clipboard permission.
    pub allow_clipboard: bool,
    /// File transfer permission.
    pub allow_file_transfer: bool,
    /// Audio permission.
    pub allow_audio: bool,
}

impl ResolvedGrant {
    /// The effective policy, clamped to what the role may ever grant.
    ///
    /// Clamping here rather than trusting the entitlement columns means a
    /// misconfigured row — a VIEWER with `allow_clipboard` set — cannot widen
    /// access beyond the role.
    pub fn policy(&self) -> ApiResult<(SessionRole, SessionPolicy)> {
        effective_policy(
            &self.role,
            self.allow_clipboard,
            self.allow_file_transfer,
            self.allow_audio,
        )
    }
}

fn effective_policy(
    role: &str,
    clipboard: bool,
    file_transfer: bool,
    audio: bool,
) -> ApiResult<(SessionRole, SessionPolicy)> {
    let role = SessionRole::from_db(role)
        .map_err(|_| ApiError::Internal(anyhow::anyhow!("bad role in entitlement")))?;
    let ceiling = role.max_policy();
    Ok((
        role,
        SessionPolicy {
            clipboard: clipboard && ceiling.clipboard,
            file_transfer: file_transfer && ceiling.file_transfer,
            audio: audio && ceiling.audio,
            input: ceiling.input,
        },
    ))
}

/// Resolve what a user may do with one specific resource.
pub async fn resolve_grant(
    state: &AppState,
    caller: &AuthUser,
    resource: Uuid,
) -> ApiResult<Option<ResolvedGrant>> {
    let sql = format!(
        "SELECT g.kind, m.id AS machine_id, {} AS machine_status, m.noise_public_key,
                g.role, g.allow_clipboard, g.allow_file_transfer, g.allow_audio
         FROM ({}) g
         JOIN published_resources r ON r.id = g.id
         JOIN machines m ON m.id = r.machine_id
         WHERE g.id = $3",
        live_status(),
        entitled_resources_sql()
    );
    Ok(sqlx::query_as::<_, ResolvedGrant>(&sql)
        .bind(caller.tenant.as_uuid())
        .bind(caller.id.as_uuid())
        .bind(resource)
        .fetch_optional(&state.db)
        .await?)
}

/// Request to grant access to a resource.
#[derive(Debug, Deserialize)]
pub struct CreateEntitlement {
    /// `USER` or `GROUP`.
    #[serde(default)]
    pub subject_kind: Option<String>,
    /// The user or group being granted access.
    #[serde(default)]
    pub subject_id: Option<Uuid>,
    /// Exact recipient email in the caller's tenant, instead of a subject id.
    #[serde(default)]
    pub email: Option<String>,
    /// User-id shorthand, mutually exclusive with all other selectors.
    #[serde(default)]
    pub user_id: Option<Uuid>,
    /// Group-id shorthand, mutually exclusive with all other selectors.
    #[serde(default)]
    pub group_id: Option<Uuid>,
    /// `VIEWER`, `CONTROLLER` or `ADMIN`.
    pub role: String,
    /// Permit clipboard synchronisation.
    ///
    /// Defaults on for the same reason as audio: the role ceiling already
    /// keeps it away from a viewer, and someone who may drive a machine's
    /// keyboard can already move text either way by typing it.
    #[serde(default = "default_true")]
    pub allow_clipboard: bool,
    /// Permit file transfer.
    ///
    /// Defaults off, unlike the others. Copying a file is the one thing
    /// here that leaves something behind, so it is opted into.
    #[serde(default)]
    pub allow_file_transfer: bool,
    /// Permit audio. Defaults on: audio is rarely the sensitive channel.
    #[serde(default = "default_true")]
    pub allow_audio: bool,
    /// Optional expiry, RFC 3339.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

const fn default_true() -> bool {
    true
}

/// An entitlement.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct EntitlementRow {
    /// Identifier.
    pub id: Uuid,
    /// The resource granted.
    pub resource_id: Uuid,
    /// `USER` or `GROUP`.
    pub subject_kind: String,
    /// Subject identifier.
    pub subject_id: Uuid,
    /// User recipient email, visible only to this resource's managers.
    pub user_email: Option<String>,
    /// User recipient display name, if the account still exists.
    pub user_display_name: Option<String>,
    /// Group recipient name, if the group still exists.
    pub group_name: Option<String>,
    /// Granted role.
    pub role: String,
    /// Clipboard permission.
    pub allow_clipboard: bool,
    /// File transfer permission.
    pub allow_file_transfer: bool,
    /// Audio permission.
    pub allow_audio: bool,
    /// Optional expiry.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked, if it was.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

const ENTITLEMENT_SELECT: &str = "
    SELECT e.id, e.resource_id, e.subject_kind, e.subject_id, e.role,
           e.allow_clipboard, e.allow_file_transfer, e.allow_audio,
           e.expires_at, e.revoked_at, e.created_at,
           u.email AS user_email, u.display_name AS user_display_name,
           g.name AS group_name
    FROM entitlements e
    LEFT JOIN users u ON e.subject_kind = 'USER'
        AND u.id = e.subject_id AND u.tenant_id = e.tenant_id
    LEFT JOIN user_groups g ON e.subject_kind = 'GROUP'
        AND g.id = e.subject_id AND g.tenant_id = e.tenant_id
";

/// `POST /v1/resources/{id}/entitlements`
pub async fn grant(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(resource): Path<Uuid>,
    Json(req): Json<CreateEntitlement>,
) -> ApiResult<(StatusCode, Json<EntitlementRow>)> {
    let mut tx = state.db.begin().await?;
    lock_managed_resource(&mut tx, &caller, resource).await?;
    let (subject_kind, subject_id) =
        match (
            &req.email,
            req.user_id,
            req.group_id,
            &req.subject_kind,
            req.subject_id,
        ) {
            (Some(email), None, None, None, None) => {
                let users = sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM users WHERE tenant_id = $1
                 AND lower(email) = lower($2) AND NOT disabled LIMIT 2",
                )
                .bind(caller.tenant.as_uuid())
                .bind(email.trim())
                .fetch_all(&mut *tx)
                .await?;
                let user = match users.as_slice() {
                    [user] => *user,
                    [] => return Err(ApiError::NotFound("recipient")),
                    _ => return Err(ApiError::Conflict("recipient email is ambiguous".into())),
                };
                ("USER".to_string(), user)
            }
            (None, Some(user), None, None, None) => ("USER".to_string(), user),
            (None, None, Some(group), None, None) => ("GROUP".to_string(), group),
            (None, None, None, Some(kind), Some(id)) => (kind.to_ascii_uppercase(), id),
            _ => return Err(ApiError::BadRequest(
                "provide exactly one of email, user_id, group_id, or subject_kind with subject_id"
                    .into(),
            )),
        };
    let subject_query = match subject_kind.as_str() {
        "USER" => {
            "SELECT EXISTS (SELECT 1 FROM users WHERE tenant_id = $1 AND id = $2 AND NOT disabled)"
        }
        "GROUP" => "SELECT EXISTS (SELECT 1 FROM user_groups WHERE tenant_id = $1 AND id = $2)",
        _ => {
            return Err(ApiError::BadRequest(
                "subject_kind must be USER or GROUP".into(),
            ))
        }
    };
    if !sqlx::query_scalar::<_, bool>(subject_query)
        .bind(caller.tenant.as_uuid())
        .bind(subject_id)
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(ApiError::NotFound("recipient"));
    }
    let role = SessionRole::from_db(&req.role.to_ascii_uppercase())
        .map_err(|_| ApiError::BadRequest(format!("unknown role {}", req.role)))?;

    let id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO entitlements
           (id, tenant_id, resource_id, subject_kind, subject_id, role,
            allow_clipboard, allow_file_transfer, allow_audio, expires_at, created_by)
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11
         WHERE EXISTS (
             SELECT 1 FROM published_resources WHERE id = $3 AND tenant_id = $2)
         ON CONFLICT (resource_id, subject_kind, subject_id) DO UPDATE SET
           role                = EXCLUDED.role,
           allow_clipboard     = EXCLUDED.allow_clipboard,
           allow_file_transfer = EXCLUDED.allow_file_transfer,
           allow_audio         = EXCLUDED.allow_audio,
           expires_at          = EXCLUDED.expires_at,
           revoked_at          = NULL
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(caller.tenant.as_uuid())
    .bind(resource)
    .bind(&subject_kind)
    .bind(subject_id)
    .bind(role.as_db())
    .bind(req.allow_clipboard)
    .bind(req.allow_file_transfer)
    .bind(req.allow_audio)
    .bind(req.expires_at)
    .bind(caller.id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound("resource"))?;
    let row = sqlx::query_as::<_, EntitlementRow>(&format!(
        "{ENTITLEMENT_SELECT} WHERE e.tenant_id = $1 AND e.id = $2"
    ))
    .bind(caller.tenant.as_uuid())
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Entry::new("entitlement.grant", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("entitlement", row.id)
        .detail(serde_json::json!({
            "resource_id": resource,
            "subject_kind": subject_kind,
            "subject_id": subject_id,
            "role": role.as_db(),
        }))
        .write(&state.db)
        .await;

    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /v1/resources/{id}/entitlements`
pub async fn list_grants(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(resource): Path<Uuid>,
) -> ApiResult<Json<Vec<EntitlementRow>>> {
    let rows = sqlx::query_as::<_, EntitlementRow>(&format!(
        "{ENTITLEMENT_SELECT}
         WHERE e.tenant_id = $1 AND e.resource_id = $2
           AND EXISTS (
               SELECT 1 FROM published_resources r
               JOIN machines m ON m.id = r.machine_id AND m.tenant_id = r.tenant_id
               WHERE r.id = e.resource_id AND r.tenant_id = $1
                 AND ($3 OR m.owner_user_id = $4))
         ORDER BY e.created_at"
    ))
    .bind(caller.tenant.as_uuid())
    .bind(resource)
    .bind(caller.role.is_admin())
    .bind(caller.id.as_uuid())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// `DELETE /v1/entitlements/{id}`
///
/// Revokes rather than deletes: the audit trail should still show who had
/// access and when it was taken away.
pub async fn revoke(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let resource = sqlx::query_scalar::<_, Uuid>(
        "SELECT resource_id FROM entitlements WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound("entitlement"))?;
    lock_managed_resource(&mut tx, &caller, resource).await?;
    let result = sqlx::query(
        "UPDATE entitlements SET revoked_at = now()
         WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("entitlement"));
    }
    tx.commit().await?;
    Entry::new("entitlement.revoke", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("entitlement", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant_with(role: &str, clipboard: bool, files: bool) -> ResolvedGrant {
        ResolvedGrant {
            kind: "DESKTOP".into(),
            machine_id: Uuid::now_v7(),
            machine_status: "ONLINE".into(),
            noise_public_key: vec![0; 32],
            role: role.into(),
            allow_clipboard: clipboard,
            allow_file_transfer: files,
            allow_audio: true,
        }
    }

    #[test]
    fn a_viewer_never_gets_input_or_clipboard() {
        // Even if the entitlement row says otherwise: the role is the ceiling.
        let (role, policy) = grant_with("VIEWER", true, true).policy().unwrap();
        assert_eq!(role, SessionRole::Viewer);
        assert!(!policy.input);
        assert!(!policy.clipboard);
        assert!(!policy.file_transfer);
        assert!(!policy.audio);
    }

    #[test]
    fn a_controller_gets_exactly_what_the_entitlement_allows() {
        let (_, policy) = grant_with("CONTROLLER", true, false).policy().unwrap();
        assert!(policy.input);
        assert!(policy.clipboard);
        assert!(!policy.file_transfer, "entitlement must be able to narrow");
        assert!(policy.audio);
    }

    #[test]
    fn a_corrupt_role_is_an_internal_error_not_a_silent_grant() {
        assert!(grant_with("SUPERUSER", true, true).policy().is_err());
    }
}
