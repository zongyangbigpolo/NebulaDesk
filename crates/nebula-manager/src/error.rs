//! The API error type.
//!
//! Every failure the HTTP layer can produce funnels through [`ApiError`] so
//! that status codes, log levels and response bodies are decided in exactly
//! one place. In particular, authentication and authorisation failures must
//! never leak *why* they failed: "no such user", "wrong password" and
//! "account disabled" all become the same 401.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use tracing::{error, warn};

/// A failure that can be rendered as an HTTP response.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request body or parameters were unusable.
    #[error("{0}")]
    BadRequest(String),

    /// No valid credentials were presented.
    #[error("authentication required")]
    Unauthorized,

    /// Valid credentials, insufficient rights.
    #[error("{0}")]
    Forbidden(String),

    /// The addressed entity does not exist, or the caller may not see it.
    #[error("{0} not found")]
    NotFound(&'static str),

    /// The request conflicts with existing state.
    #[error("{0}")]
    Conflict(String),

    /// The caller is asking too often.
    #[error("too many requests")]
    TooManyRequests,

    /// The database rejected or failed the operation.
    #[error(transparent)]
    Database(#[from] sqlx::Error),

    /// Anything unexpected.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m.clone()),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            ApiError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                self.to_string(),
            ),
            // A unique-violation reaching this point means a concurrent
            // request won the race; that is a conflict, not a server fault.
            ApiError::Database(sqlx::Error::Database(e)) if e.is_unique_violation() => (
                StatusCode::CONFLICT,
                "conflict",
                "resource already exists".into(),
            ),
            ApiError::Database(sqlx::Error::RowNotFound) => {
                (StatusCode::NOT_FOUND, "not_found", "not found".into())
            }
            // Details of an internal failure stay in the logs. Echoing a
            // database error to the caller is an information leak.
            ApiError::Database(_) | ApiError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error".into(),
            ),
        }
    }
}

#[derive(Serialize)]
struct Body {
    error: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
        if status.is_server_error() {
            error!(error = %self, "request failed");
        } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            warn!(error = %self, "request denied");
        }
        (
            status,
            Json(Body {
                error: code,
                message,
            }),
        )
            .into_response()
    }
}

/// Result alias for handlers.
pub type ApiResult<T> = Result<T, ApiError>;

impl From<nebula_common::Error> for ApiError {
    fn from(e: nebula_common::Error) -> Self {
        use nebula_common::Error as E;
        match e {
            E::NotFound(_) => ApiError::NotFound("resource"),
            E::Denied(m) => ApiError::Forbidden(m),
            E::Invalid(m) | E::Config(m) => ApiError::BadRequest(m),
            E::Conflict(m) => ApiError::Conflict(m),
            other => ApiError::Internal(other.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_failures_do_not_leak_their_detail() {
        let err = ApiError::Internal(anyhow::anyhow!("connection string: postgres://secret"));
        let (status, code, message) = err.parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "internal");
        assert_eq!(message, "internal error");
        assert!(!message.contains("secret"));
    }

    #[test]
    fn database_errors_do_not_leak_their_detail() {
        let err = ApiError::Database(sqlx::Error::PoolTimedOut);
        let (status, _, message) = err.parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(message, "internal error");
    }

    #[test]
    fn a_missing_row_is_a_404_not_a_500() {
        let (status, code, _) = ApiError::Database(sqlx::Error::RowNotFound).parts();
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(code, "not_found");
    }

    #[test]
    fn unauthorized_says_nothing_about_the_cause() {
        // Distinguishing "no such user" from "wrong password" is a user
        // enumeration oracle.
        let (status, _, message) = ApiError::Unauthorized.parts();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(message, "authentication required");
    }
}
