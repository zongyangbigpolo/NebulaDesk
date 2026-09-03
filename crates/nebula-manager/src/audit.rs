//! Audit logging.
//!
//! Every decision that grants or denies access to a machine is recorded. The
//! log is append-only from the application's point of view: there is no
//! update or delete path, because an audit trail an administrator can edit is
//! not an audit trail.

use nebula_common::{TenantId, UserId};
use serde_json::Value;
use sqlx::PgPool;
use tracing::warn;
use uuid::Uuid;

/// The outcome being recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The action was permitted.
    Allow,
    /// The action was refused.
    Deny,
    /// The action failed for a non-policy reason.
    Error,
}

impl Outcome {
    const fn as_db(self) -> &'static str {
        match self {
            Outcome::Allow => "ALLOW",
            Outcome::Deny => "DENY",
            Outcome::Error => "ERROR",
        }
    }
}

/// One audit entry, built fluently at the call site.
#[derive(Debug, Clone)]
pub struct Entry {
    tenant: Option<TenantId>,
    actor: Option<UserId>,
    action: String,
    target_kind: Option<String>,
    target_id: Option<Uuid>,
    outcome: Outcome,
    detail: Value,
    ip: Option<String>,
}

impl Entry {
    /// Start an entry for `action`, for example `session.create`.
    #[must_use]
    pub fn new(action: impl Into<String>, outcome: Outcome) -> Self {
        Self {
            tenant: None,
            actor: None,
            action: action.into(),
            target_kind: None,
            target_id: None,
            outcome,
            detail: Value::Object(Default::default()),
            ip: None,
        }
    }

    /// Attribute the entry to a tenant.
    #[must_use]
    pub fn tenant(mut self, tenant: TenantId) -> Self {
        self.tenant = Some(tenant);
        self
    }

    /// Attribute the entry to a user.
    #[must_use]
    pub fn actor(mut self, actor: UserId) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Record what was acted upon.
    #[must_use]
    pub fn target(mut self, kind: impl Into<String>, id: Uuid) -> Self {
        self.target_kind = Some(kind.into());
        self.target_id = Some(id);
        self
    }

    /// Attach structured detail.
    #[must_use]
    pub fn detail(mut self, detail: Value) -> Self {
        self.detail = detail;
        self
    }

    /// Record the caller's address.
    #[must_use]
    pub fn ip(mut self, ip: Option<String>) -> Self {
        self.ip = ip;
        self
    }

    /// Write the entry.
    ///
    /// A failure to audit is logged loudly but never fails the request: an
    /// operator who cannot log in because the audit table is full has a worse
    /// problem than a missing log line. Denials are the exception the caller
    /// should care about, and those are also emitted to the tracing log.
    pub async fn write(self, db: &PgPool) {
        let result = sqlx::query(
            "INSERT INTO audit_log
               (id, tenant_id, actor_user_id, action, target_kind, target_id, result, detail, ip)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(Uuid::now_v7())
        .bind(self.tenant.map(|t| t.as_uuid()))
        .bind(self.actor.map(|a| a.as_uuid()))
        .bind(&self.action)
        .bind(self.target_kind.as_deref())
        .bind(self.target_id)
        .bind(self.outcome.as_db())
        .bind(&self.detail)
        .bind(self.ip.as_deref())
        .execute(db)
        .await;

        if let Err(e) = result {
            warn!(action = %self.action, error = %e, "failed to write audit entry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_map_to_the_schemas_check_constraint() {
        // These strings are constrained by the schema; a typo would only
        // surface as a runtime insert failure.
        assert_eq!(Outcome::Allow.as_db(), "ALLOW");
        assert_eq!(Outcome::Deny.as_db(), "DENY");
        assert_eq!(Outcome::Error.as_db(), "ERROR");
    }

    #[test]
    fn entries_build_fluently() {
        let id = Uuid::now_v7();
        let e = Entry::new("session.create", Outcome::Deny)
            .tenant(TenantId::new())
            .actor(UserId::new())
            .target("resource", id)
            .detail(serde_json::json!({ "reason": "no entitlement" }))
            .ip(Some("203.0.113.9".into()));
        assert_eq!(e.action, "session.create");
        assert_eq!(e.target_id, Some(id));
        assert_eq!(e.detail["reason"], "no entitlement");
    }
}
