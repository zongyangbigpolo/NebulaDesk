//! HTTP routing.

pub mod auth;
pub mod directory;
pub mod machines;
pub mod nodes;
pub mod resources;
pub mod sessions;

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

/// The client's apparent source address.
///
/// Behind a load balancer the socket address is the balancer's, so the
/// left-most `X-Forwarded-For` entry is used when present. This value is only
/// ever recorded for audit and support: it is never used to make an
/// authorisation decision, because a client controls the header.
#[derive(Debug, Clone)]
pub struct ClientIp(pub Option<String>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(forwarded) = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return Ok(Self(Some(forwarded.to_string())));
        }
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| addr.ip().to_string()),
        ))
    }
}

/// Liveness probe.
async fn health() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

/// Readiness probe: succeeds only when the database answers.
async fn ready(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<StatusCode, crate::error::ApiError> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The public key set gateways use to verify session tickets offline.
async fn jwks(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<nebula_common::Jwks> {
    Json(state.signer.jwks())
}

/// Build the complete application router.
pub fn router(state: AppState) -> Router {
    let max_body = state.config.max_body_bytes;

    let public = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/v1/auth/login", post(auth::login))
        .route("/v1/auth/refresh", post(auth::refresh))
        .route("/v1/auth/logout", post(auth::logout))
        // Enrolment authenticates with the token in the body, so it cannot
        // sit behind a credential extractor.
        .route("/v1/machines/enroll", post(machines::enroll));

    let tenanted = Router::new()
        .route("/v1/auth/me", get(auth::me))
        .route(
            "/v1/users",
            post(directory::create_user).get(directory::list_users),
        )
        .route(
            "/v1/users/{id}",
            patch(directory::update_user).delete(directory::delete_user),
        )
        .route(
            "/v1/groups",
            post(directory::create_group).get(directory::list_groups),
        )
        .route("/v1/groups/{id}/members", post(directory::add_group_member))
        .route(
            "/v1/groups/{id}/members/{user_id}",
            delete(directory::remove_group_member),
        )
        .route(
            "/v1/machines/enrollment-tokens",
            post(machines::create_enrollment_token),
        )
        .route("/v1/machines", get(machines::list_machines))
        .route("/v1/machines/{id}", delete(machines::delete_machine))
        .route(
            "/v1/machines/{id}/resources",
            post(resources::publish).get(resources::list_for_machine),
        )
        .route("/v1/resources", get(resources::list_mine))
        .route(
            "/v1/resources/{id}",
            patch(resources::update).delete(resources::delete),
        )
        .route(
            "/v1/resources/{id}/entitlements",
            post(resources::grant).get(resources::list_grants),
        )
        .route("/v1/entitlements/{id}", delete(resources::revoke))
        .route("/v1/sessions", post(sessions::create).get(sessions::list))
        .route("/v1/sessions/{id}", delete(sessions::close));

    let infra = Router::new()
        .route("/v1/tenants", post(directory::create_tenant))
        .route("/v1/gateways", post(nodes::register_gateway))
        .route("/v1/relays", post(nodes::register_relay))
        .route("/v1/machines/heartbeat", post(machines::heartbeat))
        .route("/v1/sessions/{id}/report", post(nodes::report_session));

    Router::new()
        .merge(public)
        .merge(tenanted)
        .merge(infra)
        // Applied outermost so an oversized body is rejected before any
        // handler, extractor or database connection is involved.
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::map_response(no_store))
        .with_state(state)
}

/// Every response here carries credentials or authorisation decisions; none of
/// it may sit in an intermediary cache.
async fn no_store(mut response: axum::response::Response) -> axum::response::Response {
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}
