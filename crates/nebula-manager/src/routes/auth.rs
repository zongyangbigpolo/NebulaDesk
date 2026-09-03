//! Authentication endpoints.

use axum::extract::State;
use axum::Json;
use nebula_common::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{Entry, Outcome};
use crate::auth::extract::{AuthUser, UserRole};
use crate::auth::{password, tokens};
use crate::error::{ApiError, ApiResult};
use crate::routes::ClientIp;
use crate::state::AppState;

/// Credentials presented at login.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    /// The tenant's slug. Users are scoped to a tenant, so the same email may
    /// exist in several of them.
    pub tenant: String,
    /// The user's email address.
    pub email: String,
    /// The user's password.
    pub password: String,
}

/// A freshly issued token pair.
#[derive(Debug, Serialize)]
pub struct TokenPair {
    /// Short-lived bearer token for API calls.
    pub access_token: String,
    /// Long-lived opaque token used to obtain a new pair.
    pub refresh_token: String,
    /// Seconds until `access_token` expires.
    pub expires_in: i64,
    /// The authenticated user.
    pub user: UserView,
}

/// A user as returned by the API. Never carries the password hash.
#[derive(Debug, Serialize)]
pub struct UserView {
    /// Identifier.
    pub id: UserId,
    /// Tenant.
    pub tenant_id: TenantId,
    /// Email address.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// Directory role.
    pub role: String,
}

/// `POST /v1/auth/login`
pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Json<TokenPair>> {
    let row: Option<(Uuid, Uuid, String, String, String, String, bool)> = sqlx::query_as(
        "SELECT u.id, u.tenant_id, u.email, u.display_name, u.role, u.password_hash, u.disabled
         FROM users u
         JOIN tenants t ON t.id = u.tenant_id
         WHERE t.slug = $1 AND lower(u.email) = lower($2) AND NOT t.disabled",
    )
    .bind(&req.tenant)
    .bind(&req.email)
    .fetch_optional(&state.db)
    .await?;

    let Some((id, tenant, email, display_name, role, hash, disabled)) = row else {
        // Spend the same time as a real verification so a missing account is
        // indistinguishable from a wrong password.
        password::verify_dummy(&req.password);
        Entry::new("auth.login", Outcome::Deny)
            .detail(serde_json::json!({ "tenant": req.tenant, "reason": "no_such_user" }))
            .ip(ip.clone())
            .write(&state.db)
            .await;
        return Err(ApiError::Unauthorized);
    };

    let ok = password::verify(&req.password, &hash);
    if !ok || disabled {
        Entry::new("auth.login", Outcome::Deny)
            .tenant(TenantId::from_uuid(tenant))
            .actor(UserId::from_uuid(id))
            .detail(serde_json::json!({
                "reason": if disabled { "disabled" } else { "bad_password" }
            }))
            .ip(ip)
            .write(&state.db)
            .await;
        return Err(ApiError::Unauthorized);
    }

    let user = UserId::from_uuid(id);
    let tenant = TenantId::from_uuid(tenant);
    let pair = issue_pair(
        &state,
        user,
        tenant,
        &role,
        UserView {
            id: user,
            tenant_id: tenant,
            email,
            display_name,
            role: role.clone(),
        },
    )
    .await?;

    Entry::new("auth.login", Outcome::Allow)
        .tenant(tenant)
        .actor(user)
        .ip(ip)
        .write(&state.db)
        .await;

    Ok(Json(pair))
}

/// A refresh request.
#[derive(Debug, Deserialize)]
pub struct RefreshRequest {
    /// The opaque refresh token previously issued.
    pub refresh_token: String,
}

/// `POST /v1/auth/refresh`
///
/// Rotates the refresh token: the presented one is revoked and a new one
/// issued. Reuse of a revoked token is therefore visible, which is the only
/// practical way to detect a stolen refresh token.
pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> ApiResult<Json<TokenPair>> {
    let hash = tokens::hash_token(&req.refresh_token);
    let mut tx = state.db.begin().await?;

    // Revoke and read in one statement so two concurrent refreshes cannot
    // both succeed with the same token.
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "UPDATE refresh_tokens
         SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()
         RETURNING id, user_id, tenant_id",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((_, user_id, tenant_id)) = row else {
        return Err(ApiError::Unauthorized);
    };

    let user: Option<(String, String, String, bool)> =
        sqlx::query_as("SELECT email, display_name, role, disabled FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((email, display_name, role, disabled)) = user else {
        return Err(ApiError::Unauthorized);
    };
    if disabled {
        // The token was still valid, but the account is not. Committing the
        // revocation is the right outcome.
        tx.commit().await?;
        return Err(ApiError::Unauthorized);
    }

    let user = UserId::from_uuid(user_id);
    let tenant = TenantId::from_uuid(tenant_id);
    let (refresh_token, expires_at) = new_refresh(&state);
    sqlx::query(
        "INSERT INTO refresh_tokens (id, tenant_id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id)
    .bind(user_id)
    .bind(&refresh_token.hash)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(Json(TokenPair {
        access_token: state.access_tokens.issue(user, tenant, &role)?,
        refresh_token: refresh_token.secret,
        expires_in: state.access_tokens.ttl_secs(),
        user: UserView {
            id: user,
            tenant_id: tenant,
            email,
            display_name,
            role,
        },
    }))
}

/// `POST /v1/auth/logout`
pub async fn logout(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> ApiResult<axum::http::StatusCode> {
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(tokens::hash_token(&req.refresh_token))
    .execute(&state.db)
    .await?;
    // Always 204: telling the caller whether the token existed would let an
    // attacker probe for valid tokens.
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// `GET /v1/auth/me`
pub async fn me(State(state): State<AppState>, user: AuthUser) -> ApiResult<Json<UserView>> {
    let row: (String, String, String) =
        sqlx::query_as("SELECT email, display_name, role FROM users WHERE id = $1")
            .bind(user.id.as_uuid())
            .fetch_one(&state.db)
            .await?;
    Ok(Json(UserView {
        id: user.id,
        tenant_id: user.tenant,
        email: row.0,
        display_name: row.1,
        role: row.2,
    }))
}

/// Issue an access/refresh pair and persist the refresh token.
pub(crate) async fn issue_pair(
    state: &AppState,
    user: UserId,
    tenant: TenantId,
    role: &str,
    view: UserView,
) -> ApiResult<TokenPair> {
    let (refresh_token, expires_at) = new_refresh(state);
    sqlx::query(
        "INSERT INTO refresh_tokens (id, tenant_id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.as_uuid())
    .bind(user.as_uuid())
    .bind(&refresh_token.hash)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    Ok(TokenPair {
        access_token: state.access_tokens.issue(user, tenant, role)?,
        refresh_token: refresh_token.secret,
        expires_in: state.access_tokens.ttl_secs(),
        user: view,
    })
}

fn new_refresh(state: &AppState) -> (tokens::OpaqueToken, OffsetDateTime) {
    let ttl = time::Duration::try_from(state.config.refresh_token_ttl)
        .unwrap_or(time::Duration::days(30));
    (tokens::generate_opaque(), OffsetDateTime::now_utc() + ttl)
}

/// Parse a directory role from user input.
pub(crate) fn parse_role(s: &str) -> ApiResult<UserRole> {
    UserRole::from_db(s).ok_or_else(|| ApiError::BadRequest(format!("unknown role {s}")))
}
