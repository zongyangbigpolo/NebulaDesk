//! Tenant and directory endpoints.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use nebula_common::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{Entry, Outcome};
use crate::auth::extract::{AuthUser, BootstrapAuth, UserRole};
use crate::auth::password;
use crate::error::{ApiError, ApiResult};
use crate::routes::auth::{parse_role, UserView};
use crate::state::AppState;

/// Request to create a tenant along with its first owner.
#[derive(Debug, Deserialize)]
pub struct CreateTenant {
    /// Human-readable name.
    pub name: String,
    /// URL-safe identifier, used at login.
    pub slug: String,
    /// The owner's email address.
    pub owner_email: String,
    /// The owner's password.
    pub owner_password: String,
    /// The owner's display name.
    pub owner_display_name: String,
}

/// A tenant with its owner.
#[derive(Debug, Serialize)]
pub struct TenantCreated {
    /// The new tenant.
    pub id: TenantId,
    /// Its slug.
    pub slug: String,
    /// The owner account created alongside it.
    pub owner: UserView,
}

/// `POST /v1/tenants`
///
/// Guarded by the deployment-wide bootstrap token: there is no tenant to
/// authenticate against yet, and self-service tenant creation would let
/// anyone consume the deployment's resources.
pub async fn create_tenant(
    State(state): State<AppState>,
    _auth: BootstrapAuth,
    Json(req): Json<CreateTenant>,
) -> ApiResult<(StatusCode, Json<TenantCreated>)> {
    validate_slug(&req.slug)?;
    let hash = password::hash(&req.owner_password)?;

    let tenant = TenantId::new();
    let owner = UserId::new();
    let mut tx = state.db.begin().await?;

    sqlx::query("INSERT INTO tenants (id, name, slug) VALUES ($1, $2, $3)")
        .bind(tenant.as_uuid())
        .bind(&req.name)
        .bind(&req.slug)
        .execute(&mut *tx)
        .await
        .map_err(|e| conflict(e, "a tenant with that slug already exists"))?;

    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, display_name, role)
         VALUES ($1, $2, $3, $4, $5, 'OWNER')",
    )
    .bind(owner.as_uuid())
    .bind(tenant.as_uuid())
    .bind(&req.owner_email)
    .bind(&hash)
    .bind(&req.owner_display_name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Entry::new("tenant.create", Outcome::Allow)
        .tenant(tenant)
        .target("tenant", tenant.as_uuid())
        .write(&state.db)
        .await;

    Ok((
        StatusCode::CREATED,
        Json(TenantCreated {
            id: tenant,
            slug: req.slug,
            owner: UserView {
                id: owner,
                tenant_id: tenant,
                email: req.owner_email,
                display_name: req.owner_display_name,
                role: "OWNER".into(),
            },
        }),
    ))
}

/// Request to create a user inside the caller's tenant.
#[derive(Debug, Deserialize)]
pub struct CreateUser {
    /// Email address, unique within the tenant.
    pub email: String,
    /// Initial password.
    pub password: String,
    /// Display name.
    pub display_name: String,
    /// Directory role; defaults to `USER`.
    #[serde(default)]
    pub role: Option<String>,
}

/// `POST /v1/users`
pub async fn create_user(
    State(state): State<AppState>,
    caller: AuthUser,
    Json(req): Json<CreateUser>,
) -> ApiResult<(StatusCode, Json<UserView>)> {
    caller.require_admin()?;
    let role = match req.role.as_deref() {
        None => UserRole::User,
        Some(r) => parse_role(r)?,
    };
    // Only an owner may mint another owner; otherwise an admin could promote
    // themselves sideways into full control of the tenant.
    if role == UserRole::Owner {
        caller.require_owner()?;
    }

    let hash = password::hash(&req.password)?;
    let id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, email, password_hash, display_name, role)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id.as_uuid())
    .bind(caller.tenant.as_uuid())
    .bind(&req.email)
    .bind(&hash)
    .bind(&req.display_name)
    .bind(role.as_db())
    .execute(&state.db)
    .await
    .map_err(|e| conflict(e, "a user with that email already exists"))?;

    Entry::new("user.create", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("user", id.as_uuid())
        .write(&state.db)
        .await;

    Ok((
        StatusCode::CREATED,
        Json(UserView {
            id,
            tenant_id: caller.tenant,
            email: req.email,
            display_name: req.display_name,
            role: role.as_db().into(),
        }),
    ))
}

/// A directory listing entry.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserRow {
    /// Identifier.
    pub id: Uuid,
    /// Email address.
    pub email: String,
    /// Display name.
    pub display_name: String,
    /// Directory role.
    pub role: String,
    /// Whether the account is suspended.
    pub disabled: bool,
    /// When the account was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// `GET /v1/users`
pub async fn list_users(
    State(state): State<AppState>,
    caller: AuthUser,
) -> ApiResult<Json<Vec<UserRow>>> {
    caller.require_admin()?;
    let rows = sqlx::query_as::<_, UserRow>(
        "SELECT id, email, display_name, role, disabled, created_at
         FROM users WHERE tenant_id = $1 ORDER BY created_at",
    )
    .bind(caller.tenant.as_uuid())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Fields that may be changed on a user.
#[derive(Debug, Deserialize)]
pub struct UpdateUser {
    /// New display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// New directory role.
    #[serde(default)]
    pub role: Option<String>,
    /// Suspend or reinstate the account.
    #[serde(default)]
    pub disabled: Option<bool>,
    /// Replace the password.
    #[serde(default)]
    pub password: Option<String>,
}

/// `PATCH /v1/users/{id}`
pub async fn update_user(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateUser>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    if let Some(role) = req.role.as_deref() {
        if parse_role(role)? == UserRole::Owner {
            caller.require_owner()?;
        }
    }
    // Locking yourself out is a support ticket, not a feature.
    if id == caller.id.as_uuid() && req.disabled == Some(true) {
        return Err(ApiError::BadRequest(
            "you cannot disable your own account".into(),
        ));
    }

    let hash = match req.password.as_deref() {
        Some(p) => Some(password::hash(p)?),
        None => None,
    };

    let result = sqlx::query(
        "UPDATE users SET
           display_name  = COALESCE($3, display_name),
           role          = COALESCE($4, role),
           disabled      = COALESCE($5, disabled),
           password_hash = COALESCE($6, password_hash)
         WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(caller.tenant.as_uuid())
    .bind(req.display_name.as_deref())
    .bind(req.role.as_deref())
    .bind(req.disabled)
    .bind(hash.as_deref())
    .execute(&state.db)
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("user"));
    }

    // Changing a password or suspending an account must end existing sessions
    // immediately; leaving refresh tokens alive would defeat the point.
    if req.password.is_some() || req.disabled == Some(true) {
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = now()
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .execute(&state.db)
        .await?;
    }

    Entry::new("user.update", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("user", id)
        .detail(serde_json::json!({
            "role": req.role,
            "disabled": req.disabled,
            "password_changed": req.password.is_some(),
        }))
        .write(&state.db)
        .await;

    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /v1/users/{id}`
pub async fn delete_user(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    if id == caller.id.as_uuid() {
        return Err(ApiError::BadRequest(
            "you cannot delete your own account".into(),
        ));
    }
    let result = sqlx::query("DELETE FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(caller.tenant.as_uuid())
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("user"));
    }
    Entry::new("user.delete", Outcome::Allow)
        .tenant(caller.tenant)
        .actor(caller.id)
        .target("user", id)
        .write(&state.db)
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Request to create a group.
#[derive(Debug, Deserialize)]
pub struct CreateGroup {
    /// Group name, unique within the tenant.
    pub name: String,
}

/// A group.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct GroupRow {
    /// Identifier.
    pub id: Uuid,
    /// Name.
    pub name: String,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// `POST /v1/groups`
pub async fn create_group(
    State(state): State<AppState>,
    caller: AuthUser,
    Json(req): Json<CreateGroup>,
) -> ApiResult<(StatusCode, Json<GroupRow>)> {
    caller.require_admin()?;
    let row = sqlx::query_as::<_, GroupRow>(
        "INSERT INTO user_groups (id, tenant_id, name) VALUES ($1, $2, $3)
         RETURNING id, name, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(caller.tenant.as_uuid())
    .bind(&req.name)
    .fetch_one(&state.db)
    .await
    .map_err(|e| conflict(e, "a group with that name already exists"))?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /v1/groups`
pub async fn list_groups(
    State(state): State<AppState>,
    caller: AuthUser,
) -> ApiResult<Json<Vec<GroupRow>>> {
    caller.require_admin()?;
    let rows = sqlx::query_as::<_, GroupRow>(
        "SELECT id, name, created_at FROM user_groups WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(caller.tenant.as_uuid())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Request to add a member to a group.
#[derive(Debug, Deserialize)]
pub struct AddMember {
    /// The user to add.
    pub user_id: Uuid,
}

/// `POST /v1/groups/{id}/members`
pub async fn add_group_member(
    State(state): State<AppState>,
    caller: AuthUser,
    Path(group): Path<Uuid>,
    Json(req): Json<AddMember>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    // The composite foreign keys make a cross-tenant membership impossible,
    // so a violation here means one of the two ids is not ours.
    sqlx::query(
        "INSERT INTO user_group_members (tenant_id, group_id, user_id)
         VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(caller.tenant.as_uuid())
    .bind(group)
    .bind(req.user_id)
    .execute(&state.db)
    .await
    .map_err(|_| ApiError::NotFound("group or user"))?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /v1/groups/{id}/members/{user_id}`
pub async fn remove_group_member(
    State(state): State<AppState>,
    caller: AuthUser,
    Path((group, user)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    caller.require_admin()?;
    sqlx::query(
        "DELETE FROM user_group_members
         WHERE tenant_id = $1 AND group_id = $2 AND user_id = $3",
    )
    .bind(caller.tenant.as_uuid())
    .bind(group)
    .bind(user)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_slug(slug: &str) -> ApiResult<()> {
    let ok = !slug.is_empty()
        && slug.len() <= 63
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "slug must be lowercase letters, digits and dashes".into(),
        ))
    }
}

fn conflict(e: sqlx::Error, message: &str) -> ApiError {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            ApiError::Conflict(message.to_string())
        }
        _ => ApiError::Database(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_must_be_url_safe() {
        assert!(validate_slug("acme").is_ok());
        assert!(validate_slug("acme-corp-2").is_ok());
        assert!(validate_slug("Acme").is_err());
        assert!(validate_slug("acme corp").is_err());
        assert!(validate_slug("-acme").is_err());
        assert!(validate_slug("acme-").is_err());
        assert!(validate_slug("").is_err());
        assert!(validate_slug(&"a".repeat(64)).is_err());
        // A slug is used in login lookups; path traversal shapes must not pass.
        assert!(validate_slug("../etc").is_err());
    }
}
