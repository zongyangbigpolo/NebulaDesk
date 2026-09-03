//! Request extractors that turn credentials into authenticated principals.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use nebula_common::{MachineId, TenantId, UserId};

use crate::error::ApiError;
use crate::state::AppState;

use super::tokens::parse_machine_credential;

/// A user's directory role, which governs control-plane permissions.
///
/// Distinct from `SessionRole`, which governs what a *session* may do to a
/// machine. An ADMIN may publish resources without being entitled to use any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserRole {
    /// Ordinary user: may list and launch what they are entitled to.
    User,
    /// May manage users, machines, resources and entitlements.
    Admin,
    /// The tenant's owner. Everything an admin can do, plus tenant settings.
    Owner,
}

impl UserRole {
    /// Parse the database representation.
    #[must_use]
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "USER" => Some(Self::User),
            "ADMIN" => Some(Self::Admin),
            "OWNER" => Some(Self::Owner),
            _ => None,
        }
    }

    /// The database representation.
    #[must_use]
    pub const fn as_db(self) -> &'static str {
        match self {
            UserRole::User => "USER",
            UserRole::Admin => "ADMIN",
            UserRole::Owner => "OWNER",
        }
    }

    /// Whether this role may administer the tenant.
    #[must_use]
    pub const fn is_admin(self) -> bool {
        matches!(self, UserRole::Admin | UserRole::Owner)
    }
}

/// An authenticated user.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    /// Who they are.
    pub id: UserId,
    /// Which tenant they belong to. Every query must filter on this.
    pub tenant: TenantId,
    /// What they may do.
    pub role: UserRole,
}

impl AuthUser {
    /// Fail unless the user may administer their tenant.
    pub fn require_admin(&self) -> Result<(), ApiError> {
        if self.role.is_admin() {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "this action requires an administrator".into(),
            ))
        }
    }

    /// Fail unless the user owns the tenant.
    pub fn require_owner(&self) -> Result<(), ApiError> {
        if self.role == UserRole::Owner {
            Ok(())
        } else {
            Err(ApiError::Forbidden(
                "this action requires the tenant owner".into(),
            ))
        }
    }
}

fn bearer<'a>(parts: &'a Parts, scheme: &str) -> Option<&'a str> {
    let raw = parts
        .headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (got, value) = raw.split_once(' ')?;
    got.eq_ignore_ascii_case(scheme).then(|| value.trim())
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer(parts, "Bearer").ok_or(ApiError::Unauthorized)?;
        let claims = state.access_tokens.verify(token)?;
        let id = claims.user_id()?;
        let tenant = claims.tenant_id()?;

        // The token asserts a role, but a role change or a disabled account
        // must take effect immediately rather than at the next token refresh,
        // so the live row is authoritative.
        let row: Option<(String, bool)> =
            sqlx::query_as("SELECT role, disabled FROM users WHERE id = $1 AND tenant_id = $2")
                .bind(id.as_uuid())
                .bind(tenant.as_uuid())
                .fetch_optional(&state.db)
                .await?;

        let (role, disabled) = row.ok_or(ApiError::Unauthorized)?;
        if disabled {
            return Err(ApiError::Unauthorized);
        }
        let role = UserRole::from_db(&role).ok_or(ApiError::Unauthorized)?;
        Ok(Self { id, tenant, role })
    }
}

/// An authenticated agent process.
#[derive(Debug, Clone, Copy)]
pub struct AuthMachine {
    /// The machine's id.
    pub id: MachineId,
    /// Its tenant.
    pub tenant: TenantId,
}

impl FromRequestParts<AppState> for AuthMachine {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let raw = bearer(parts, "Machine").ok_or(ApiError::Unauthorized)?;
        let (id, secret) = parse_machine_credential(raw)?;

        let row: Option<(uuid::Uuid, String)> =
            sqlx::query_as("SELECT tenant_id, credential_hash FROM machines WHERE id = $1")
                .bind(id)
                .fetch_optional(&state.db)
                .await?;
        let (tenant, stored) = row.ok_or(ApiError::Unauthorized)?;

        let presented = super::tokens::hash_token(secret);
        // Both sides are fixed-length hex of a hash, so a constant-time
        // comparison is cheap insurance against a timing oracle.
        if !constant_time_eq(presented.as_bytes(), stored.as_bytes()) {
            return Err(ApiError::Unauthorized);
        }
        Ok(Self {
            id: MachineId::from_uuid(id),
            tenant: TenantId::from_uuid(tenant),
        })
    }
}

/// Proof that the caller holds the deployment-wide bootstrap secret.
///
/// Used for actions that have no tenant to authenticate against: creating the
/// first tenant, and registering gateway and relay nodes.
#[derive(Debug, Clone, Copy)]
pub struct BootstrapAuth;

impl FromRequestParts<AppState> for BootstrapAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer(parts, "Bearer").ok_or(ApiError::Unauthorized)?;
        if constant_time_eq(token.as_bytes(), state.config.bootstrap_token.as_bytes()) {
            Ok(Self)
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts_with(header: &str) -> Parts {
        Request::builder()
            .header(http::header::AUTHORIZATION, header)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[test]
    fn bearer_is_extracted_case_insensitively() {
        assert_eq!(bearer(&parts_with("Bearer abc"), "Bearer"), Some("abc"));
        assert_eq!(bearer(&parts_with("bearer abc"), "Bearer"), Some("abc"));
        assert_eq!(bearer(&parts_with("Machine x.y"), "Machine"), Some("x.y"));
    }

    #[test]
    fn the_wrong_scheme_is_not_accepted() {
        // A machine credential must never satisfy a user endpoint.
        assert_eq!(bearer(&parts_with("Machine x.y"), "Bearer"), None);
        assert_eq!(bearer(&parts_with("Basic abc"), "Bearer"), None);
        assert_eq!(bearer(&parts_with("Bearer"), "Bearer"), None);
    }

    #[test]
    fn role_ordering_matches_privilege() {
        assert!(UserRole::Owner > UserRole::Admin);
        assert!(UserRole::Admin > UserRole::User);
        assert!(UserRole::Admin.is_admin());
        assert!(!UserRole::User.is_admin());
    }

    #[test]
    fn role_db_representation_roundtrips() {
        for r in [UserRole::User, UserRole::Admin, UserRole::Owner] {
            assert_eq!(UserRole::from_db(r.as_db()), Some(r));
        }
        assert_eq!(UserRole::from_db("SUPERUSER"), None);
    }

    #[test]
    fn admin_checks_reject_ordinary_users() {
        let user = AuthUser {
            id: UserId::new(),
            tenant: TenantId::new(),
            role: UserRole::User,
        };
        assert!(user.require_admin().is_err());
        assert!(user.require_owner().is_err());

        let admin = AuthUser {
            role: UserRole::Admin,
            ..user
        };
        assert!(admin.require_admin().is_ok());
        // An admin is not an owner: tenant settings stay with the owner.
        assert!(admin.require_owner().is_err());
    }

    #[test]
    fn constant_time_eq_agrees_with_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
