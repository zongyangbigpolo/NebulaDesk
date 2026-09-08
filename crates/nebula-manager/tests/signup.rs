//! Signup and invitation contracts against an isolated real PostgreSQL database.
//! Set NEBULA_TEST_DATABASE_URL to a newly created nebula_signup_* database.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use nebula_manager::{routes, AppState, Config};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";

struct App {
    router: Router,
    db: PgPool,
}

impl App {
    async fn start(enabled: bool) -> Self {
        let url = std::env::var("NEBULA_TEST_DATABASE_URL")
            .expect("set NEBULA_TEST_DATABASE_URL to a dedicated nebula_signup_* database");
        let db_name = url.rsplit('/').next().unwrap().split('?').next().unwrap();
        assert!(
            db_name.starts_with("nebula_signup_"),
            "refusing non-signup database"
        );
        let mut config = Config::for_test(url);
        config.allow_self_registration = enabled;
        let state = AppState::bootstrap(config).await.unwrap();
        Self {
            db: state.db.clone(),
            router: routes::router(state),
        }
    }

    async fn call(&self, method: &str, path: &str, token: Option<&str>, body: Value) -> Reply {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        Reply { status, body }
    }

    async fn signup(&self, kind: &str) -> Value {
        self.call("POST", "/v1/auth/register", None, registration(kind))
            .await
            .ok(StatusCode::CREATED)
    }

    async fn invite(&self, owner: &Value, email: &str) -> Value {
        self.call(
            "POST",
            "/v1/workspace/invitations",
            access(owner),
            json!({"email": email}),
        )
        .await
        .ok(StatusCode::CREATED)
    }

    async fn accept(&self, invitation: &Value, email: &str) -> Reply {
        self.call(
            "POST",
            "/v1/auth/accept-invitation",
            None,
            acceptance(invitation, email),
        )
        .await
    }
}

struct Reply {
    status: StatusCode,
    body: Value,
}

impl Reply {
    fn ok(self, expected: StatusCode) -> Value {
        assert_eq!(self.status, expected, "{}", self.body);
        self.body
    }
}

fn access(pair: &Value) -> Option<&str> {
    Some(pair["access_token"].as_str().unwrap())
}

fn id(value: &Value) -> Uuid {
    value.as_str().unwrap().parse().unwrap()
}

fn registration(kind: &str) -> Value {
    json!({
        "workspace_slug": format!("signup-{}", Uuid::now_v7().simple()),
        "workspace_name": "Test workspace",
        "workspace_kind": kind,
        "display_name": "Account owner",
        "email": " Owner@Example.Test ",
        "password": PASSWORD,
    })
}

fn acceptance(invitation: &Value, email: &str) -> Value {
    json!({
        "token": invitation["token"],
        "email": email,
        "display_name": "Invited member",
        "password": PASSWORD,
    })
}

#[tokio::test]
async fn signup_flag_validation_and_atomic_unique_slug() {
    let disabled = App::start(false).await;
    assert_eq!(
        disabled
            .call("GET", "/v1/auth/registration", None, Value::Null)
            .await
            .ok(StatusCode::OK),
        json!({"self_registration_enabled": false})
    );
    disabled
        .call("POST", "/v1/auth/register", None, registration("PERSONAL"))
        .await
        .ok(StatusCode::FORBIDDEN);
    disabled
        .call("GET", "/v1/workspace", None, Value::Null)
        .await
        .ok(StatusCode::UNAUTHORIZED);

    let app = App::start(true).await;
    assert_eq!(
        app.call("GET", "/v1/auth/registration", None, Value::Null)
            .await
            .ok(StatusCode::OK),
        json!({"self_registration_enabled": true})
    );
    for (field, value, expected) in [
        ("workspace_slug", json!("Bad Slug"), StatusCode::BAD_REQUEST),
        ("workspace_slug", json!("-bad"), StatusCode::BAD_REQUEST),
        (
            "workspace_slug",
            json!("a".repeat(64)),
            StatusCode::BAD_REQUEST,
        ),
        (
            "workspace_kind",
            json!("TEAM"),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "workspace_name",
            json!(" ".repeat(4)),
            StatusCode::BAD_REQUEST,
        ),
        (
            "workspace_name",
            json!("a".repeat(201)),
            StatusCode::BAD_REQUEST,
        ),
        (
            "display_name",
            json!("a".repeat(201)),
            StatusCode::BAD_REQUEST,
        ),
        ("email", json!("not-email"), StatusCode::BAD_REQUEST),
        (
            "email",
            json!(format!("{}@test", "a".repeat(255))),
            StatusCode::BAD_REQUEST,
        ),
        ("password", json!("short"), StatusCode::BAD_REQUEST),
        ("password", json!("a".repeat(1025)), StatusCode::BAD_REQUEST),
        ("role", json!("OWNER"), StatusCode::UNPROCESSABLE_ENTITY),
        (
            "tenant_id",
            json!(Uuid::now_v7()),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let mut body = registration("ORGANIZATION");
        body[field] = value;
        app.call("POST", "/v1/auth/register", None, body)
            .await
            .ok(expected);
    }

    let body = registration("ORGANIZATION");
    let (a, b) = tokio::join!(
        app.call("POST", "/v1/auth/register", None, body.clone()),
        app.call("POST", "/v1/auth/register", None, body.clone()),
    );
    let winner = if a.status == StatusCode::CREATED {
        b.ok(StatusCode::CONFLICT);
        a.body
    } else {
        a.ok(StatusCode::CONFLICT);
        b.ok(StatusCode::CREATED)
    };
    assert_eq!(winner["user"]["role"], "ADMIN");
    assert_eq!(winner["user"]["email"], "owner@example.test");
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users WHERE tenant_id = $1),
                (SELECT count(*) FROM refresh_tokens WHERE tenant_id = $1)",
    )
    .bind(id(&winner["workspace"]["id"]))
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1));
    app.call("POST", "/v1/auth/register", None, body)
        .await
        .ok(StatusCode::CONFLICT);
}

#[tokio::test]
async fn workspace_metadata_login_and_enrollment_bind_current_account() {
    let app = App::start(true).await;
    let personal = app.signup("PERSONAL").await;
    let organization = app.signup("ORGANIZATION").await;
    assert_ne!(personal["workspace"]["id"], organization["workspace"]["id"]);
    for pair in [&personal, &organization] {
        let ws = app
            .call("GET", "/v1/workspace", access(pair), Value::Null)
            .await
            .ok(StatusCode::OK);
        assert_eq!(ws, pair["workspace"]);
        assert_eq!(ws.as_object().unwrap().len(), 4);
        assert_eq!(ws["id"], pair["user"]["tenant_id"]);
        let login = app
            .call(
                "POST",
                "/v1/auth/login",
                None,
                json!({
                    "tenant": ws["slug"], "email": "OWNER@EXAMPLE.TEST", "password": PASSWORD,
                }),
            )
            .await
            .ok(StatusCode::OK);
        assert!(
            login.get("workspace").is_none(),
            "legacy login shape changed"
        );
        assert_eq!(login["user"]["id"], pair["user"]["id"]);
        app.call(
            "POST",
            "/v1/auth/refresh",
            None,
            json!({
                "refresh_token": pair["refresh_token"],
            }),
        )
        .await
        .ok(StatusCode::OK);
        enroll_and_assert_owner(&app, pair).await;
    }
    for method in ["GET", "POST"] {
        app.call(
            method,
            "/v1/workspace/invitations",
            access(&personal),
            json!({"email": "x@test"}),
        )
        .await
        .ok(StatusCode::FORBIDDEN);
    }
    let invite = app.invite(&organization, "member@test").await;
    let member = app
        .accept(&invite, "member@test")
        .await
        .ok(StatusCode::CREATED);
    enroll_and_assert_owner(&app, &member).await;
    app.call("GET", "/v1/workspace", access(&member), Value::Null)
        .await
        .ok(StatusCode::OK);
    app.call("GET", "/v1/users", access(&member), Value::Null)
        .await
        .ok(StatusCode::FORBIDDEN);
}

async fn enroll_and_assert_owner(app: &App, pair: &Value) {
    let machine_name = format!("Assigned device {}", Uuid::now_v7());
    let token = app
        .call(
            "POST",
            "/v1/machines/enrollment-tokens",
            access(pair),
            json!({"machine_name": machine_name, "owner_user_id": pair["user"]["id"]}),
        )
        .await
        .ok(StatusCode::CREATED);
    let enrolled = app
        .call(
            "POST",
            "/v1/machines/enroll",
            None,
            json!({
                "token": token["token"], "name": machine_name, "os": "MACOS",
                "os_version": "26.0", "arch": "arm64", "agent_version": "0.1.0",
                "noise_public_key": hex::encode(rand::random::<[u8; 32]>()),
            }),
        )
        .await
        .ok(StatusCode::CREATED);
    let stored: (Uuid, Uuid) =
        sqlx::query_as("SELECT tenant_id, owner_user_id FROM machines WHERE id = $1")
            .bind(id(&enrolled["machine_id"]))
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        stored,
        (id(&pair["workspace"]["id"]), id(&pair["user"]["id"]))
    );
}

#[tokio::test]
async fn invitation_contract_is_secret_free_scoped_and_fixed_role() {
    let app = App::start(true).await;
    let owner = app.signup("ORGANIZATION").await;
    let foreign = app.signup("ORGANIZATION").await;
    for extra in ["role", "tenant_id", "issuer_id"] {
        let mut body = json!({"email": "member@test"});
        body[extra] = json!("arbitrary");
        app.call("POST", "/v1/workspace/invitations", access(&owner), body)
            .await
            .ok(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let invitation = app.invite(&owner, " Member@Example.Test ").await;
    assert_eq!(invitation["email"], "member@example.test");
    assert_eq!(invitation.as_object().unwrap().len(), 7);
    assert!(invitation["accepted_at"].is_null());
    assert!(invitation["revoked_at"].is_null());
    assert!(invitation["token"].as_str().unwrap().len() >= 32);
    let stored: (String, f64) = sqlx::query_as(
        "SELECT token_hash, EXTRACT(EPOCH FROM expires_at - created_at)::float8
         FROM workspace_invitations WHERE id = $1",
    )
    .bind(id(&invitation["id"]))
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_ne!(stored.0, invitation["token"].as_str().unwrap());
    assert!((stored.1 - 48.0 * 3600.0).abs() < 5.0);
    let list = app
        .call(
            "GET",
            "/v1/workspace/invitations",
            access(&owner),
            Value::Null,
        )
        .await
        .ok(StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0].as_object().unwrap().len(), 6);
    assert!(list[0].get("token").is_none());
    assert!(list[0].get("token_hash").is_none());
    assert_eq!(
        app.call(
            "GET",
            "/v1/workspace/invitations",
            access(&foreign),
            Value::Null
        )
        .await
        .ok(StatusCode::OK),
        json!([])
    );
    let path = format!(
        "/v1/workspace/invitations/{}",
        invitation["id"].as_str().unwrap()
    );
    app.call("DELETE", &path, access(&foreign), Value::Null)
        .await
        .ok(StatusCode::NOT_FOUND);
    let wrong = app
        .accept(&invitation, "wrong@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    let unknown = app
        .accept(&json!({"token": "unknown"}), "wrong@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    assert_eq!(wrong, unknown);
    for extra in ["role", "tenant_id", "workspace_slug", "owner_id"] {
        let mut body = acceptance(&invitation, "member@example.test");
        body[extra] = json!("arbitrary");
        app.call("POST", "/v1/auth/accept-invitation", None, body)
            .await
            .ok(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let flag_off = App::start(false).await;
    let member = flag_off
        .accept(&invitation, " MEMBER@EXAMPLE.TEST ")
        .await
        .ok(StatusCode::CREATED);
    assert_eq!(member["user"]["role"], "USER");
    assert_eq!(member["user"]["email"], "member@example.test");
    assert_eq!(member["workspace"], owner["workspace"]);
    assert_eq!(member["user"]["tenant_id"], owner["user"]["tenant_id"]);
    app.accept(&invitation, "member@example.test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    for method in ["GET", "POST", "DELETE"] {
        let endpoint = if method == "DELETE" {
            path.as_str()
        } else {
            "/v1/workspace/invitations"
        };
        app.call(
            method,
            endpoint,
            access(&member),
            json!({"email": "x@test"}),
        )
        .await
        .ok(StatusCode::FORBIDDEN);
    }
    let list = app
        .call(
            "GET",
            "/v1/workspace/invitations",
            access(&owner),
            Value::Null,
        )
        .await
        .ok(StatusCode::OK);
    assert!(!list[0]["accepted_at"].is_null());
    let audits: String = sqlx::query_scalar(
        "SELECT COALESCE(json_agg(a)::text, '[]') FROM audit_log a WHERE tenant_id = $1",
    )
    .bind(id(&owner["workspace"]["id"]))
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert!(!audits.contains(invitation["token"].as_str().unwrap()));
    assert!(!audits.contains(&stored.0));
    assert!(!audits.contains(PASSWORD));
}

#[tokio::test]
async fn invitations_reject_revoked_expired_and_disabled_authorities() {
    let app = App::start(true).await;
    let owner = app.signup("ORGANIZATION").await;
    let revoked = app.invite(&owner, "revoked@test").await;
    let path = format!(
        "/v1/workspace/invitations/{}",
        revoked["id"].as_str().unwrap()
    );
    for _ in 0..2 {
        app.call("DELETE", &path, access(&owner), Value::Null)
            .await
            .ok(StatusCode::NO_CONTENT);
    }
    app.accept(&revoked, "revoked@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    let expired = app.invite(&owner, "expired@test").await;
    sqlx::query(
        "UPDATE workspace_invitations SET expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(id(&expired["id"]))
    .execute(&app.db)
    .await
    .unwrap();
    app.accept(&expired, "expired@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);

    let invitation = app.invite(&owner, "pending@test").await;
    sqlx::query("UPDATE users SET disabled = true WHERE id = $1")
        .bind(id(&owner["user"]["id"]))
        .execute(&app.db)
        .await
        .unwrap();
    app.accept(&invitation, "pending@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    sqlx::query("UPDATE users SET disabled = false, role = 'USER' WHERE id = $1")
        .bind(id(&owner["user"]["id"]))
        .execute(&app.db)
        .await
        .unwrap();
    app.accept(&invitation, "pending@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    sqlx::query("UPDATE users SET role = 'ADMIN' WHERE id = $1")
        .bind(id(&owner["user"]["id"]))
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE tenants SET disabled = true WHERE id = $1")
        .bind(id(&owner["workspace"]["id"]))
        .execute(&app.db)
        .await
        .unwrap();
    app.accept(&invitation, "pending@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    app.call("GET", "/v1/workspace", access(&owner), Value::Null)
        .await
        .ok(StatusCode::UNAUTHORIZED);
    app.call(
        "POST",
        "/v1/workspace/invitations",
        access(&owner),
        json!({"email": "x@test"}),
    )
    .await
    .ok(StatusCode::FORBIDDEN);
    sqlx::query("UPDATE tenants SET disabled = false, kind = 'PERSONAL' WHERE id = $1")
        .bind(id(&owner["workspace"]["id"]))
        .execute(&app.db)
        .await
        .unwrap();
    app.accept(&invitation, "pending@test")
        .await
        .ok(StatusCode::UNAUTHORIZED);
    let consumed: bool = sqlx::query_scalar(
        "SELECT accepted_at IS NOT NULL FROM workspace_invitations WHERE id = $1",
    )
    .bind(id(&invitation["id"]))
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert!(!consumed);
    sqlx::query("UPDATE tenants SET kind = 'ORGANIZATION' WHERE id = $1")
        .bind(id(&owner["workspace"]["id"]))
        .execute(&app.db)
        .await
        .unwrap();
    app.accept(&invitation, "pending@test")
        .await
        .ok(StatusCode::CREATED);
}

#[tokio::test]
async fn concurrent_acceptance_and_duplicate_accounts_are_atomic() {
    let app = App::start(true).await;
    let owner = app.signup("ORGANIZATION").await;
    let invite = app.invite(&owner, "race@test").await;
    let (a, b) = tokio::join!(
        app.accept(&invite, "race@test"),
        app.accept(&invite, "race@test")
    );
    if a.status == StatusCode::CREATED {
        b.ok(StatusCode::UNAUTHORIZED);
    } else {
        a.ok(StatusCode::UNAUTHORIZED);
        b.ok(StatusCode::CREATED);
    }
    let one = app.invite(&owner, "double@test").await;
    let two = app.invite(&owner, "DOUBLE@test").await;
    let (a, b) = tokio::join!(
        app.accept(&one, "double@test"),
        app.accept(&two, "double@test")
    );
    if a.status == StatusCode::CREATED {
        b.ok(StatusCode::CONFLICT);
    } else {
        a.ok(StatusCode::CONFLICT);
        b.ok(StatusCode::CREATED);
    }
    let consumed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workspace_invitations WHERE id IN ($1, $2) AND accepted_at IS NOT NULL"
    ).bind(id(&one["id"])).bind(id(&two["id"])).fetch_one(&app.db).await.unwrap();
    assert_eq!(consumed, 1);
    let before: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(id(&owner["user"]["id"]))
        .fetch_one(&app.db)
        .await
        .unwrap();
    let duplicate = app.invite(&owner, "OWNER@example.test").await;
    let mut body = acceptance(&duplicate, "owner@example.test");
    body["password"] = json!("replacement password must not stick");
    app.call("POST", "/v1/auth/accept-invitation", None, body)
        .await
        .ok(StatusCode::CONFLICT);
    let after: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
        .bind(id(&owner["user"]["id"]))
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(before, after);
    let consumed: bool = sqlx::query_scalar(
        "SELECT accepted_at IS NOT NULL FROM workspace_invitations WHERE id = $1",
    )
    .bind(id(&duplicate["id"]))
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert!(!consumed);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM users WHERE tenant_id = $1),
                (SELECT count(*) FROM refresh_tokens WHERE tenant_id = $1)",
    )
    .bind(id(&owner["workspace"]["id"]))
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(counts, (3, 3));
}

#[tokio::test]
async fn organization_group_directory_and_memberships_are_tenant_scoped() {
    let app = App::start(true).await;
    let owner = app.signup("ORGANIZATION").await;
    let foreign = app.signup("ORGANIZATION").await;
    let invitation = app.invite(&owner, "group-member@test").await;
    let member = app
        .accept(&invitation, "group-member@test")
        .await
        .ok(StatusCode::CREATED);
    let group = app
        .call(
            "POST",
            "/v1/groups",
            access(&owner),
            json!({"name": "Engineering"}),
        )
        .await
        .ok(StatusCode::CREATED);
    let foreign_group = app
        .call(
            "POST",
            "/v1/groups",
            access(&foreign),
            json!({"name": "Engineering"}),
        )
        .await
        .ok(StatusCode::CREATED);
    let members_path = format!("/v1/groups/{}/members", group["id"].as_str().unwrap());
    let group_path = format!("/v1/groups/{}", group["id"].as_str().unwrap());
    assert_eq!(
        app.call("GET", "/v1/groups", access(&owner), Value::Null)
            .await
            .ok(StatusCode::OK),
        json!([{"id": group["id"], "name": "Engineering"}])
    );
    assert_eq!(
        app.call("GET", "/v1/groups", access(&foreign), Value::Null)
            .await
            .ok(StatusCode::OK),
        json!([{"id": foreign_group["id"], "name": "Engineering"}])
    );
    assert_eq!(
        app.call("GET", &members_path, access(&owner), Value::Null)
            .await
            .ok(StatusCode::OK),
        json!([])
    );
    for _ in 0..2 {
        app.call(
            "POST",
            &members_path,
            access(&owner),
            json!({"user_id": member["user"]["id"]}),
        )
        .await
        .ok(StatusCode::NO_CONTENT);
    }
    let members = app
        .call("GET", &members_path, access(&owner), Value::Null)
        .await
        .ok(StatusCode::OK);
    assert_eq!(
        members,
        json!([{
            "id": member["user"]["id"],
            "email": "group-member@test",
            "display_name": "Invited member",
            "role": "USER",
            "disabled": false,
        }])
    );
    app.call("GET", &members_path, access(&foreign), Value::Null)
        .await
        .ok(StatusCode::NOT_FOUND);
    app.call("DELETE", &group_path, access(&foreign), Value::Null)
        .await
        .ok(StatusCode::NOT_FOUND);
    // The foreign caller cannot even replay an already-existing membership.
    app.call(
        "POST",
        &members_path,
        access(&foreign),
        json!({"user_id": member["user"]["id"]}),
    )
    .await
    .ok(StatusCode::NOT_FOUND);
    app.call(
        "POST",
        &members_path,
        access(&owner),
        json!({"user_id": foreign["user"]["id"]}),
    )
    .await
    .ok(StatusCode::NOT_FOUND);
    for (method, path, body) in [
        ("GET", "/v1/groups", Value::Null),
        ("GET", "/v1/users", Value::Null),
        ("GET", members_path.as_str(), Value::Null),
        ("POST", "/v1/groups", json!({"name": "Unauthorized"})),
        (
            "POST",
            members_path.as_str(),
            json!({"user_id": member["user"]["id"]}),
        ),
        ("DELETE", group_path.as_str(), Value::Null),
    ] {
        app.call(method, path, access(&member), body)
            .await
            .ok(StatusCode::FORBIDDEN);
    }
    let membership_path = format!("{members_path}/{}", member["user"]["id"].as_str().unwrap());
    app.call("DELETE", &membership_path, access(&member), Value::Null)
        .await
        .ok(StatusCode::FORBIDDEN);
    app.call("DELETE", &membership_path, access(&foreign), Value::Null)
        .await
        .ok(StatusCode::NO_CONTENT);
    assert_eq!(
        app.call("GET", &members_path, access(&owner), Value::Null)
            .await
            .ok(StatusCode::OK),
        members
    );
    app.call("DELETE", &membership_path, access(&owner), Value::Null)
        .await
        .ok(StatusCode::NO_CONTENT);
    assert_eq!(
        app.call("GET", &members_path, access(&owner), Value::Null)
            .await
            .ok(StatusCode::OK),
        json!([])
    );
    app.call(
        "POST",
        &members_path,
        access(&owner),
        json!({"user_id": member["user"]["id"]}),
    )
    .await
    .ok(StatusCode::NO_CONTENT);
    app.call("DELETE", &group_path, access(&owner), Value::Null)
        .await
        .ok(StatusCode::NO_CONTENT);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_group_members WHERE group_id = $1")
            .bind(id(&group["id"]))
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(count, 0);
    app.call("GET", &members_path, access(&owner), Value::Null)
        .await
        .ok(StatusCode::NOT_FOUND);
    assert_eq!(
        app.call("GET", "/v1/groups", access(&owner), Value::Null)
            .await
            .ok(StatusCode::OK),
        json!([])
    );
    assert_eq!(
        app.call("GET", "/v1/groups", access(&foreign), Value::Null)
            .await
            .ok(StatusCode::OK),
        json!([{"id": foreign_group["id"], "name": "Engineering"}])
    );
}
