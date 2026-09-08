//! Current-workspace metadata, opt-in signup and organization invitations.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nebula_common::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{Entry, Outcome};
use crate::auth::extract::AuthUser;
use crate::auth::{password, tokens};
use crate::error::{ApiError, ApiResult};
use crate::routes::auth::{new_refresh, TokenPair, UserView};
use crate::routes::directory::validate_slug;
use crate::state::AppState;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkspaceKind {
    Personal,
    Organization,
}

impl WorkspaceKind {
    fn as_db(&self) -> &'static str {
        match self {
            Self::Personal => "PERSONAL",
            Self::Organization => "ORGANIZATION",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Workspace {
    pub id: TenantId,
    pub slug: String,
    pub name: String,
    pub kind: WorkspaceKind,
}

#[derive(Serialize)]
pub struct Registration {
    pub self_registration_enabled: bool,
}

pub async fn registration(State(state): State<AppState>) -> Json<Registration> {
    Json(Registration {
        self_registration_enabled: state.config.allow_self_registration,
    })
}

pub async fn current(
    State(state): State<AppState>,
    caller: AuthUser,
) -> ApiResult<Json<Workspace>> {
    Ok(Json(
        workspace(&mut *state.db.acquire().await?, caller.tenant).await?,
    ))
}

async fn workspace(db: &mut PgConnection, id: TenantId) -> ApiResult<Workspace> {
    let (slug, name, kind): (String, String, String) =
        sqlx::query_as("SELECT slug, name, kind FROM tenants WHERE id = $1 AND NOT disabled")
            .bind(id.as_uuid())
            .fetch_optional(db)
            .await?
            .ok_or(ApiError::Unauthorized)?;
    Ok(Workspace {
        id,
        slug,
        name,
        kind: match kind.as_str() {
            "PERSONAL" => WorkspaceKind::Personal,
            "ORGANIZATION" => WorkspaceKind::Organization,
            _ => {
                return Err(ApiError::Internal(anyhow::anyhow!(
                    "invalid workspace kind"
                )))
            }
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Register {
    pub workspace_slug: String,
    pub workspace_name: String,
    pub workspace_kind: WorkspaceKind,
    pub display_name: String,
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct Registered {
    #[serde(flatten)]
    pub tokens: TokenPair,
    pub workspace: Workspace,
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<Register>,
) -> ApiResult<(StatusCode, Json<Registered>)> {
    if !state.config.allow_self_registration {
        return Err(ApiError::Forbidden("self registration is disabled".into()));
    }
    validate_slug(&req.workspace_slug)?;
    validate_name(&req.workspace_name, "workspace_name")?;
    validate_name(&req.display_name, "display_name")?;
    let email = normalize_email(&req.email)?;
    let hash = password::hash(&req.password)?;
    let tenant = TenantId::new();
    let user = UserId::new();
    let mut tx = state.db.begin().await?;
    sqlx::query("INSERT INTO tenants (id, slug, name, kind) VALUES ($1, $2, $3, $4)")
        .bind(tenant.as_uuid())
        .bind(&req.workspace_slug)
        .bind(&req.workspace_name)
        .bind(req.workspace_kind.as_db())
        .execute(&mut *tx)
        .await?;
    let view = insert_user(
        &mut tx,
        user,
        tenant,
        email,
        req.display_name,
        hash,
        "ADMIN",
    )
    .await?;
    let tokens = issue_pair(&state, &mut tx, view).await?;
    let workspace = Workspace {
        id: tenant,
        slug: req.workspace_slug,
        name: req.workspace_name,
        kind: req.workspace_kind,
    };
    tx.commit().await?;
    Entry::new("auth.register", Outcome::Allow)
        .tenant(tenant)
        .actor(user)
        .write(&state.db)
        .await;
    Ok((StatusCode::CREATED, Json(Registered { tokens, workspace })))
}

fn validate_name(name: &str, field: &str) -> ApiResult<()> {
    if name.trim().is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(format!(
            "{field} must contain 1–200 bytes of text"
        )));
    }
    Ok(())
}

fn normalize_email(email: &str) -> ApiResult<String> {
    if email.len() > 254 {
        return Err(ApiError::BadRequest("invalid email address".into()));
    }
    let email = email.trim().to_ascii_lowercase();
    let valid = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    }) && !email.chars().any(|c| c.is_whitespace() || c.is_control());
    if !valid {
        return Err(ApiError::BadRequest("invalid email address".into()));
    }
    Ok(email)
}

async fn insert_user(
    tx: &mut Transaction<'_, Postgres>,
    id: UserId,
    tenant: TenantId,
    email: String,
    display_name: String,
    hash: String,
    role: &str,
) -> ApiResult<UserView> {
    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, display_name, role)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.as_uuid())
    .bind(tenant.as_uuid())
    .bind(&email)
    .bind(hash)
    .bind(&display_name)
    .bind(role)
    .execute(&mut **tx)
    .await?;
    Ok(UserView {
        id,
        tenant_id: tenant,
        email,
        display_name,
        role: role.into(),
    })
}

async fn issue_pair(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: UserView,
) -> ApiResult<TokenPair> {
    let access_token = state
        .access_tokens
        .issue(user.id, user.tenant_id, &user.role)?;
    let (refresh_token, expires_at) = new_refresh(state);
    sqlx::query(
        "INSERT INTO refresh_tokens (id, tenant_id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(user.tenant_id.as_uuid())
    .bind(user.id.as_uuid())
    .bind(refresh_token.hash)
    .bind(expires_at)
    .execute(&mut **tx)
    .await?;
    Ok(TokenPair {
        access_token,
        refresh_token: refresh_token.secret,
        expires_in: state.access_tokens.ttl_secs(),
        user,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invite {
    pub email: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Invitation {
    pub id: Uuid,
    pub email: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub accepted_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
pub struct InvitationCreated {
    #[serde(flatten)]
    pub invitation: Invitation,
    pub token: String,
}

async fn organization_admin(
    db: &mut PgConnection,
    tenant: TenantId,
    user: UserId,
) -> ApiResult<()> {
    // Shared locks serialize redemption/creation against issuer suspension,
    // demotion and workspace suspension until the transaction commits.
    let allowed: Option<(Uuid,)> = sqlx::query_as(
        "SELECT u.id FROM users u JOIN tenants t ON t.id = u.tenant_id
         WHERE u.id = $1 AND t.id = $2 AND NOT u.disabled AND NOT t.disabled
           AND u.role IN ('ADMIN', 'OWNER') AND t.kind = 'ORGANIZATION'
         FOR SHARE OF u, t",
    )
    .bind(user.as_uuid())
    .bind(tenant.as_uuid())
    .fetch_optional(db)
    .await?;
    allowed.ok_or_else(|| ApiError::Forbidden("organization administrator required".into()))?;
    Ok(())
}

pub async fn create_invitation(
    State(state): State<AppState>,
    caller: AuthUser,
    Json(req): Json<Invite>,
) -> ApiResult<(StatusCode, Json<InvitationCreated>)> {
    caller.require_admin()?;
    let email = normalize_email(&req.email)?;
    let token = tokens::generate_opaque();
    let mut tx = state.db.begin().await?;
    organization_admin(&mut tx, caller.tenant, caller.id).await?;
    let invitation = sqlx::query_as::<_, Invitation>(
        "INSERT INTO workspace_invitations (id, tenant_id, issuer_id, email, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5, clock_timestamp() + interval '48 hours')
         RETURNING id, email, expires_at, created_at, revoked_at, accepted_at",
    )
    .bind(Uuid::now_v7())
    .bind(caller.tenant.as_uuid())
    .bind(caller.id.as_uuid())
    .bind(email)
    .bind(token.hash)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Entry::new("workspace.invitation.create", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("invitation", invitation.id)
        .write(&state.db)
        .await;
    Ok((
        StatusCode::CREATED,
        Json(InvitationCreated {
            invitation,
            token: token.secret,
        }),
    ))
}

pub async fn list_invitations(
    State(state): State<AppState>,
    caller: AuthUser,
) -> ApiResult<Json<Vec<Invitation>>> {
    caller.require_admin()?;
    let mut tx = state.db.begin().await?;
    organization_admin(&mut tx, caller.tenant, caller.id).await?;
    let invitations = sqlx::query_as::<_, Invitation>(
        "SELECT id, email, expires_at, created_at, revoked_at, accepted_at
         FROM workspace_invitations WHERE tenant_id = $1 ORDER BY created_at DESC, id DESC",
    )
    .bind(caller.tenant.as_uuid())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(invitations))
}

pub async fn revoke_invitation(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    let mut tx = state.db.begin().await?;
    organization_admin(&mut tx, caller.tenant, caller.id).await?;
    let result = sqlx::query(
        "UPDATE workspace_invitations SET revoked_at = COALESCE(revoked_at, clock_timestamp())
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("invitation"));
    }
    tx.commit().await?;
    Entry::new("workspace.invitation.revoke", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("invitation", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptInvitation {
    pub token: String,
    pub display_name: String,
    pub email: String,
    pub password: String,
}

#[derive(sqlx::FromRow)]
struct Redeemable {
    id: Uuid,
    tenant_id: Uuid,
    issuer_id: Uuid,
    email: String,
    expires_at: OffsetDateTime,
    revoked_at: Option<OffsetDateTime>,
    accepted_at: Option<OffsetDateTime>,
}

pub async fn accept_invitation(
    State(state): State<AppState>,
    Json(req): Json<AcceptInvitation>,
) -> ApiResult<(StatusCode, Json<Registered>)> {
    if req.token.len() > 128 {
        return Err(ApiError::Unauthorized);
    }
    validate_name(&req.display_name, "display_name")?;
    let email = normalize_email(&req.email)?;
    // Bound all payload fields before spending Argon2 work or holding locks.
    let hash = password::hash(&req.password)?;
    let mut tx = state.db.begin().await?;
    let invitation = sqlx::query_as::<_, Redeemable>(
        "SELECT id, tenant_id, issuer_id, email, expires_at, revoked_at, accepted_at
         FROM workspace_invitations WHERE token_hash = $1 FOR UPDATE",
    )
    .bind(tokens::hash_token(&req.token))
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::Unauthorized)?;
    if invitation.email != email
        || invitation.revoked_at.is_some()
        || invitation.accepted_at.is_some()
        || invitation.expires_at <= OffsetDateTime::now_utc()
    {
        return Err(ApiError::Unauthorized);
    }
    let tenant = TenantId::from_uuid(invitation.tenant_id);
    match organization_admin(&mut tx, tenant, UserId::from_uuid(invitation.issuer_id)).await {
        Err(ApiError::Forbidden(_)) => return Err(ApiError::Unauthorized),
        result => result?,
    }
    let user = UserId::new();
    let view = insert_user(&mut tx, user, tenant, email, req.display_name, hash, "USER").await?;
    let tokens = issue_pair(&state, &mut tx, view).await?;
    let workspace = workspace(&mut tx, tenant).await?;
    let consumed = sqlx::query(
        "UPDATE workspace_invitations SET accepted_at = clock_timestamp()
         WHERE id = $1 AND expires_at > clock_timestamp()
           AND revoked_at IS NULL AND accepted_at IS NULL",
    )
    .bind(invitation.id)
    .execute(&mut *tx)
    .await?;
    if consumed.rows_affected() == 0 {
        return Err(ApiError::Unauthorized);
    }
    tx.commit().await?;
    Entry::new("workspace.invitation.accept", Outcome::Allow)
        .tenant(tenant)
        .actor(user)
        .target("invitation", invitation.id)
        .write(&state.db)
        .await;
    Ok((StatusCode::CREATED, Json(Registered { tokens, workspace })))
}
