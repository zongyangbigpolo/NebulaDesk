//! End-to-end tests for the manager's HTTP surface.
//!
//! These run against a real Postgres because the interesting behaviour lives
//! in the SQL: composite foreign keys that make cross-tenant references
//! impossible, single-statement token rotation, and the entitlement join that
//! decides who may launch what. Mocking the database would test none of it.
//!
//! Point `NEBULA_TEST_DATABASE_URL` at a scratch database; the default is a
//! local `nebula_manager_test`. Every test creates its own tenant, so they
//! are safe to run in parallel.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use nebula_manager::{routes, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

/// A running manager plus a freshly created tenant to work in.
struct App {
    router: Router,
    tenant_slug: String,
    owner_token: String,
    owner_id: Uuid,
}

fn database_url() -> String {
    std::env::var("NEBULA_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres:///nebula_manager_test".into())
}

const BOOTSTRAP: &str = "test-bootstrap-token";
const PASSWORD: &str = "correct horse battery staple";

impl App {
    async fn start() -> Self {
        let state = AppState::bootstrap(Config::for_test(database_url()))
            .await
            .expect("the manager should start against the test database");
        let router = routes::router(state);

        let slug = format!("t{}", Uuid::now_v7().simple());
        let body = request(
            &router,
            "POST",
            "/v1/tenants",
            Some(BOOTSTRAP),
            json!({
                "name": "Acme",
                "slug": slug,
                "owner_email": "owner@acme.test",
                "owner_password": PASSWORD,
                "owner_display_name": "Owner",
            }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();

        let owner_id: Uuid = body["owner"]["id"].as_str().unwrap().parse().unwrap();
        let mut app = Self {
            router,
            tenant_slug: slug,
            owner_token: String::new(),
            owner_id,
        };
        app.owner_token = app.login("owner@acme.test", PASSWORD).await;
        app
    }

    async fn login(&self, email: &str, password: &str) -> String {
        let body = self
            .post(
                "/v1/auth/login",
                None,
                json!({ "tenant": self.tenant_slug, "email": email, "password": password }),
            )
            .await
            .expect_status(StatusCode::OK)
            .json();
        body["access_token"].as_str().unwrap().to_string()
    }

    // These return an un-sent request rather than a future so that a caller
    // can swap in a machine or node credential before awaiting it.
    fn post(&self, path: &str, token: Option<&str>, body: Value) -> PendingRequest<'_> {
        request(&self.router, "POST", path, token, body)
    }

    fn patch(&self, path: &str, token: Option<&str>, body: Value) -> PendingRequest<'_> {
        request(&self.router, "PATCH", path, token, body)
    }

    fn get(&self, path: &str, token: Option<&str>) -> PendingRequest<'_> {
        request(&self.router, "GET", path, token, Value::Null)
    }

    fn delete(&self, path: &str, token: Option<&str>) -> PendingRequest<'_> {
        request(&self.router, "DELETE", path, token, Value::Null)
    }

    /// Create a user and return `(id, access token)`.
    async fn user(&self, email: &str, role: &str) -> (Uuid, String) {
        let body = self
            .post(
                "/v1/users",
                Some(&self.owner_token),
                json!({
                    "email": email,
                    "password": PASSWORD,
                    "display_name": email,
                    "role": role,
                }),
            )
            .await
            .expect_status(StatusCode::CREATED)
            .json();
        let id = body["id"].as_str().unwrap().parse().unwrap();
        (id, self.login(email, PASSWORD).await)
    }

    /// Enrol a machine and return `(machine id, credential)`.
    async fn machine(&self, name: &str) -> (Uuid, String) {
        self.machine_as(name, &self.owner_token).await
    }

    async fn machine_as(&self, name: &str, caller: &str) -> (Uuid, String) {
        let token = self
            .post(
                "/v1/machines/enrollment-tokens",
                Some(caller),
                json!({ "machine_name": name }),
            )
            .await
            .expect_status(StatusCode::CREATED)
            .json()["token"]
            .as_str()
            .unwrap()
            .to_string();

        let body = self
            .post(
                "/v1/machines/enroll",
                None,
                json!({
                    "token": token,
                    "name": name,
                    "os": "MACOS",
                    "os_version": "26.0",
                    "arch": "arm64",
                    "agent_version": "0.1.0",
                    "noise_public_key": hex::encode(rand_key()),
                }),
            )
            .await
            .expect_status(StatusCode::CREATED)
            .json();
        (
            body["machine_id"].as_str().unwrap().parse().unwrap(),
            body["credential"].as_str().unwrap().to_string(),
        )
    }

    /// Enrol a machine, mark it online and publish a desktop from it.
    ///
    /// `gateway` pins which gateway the machine's control tunnel is attached
    /// to, which is what makes session placement deterministic when several
    /// tests share a database.
    async fn online_machine_with_desktop(&self, name: &str, gateway: Option<Uuid>) -> (Uuid, Uuid) {
        let (machine, credential) = self.machine(name).await;
        self.post(
            "/v1/machines/heartbeat",
            None,
            json!({ "status": "ONLINE", "gateway_id": gateway }),
        )
        .with_machine(&credential)
        .await
        .expect_status(StatusCode::NO_CONTENT);

        let resource = self
            .post(
                &format!("/v1/machines/{machine}/resources"),
                Some(&self.owner_token),
                json!({ "kind": "DESKTOP", "name": "Desktop" }),
            )
            .await
            .expect_status(StatusCode::CREATED)
            .json()["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        (machine, resource)
    }

    /// Register one gateway and one relay so sessions can be placed, and
    /// return the gateway's id and credential.
    async fn infrastructure(&self) -> (Uuid, String) {
        let suffix = Uuid::now_v7().simple().to_string();
        let gateway = self
            .post(
                "/v1/gateways",
                Some(BOOTSTRAP),
                json!({
                    "name": format!("gw-{suffix}"),
                    "public_url": "https://gw.test",
                    "quic_addr": "gw.test:7443",
                    "cert_pin": "ab".repeat(32),
                }),
            )
            .await
            .expect_status(StatusCode::CREATED)
            .json();

        self.post(
            "/v1/relays",
            Some(BOOTSTRAP),
            json!({
                "name": format!("relay-{suffix}"),
                "quic_addr": "relay.test:7444",
                "cert_pin": "cd".repeat(32),
            }),
        )
        .await
        .expect_status(StatusCode::CREATED);

        (
            gateway["id"].as_str().unwrap().parse().unwrap(),
            gateway["credential"].as_str().unwrap().to_string(),
        )
    }
}

fn rand_key() -> [u8; 32] {
    rand::random()
}

/// A captured HTTP response.
struct Response {
    status: StatusCode,
    body: Vec<u8>,
}

impl Response {
    fn expect_status(self, expected: StatusCode) -> Self {
        assert_eq!(
            self.status,
            expected,
            "unexpected status; body was {}",
            String::from_utf8_lossy(&self.body)
        );
        self
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "expected JSON, got {:?}: {e}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }
}

/// A request that has been built but not yet sent, so the auth scheme can be
/// swapped for a machine or node credential.
struct PendingRequest<'a> {
    router: &'a Router,
    method: &'static str,
    path: String,
    body: Value,
    auth: Option<String>,
}

impl PendingRequest<'_> {
    fn with_machine(mut self, credential: &str) -> Self {
        self.auth = Some(format!("Machine {credential}"));
        self
    }

    fn with_node(mut self, credential: &str) -> Self {
        self.auth = Some(format!("Node {credential}"));
        self
    }
}

impl std::future::IntoFuture for PendingRequest<'_> {
    type Output = Response;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        // Everything is moved out first so the returned future owns its data
        // and does not borrow the caller's router.
        let (router, method, path, auth, body) = (
            self.router.clone(),
            self.method,
            self.path,
            self.auth,
            self.body,
        );
        Box::pin(async move { send(&router, method, &path, auth, body).await })
    }
}

fn request<'a>(
    router: &'a Router,
    method: &'static str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> PendingRequest<'a> {
    PendingRequest {
        router,
        method,
        path: path.to_string(),
        body,
        auth: token.map(|t| format!("Bearer {t}")),
    }
}

async fn send(
    router: &Router,
    method: &str,
    path: &str,
    auth: Option<String>,
    body: Value,
) -> Response {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(auth) = auth {
        builder = builder.header("authorization", auth);
    }
    let request = if body.is_null() {
        builder.body(Body::empty()).unwrap()
    } else {
        builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Response {
        status,
        body: body.to_vec(),
    }
}

#[tokio::test]
async fn health_and_jwks_are_public() {
    let app = App::start().await;
    app.get("/health", None).await.expect_status(StatusCode::OK);
    app.get("/ready", None)
        .await
        .expect_status(StatusCode::NO_CONTENT);

    let jwks = app
        .get("/.well-known/jwks.json", None)
        .await
        .expect_status(StatusCode::OK)
        .json();
    // A gateway with no key cannot verify a ticket, so an empty set here
    // would silently break every session launch.
    let key = &jwks["keys"][0];
    assert_eq!(key["kty"], "OKP");
    assert_eq!(key["crv"], "Ed25519");
    assert!(key["kid"].as_str().is_some_and(|k| !k.is_empty()));
}

#[tokio::test]
async fn tenant_creation_requires_the_bootstrap_secret() {
    let app = App::start().await;
    for token in [None, Some("wrong")] {
        app.post(
            "/v1/tenants",
            token,
            json!({
                "name": "Squatter",
                "slug": format!("s{}", Uuid::now_v7().simple()),
                "owner_email": "a@b.test",
                "owner_password": PASSWORD,
                "owner_display_name": "A",
            }),
        )
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn login_rejects_bad_credentials_without_saying_why() {
    let app = App::start().await;
    let wrong_password = app
        .post(
            "/v1/auth/login",
            None,
            json!({ "tenant": app.tenant_slug, "email": "owner@acme.test", "password": "nope" }),
        )
        .await
        .expect_status(StatusCode::UNAUTHORIZED)
        .json();
    let missing_user = app
        .post(
            "/v1/auth/login",
            None,
            json!({ "tenant": app.tenant_slug, "email": "ghost@acme.test", "password": PASSWORD }),
        )
        .await
        .expect_status(StatusCode::UNAUTHORIZED)
        .json();
    // Identical responses: anything else turns login into a user directory.
    assert_eq!(wrong_password, missing_user);
}

#[tokio::test]
async fn refresh_tokens_rotate_and_the_old_one_dies() {
    let app = App::start().await;
    let first = app
        .post(
            "/v1/auth/login",
            None,
            json!({ "tenant": app.tenant_slug, "email": "owner@acme.test", "password": PASSWORD }),
        )
        .await
        .expect_status(StatusCode::OK)
        .json()["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    let second = app
        .post("/v1/auth/refresh", None, json!({ "refresh_token": first }))
        .await
        .expect_status(StatusCode::OK)
        .json()["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(first, second);

    // Replaying the consumed token must fail: that is what makes theft
    // detectable rather than silently useful forever.
    app.post("/v1/auth/refresh", None, json!({ "refresh_token": first }))
        .await
        .expect_status(StatusCode::UNAUTHORIZED);

    app.post("/v1/auth/logout", None, json!({ "refresh_token": second }))
        .await
        .expect_status(StatusCode::NO_CONTENT);
    app.post("/v1/auth/refresh", None, json!({ "refresh_token": second }))
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_plain_user_cannot_administer_the_tenant() {
    let app = App::start().await;
    let (_, user_token) = app.user("user@acme.test", "USER").await;

    app.get("/v1/users", Some(&user_token))
        .await
        .expect_status(StatusCode::FORBIDDEN);
    app.machine("not-owned-by-user").await;
    let machines = app
        .get("/v1/machines", Some(&user_token))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert!(machines.as_array().unwrap().is_empty());
    app.get("/v1/groups", Some(&user_token))
        .await
        .expect_status(StatusCode::FORBIDDEN);
    app.post(
        "/v1/machines/enrollment-tokens",
        Some(&user_token),
        json!({}),
    )
    .await
    .expect_status(StatusCode::BAD_REQUEST);

    // But their own identity and resource list are always available.
    app.get("/v1/auth/me", Some(&user_token))
        .await
        .expect_status(StatusCode::OK);
    let resources = app
        .get("/v1/resources", Some(&user_token))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(resources.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn only_an_owner_may_create_another_owner() {
    let app = App::start().await;
    let (_, admin_token) = app.user("admin@acme.test", "ADMIN").await;

    app.post(
        "/v1/users",
        Some(&admin_token),
        json!({
            "email": "usurper@acme.test",
            "password": PASSWORD,
            "display_name": "U",
            "role": "OWNER",
        }),
    )
    .await
    .expect_status(StatusCode::FORBIDDEN);

    // An admin promoting an existing account is the same escalation.
    let (victim, _) = app.user("victim@acme.test", "USER").await;
    app.patch(
        &format!("/v1/users/{victim}"),
        Some(&admin_token),
        json!({ "role": "OWNER" }),
    )
    .await
    .expect_status(StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn disabling_a_user_takes_effect_immediately() {
    let app = App::start().await;
    let (id, token) = app.user("temp@acme.test", "USER").await;
    app.get("/v1/auth/me", Some(&token))
        .await
        .expect_status(StatusCode::OK);

    app.patch(
        &format!("/v1/users/{id}"),
        Some(&app.owner_token),
        json!({ "disabled": true }),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);

    // The access token is still cryptographically valid; the live row is
    // what decides, so it must stop working right away.
    app.get("/v1/auth/me", Some(&token))
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_enrollment_token_works_once_and_pins_the_name() {
    let app = App::start().await;
    let token = app
        .post(
            "/v1/machines/enrollment-tokens",
            Some(&app.owner_token),
            json!({ "machine_name": "studio" }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();

    let enroll = |name: &str, token: &str| {
        app.post(
            "/v1/machines/enroll",
            None,
            json!({
                "token": token,
                "name": name,
                "os": "MACOS",
                "noise_public_key": hex::encode(rand_key()),
            }),
        )
    };

    // A token bound to one name must not enrol an impostor under another.
    enroll("impostor", &token)
        .await
        .expect_status(StatusCode::FORBIDDEN);
    enroll("studio", &token)
        .await
        .expect_status(StatusCode::CREATED);
    // And it is consumed, so a leaked copy is worthless.
    enroll("studio", &token)
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_machine_credential_is_not_a_user_credential() {
    let app = App::start().await;
    let (_, credential) = app.machine("laptop").await;

    // Neither as a bearer token...
    app.get("/v1/machines", Some(&credential))
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
    // ...nor for anything but its own heartbeat.
    app.post(
        "/v1/machines/heartbeat",
        None,
        json!({ "status": "ONLINE" }),
    )
    .with_machine(&credential)
    .await
    .expect_status(StatusCode::NO_CONTENT);
    app.post(
        "/v1/machines/heartbeat",
        None,
        json!({ "status": "ONLINE" }),
    )
    .with_machine("not.a-credential")
    .await
    .expect_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn published_resources_must_be_coherent() {
    let app = App::start().await;
    let (machine, _) = app.machine("workstation").await;
    let path = format!("/v1/machines/{machine}/resources");

    app.post(
        &path,
        Some(&app.owner_token),
        json!({ "kind": "APP", "name": "Xcode" }),
    )
    .await
    .expect_status(StatusCode::BAD_REQUEST);

    app.post(
        &path,
        Some(&app.owner_token),
        json!({ "kind": "DESKTOP", "name": "Desktop", "launch_path": "/bin/sh" }),
    )
    .await
    .expect_status(StatusCode::BAD_REQUEST);

    app.post(
        &path,
        Some(&app.owner_token),
        json!({
            "kind": "APP",
            "name": "Xcode",
            "launch_path": "/Applications/Xcode.app",
            "launch_args": ["--new"],
        }),
    )
    .await
    .expect_status(StatusCode::CREATED);

    // Two resources with the same name on one machine would be
    // indistinguishable in the client.
    app.post(
        &path,
        Some(&app.owner_token),
        json!({ "kind": "APP", "name": "Xcode", "launch_path": "/other" }),
    )
    .await
    .expect_status(StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_user_only_sees_resources_they_are_entitled_to() {
    let app = App::start().await;
    let (machine, resource) = app.online_machine_with_desktop("mac-studio", None).await;
    let (alice_id, alice) = app.user("alice@acme.test", "USER").await;
    let (_, bob) = app.user("bob@acme.test", "USER").await;

    assert_eq!(list_len(&app, &alice).await, 0);

    app.post(
        &format!("/v1/resources/{resource}/entitlements"),
        Some(&app.owner_token),
        json!({
            "subject_kind": "USER",
            "subject_id": alice_id,
            "role": "CONTROLLER",
            "allow_clipboard": true,
        }),
    )
    .await
    .expect_status(StatusCode::CREATED);

    let mine = app
        .get("/v1/resources", Some(&alice))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(mine.as_array().unwrap().len(), 1);
    assert_eq!(mine[0]["name"], "Desktop");
    assert_eq!(mine[0]["role"], "CONTROLLER");
    // Desktop details may identify the device; APP consumers remain independent.
    assert_eq!(mine[0]["machine_id"], machine.to_string());
    assert_eq!(mine[0]["owned"], false);

    // Bob was never granted anything.
    assert_eq!(list_len(&app, &bob).await, 0);
}

#[tokio::test]
async fn group_membership_grants_access_and_revocation_removes_it() {
    let app = App::start().await;
    let (_, resource) = app.online_machine_with_desktop("mac-mini", None).await;
    let (user_id, user_token) = app.user("carol@acme.test", "USER").await;

    let group: Uuid = app
        .post(
            "/v1/groups",
            Some(&app.owner_token),
            json!({ "name": "Designers" }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    let entitlement: Uuid = app
        .post(
            &format!("/v1/resources/{resource}/entitlements"),
            Some(&app.owner_token),
            json!({ "subject_kind": "GROUP", "subject_id": group, "role": "VIEWER" }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Entitled group, but not yet a member.
    assert_eq!(list_len(&app, &user_token).await, 0);

    app.post(
        &format!("/v1/groups/{group}/members"),
        Some(&app.owner_token),
        json!({ "user_id": user_id }),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &user_token).await, 1);

    app.delete(
        &format!("/v1/groups/{group}/members/{user_id}"),
        Some(&app.owner_token),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &user_token).await, 0);

    // Re-join, then revoke the entitlement itself.
    app.post(
        &format!("/v1/groups/{group}/members"),
        Some(&app.owner_token),
        json!({ "user_id": user_id }),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &user_token).await, 1);

    app.delete(
        &format!("/v1/entitlements/{entitlement}"),
        Some(&app.owner_token),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &user_token).await, 0);
}

#[tokio::test]
async fn a_session_ticket_carries_the_clamped_policy() {
    let app = App::start().await;
    let (gateway, _) = app.infrastructure().await;
    let (machine, resource) = app
        .online_machine_with_desktop("mac-pro", Some(gateway))
        .await;
    let (viewer_id, viewer) = app.user("viewer@acme.test", "USER").await;

    // Deliberately over-permissive entitlement on a viewer role.
    app.post(
        &format!("/v1/resources/{resource}/entitlements"),
        Some(&app.owner_token),
        json!({
            "subject_kind": "USER",
            "subject_id": viewer_id,
            "role": "VIEWER",
            "allow_clipboard": true,
            "allow_file_transfer": true,
        }),
    )
    .await
    .expect_status(StatusCode::CREATED);

    let ticket = app
        .post(
            "/v1/sessions",
            Some(&viewer),
            json!({ "resource_id": resource }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();

    assert_eq!(ticket["relay_addr"], "relay.test:7444");
    assert_eq!(ticket["gateway_addr"], "gw.test:7443");
    assert!(ticket["expires_in"].as_i64().unwrap() > 0);

    let claims = decode_claims(ticket["ticket"].as_str().unwrap());
    assert_eq!(claims["role"], "VIEWER");
    assert_eq!(claims["mid"], machine.to_string());
    assert_eq!(claims["jti"], ticket["session_id"]);
    // The role is the ceiling: the entitlement cannot hand a viewer input,
    // a clipboard or file transfer no matter what the row says.
    assert_eq!(claims["policy"]["input"], false);
    assert_eq!(claims["policy"]["clipboard"], false);
    assert_eq!(claims["policy"]["file_transfer"], false);
    // The agent key must be the one the client will encrypt to.
    assert_eq!(claims["agent_key"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn a_session_is_refused_without_an_entitlement_or_an_online_machine() {
    let app = App::start().await;
    app.infrastructure().await;
    let (_, user_token) = app.user("dave@acme.test", "USER").await;

    // Machine deliberately left offline.
    let (machine, _) = app.machine("sleeping").await;
    let resource: Uuid = app
        .post(
            &format!("/v1/machines/{machine}/resources"),
            Some(&app.owner_token),
            json!({ "kind": "DESKTOP", "name": "Desktop" }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Without an entitlement the resource must look absent, not forbidden:
    // a 403 would confirm that it exists.
    app.post(
        "/v1/sessions",
        Some(&user_token),
        json!({ "resource_id": resource }),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);

    let (dave_id, _) = app.user("dave2@acme.test", "USER").await;
    app.post(
        &format!("/v1/resources/{resource}/entitlements"),
        Some(&app.owner_token),
        json!({ "subject_kind": "USER", "subject_id": dave_id, "role": "CONTROLLER" }),
    )
    .await
    .expect_status(StatusCode::CREATED);

    let dave = app.login("dave2@acme.test", PASSWORD).await;
    app.post(
        "/v1/sessions",
        Some(&dave),
        json!({ "resource_id": resource }),
    )
    .await
    .expect_status(StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_gateway_reports_session_progress() {
    let app = App::start().await;
    let (gateway, credential) = app.infrastructure().await;
    let (_, resource) = app
        .online_machine_with_desktop("reported", Some(gateway))
        .await;
    let (user_id, user_token) = app.user("erin@acme.test", "USER").await;
    app.post(
        &format!("/v1/resources/{resource}/entitlements"),
        Some(&app.owner_token),
        json!({ "subject_kind": "USER", "subject_id": user_id, "role": "CONTROLLER" }),
    )
    .await
    .expect_status(StatusCode::CREATED);

    let session = app
        .post(
            "/v1/sessions",
            Some(&user_token),
            json!({ "resource_id": resource }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json()["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Only a node credential may report, and only with a valid state.
    app.post(
        &format!("/v1/sessions/{session}/report"),
        Some(&user_token),
        json!({ "state": "ACTIVE" }),
    )
    .await
    .expect_status(StatusCode::UNAUTHORIZED);

    app.post(
        &format!("/v1/sessions/{session}/report"),
        None,
        json!({ "state": "ACTIVE", "bytes_up": 1024, "bytes_down": 4096 }),
    )
    .with_node(&credential)
    .await
    .expect_status(StatusCode::NO_CONTENT);

    let listed = app
        .get("/v1/sessions", Some(&user_token))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(listed[0]["state"], "ACTIVE");
    assert_eq!(listed[0]["bytes_down"], 4096);
    assert!(listed[0]["started_at"].is_string());

    app.post(
        &format!("/v1/sessions/{session}/report"),
        None,
        json!({ "state": "CLOSED", "reason": "client_disconnect", "bytes_down": 1 }),
    )
    .with_node(&credential)
    .await
    .expect_status(StatusCode::NO_CONTENT);

    let listed = app
        .get("/v1/sessions", Some(&user_token))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(listed[0]["state"], "CLOSED");
    // Counters are cumulative; a late, smaller report must not rewind them.
    assert_eq!(listed[0]["bytes_down"], 4096);

    // Both ends of a session report it ending, so the second report always
    // finds the row already closed. Answering that with "no such session"
    // would fill an operator's logs with alarming, meaningless errors.
    app.post(
        &format!("/v1/sessions/{session}/report"),
        None,
        json!({ "state": "CLOSED", "reason": "agent_finished" }),
    )
    .with_node(&credential)
    .await
    .expect_status(StatusCode::NO_CONTENT);

    // A session that genuinely is not there still says so.
    app.post(
        &format!("/v1/sessions/{}/report", Uuid::now_v7()),
        None,
        json!({ "state": "CLOSED" }),
    )
    .with_node(&credential)
    .await
    .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tenants_cannot_reach_across_the_boundary() {
    let app = App::start().await;
    let other = App::start().await;

    let (machine, resource) = app.online_machine_with_desktop("private", None).await;
    let (victim_id, _) = app.user("victim@acme.test", "USER").await;

    // Another tenant's owner knows the ids but must be told nothing exists.
    let leaked = other
        .get(
            &format!("/v1/machines/{machine}/resources"),
            Some(&other.owner_token),
        )
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert!(
        leaked.as_array().unwrap().is_empty(),
        "leaked another tenant's resources"
    );

    other
        .delete(&format!("/v1/machines/{machine}"), Some(&other.owner_token))
        .await
        .expect_status(StatusCode::NOT_FOUND);
    other
        .delete(
            &format!("/v1/resources/{resource}"),
            Some(&other.owner_token),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
    other
        .patch(
            &format!("/v1/users/{victim_id}"),
            Some(&other.owner_token),
            json!({ "disabled": true }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);

    // And it cannot grant itself access to a resource it does not own.
    other
        .post(
            &format!("/v1/resources/{resource}/entitlements"),
            Some(&other.owner_token),
            json!({
                "subject_kind": "USER",
                "subject_id": other.owner_id,
                "role": "ADMIN",
            }),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);

    // The victim's own view is unchanged.
    assert_eq!(
        app.get("/v1/machines", Some(&app.owner_token))
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn an_expired_entitlement_stops_granting_access() {
    let app = App::start().await;
    let (_, resource) = app.online_machine_with_desktop("clock", None).await;
    let (user_id, user_token) = app.user("frank@acme.test", "USER").await;

    app.post(
        &format!("/v1/resources/{resource}/entitlements"),
        Some(&app.owner_token),
        json!({
            "subject_kind": "USER",
            "subject_id": user_id,
            "role": "CONTROLLER",
            "expires_at": "2000-01-01T00:00:00Z",
        }),
    )
    .await
    .expect_status(StatusCode::CREATED);

    assert_eq!(list_len(&app, &user_token).await, 0);
}

#[tokio::test]
async fn a_disabled_resource_disappears_from_the_list() {
    let app = App::start().await;
    let (_, resource) = app.online_machine_with_desktop("toggle", None).await;
    let (user_id, user_token) = app.user("grace@acme.test", "USER").await;
    app.post(
        &format!("/v1/resources/{resource}/entitlements"),
        Some(&app.owner_token),
        json!({ "subject_kind": "USER", "subject_id": user_id, "role": "CONTROLLER" }),
    )
    .await
    .expect_status(StatusCode::CREATED);
    assert_eq!(list_len(&app, &user_token).await, 1);

    app.patch(
        &format!("/v1/resources/{resource}"),
        Some(&app.owner_token),
        json!({ "enabled": false }),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &user_token).await, 0);
}

async fn list_len(app: &App, token: &str) -> usize {
    app.get("/v1/resources", Some(token))
        .await
        .expect_status(StatusCode::OK)
        .json()
        .as_array()
        .unwrap()
        .len()
}

#[tokio::test]
async fn owners_manage_only_their_current_same_tenant_devices() {
    let app = App::start().await;
    let other = App::start().await;
    let (alice_id, alice) = app.user("device-owner@acme.test", "USER").await;
    let (_, bob) = app.user("nonowner@acme.test", "USER").await;
    let (_, foreign_user) = other.user("foreign@acme.test", "USER").await;
    let (machine, credential) = app.machine_as("owned", &alice).await;
    app.machine("unassigned").await;
    let path = format!("/v1/machines/{machine}");
    let publications = format!("{path}/resources");

    let mine = app
        .get("/v1/machines", Some(&alice))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(mine.as_array().unwrap().len(), 1);
    assert_eq!(mine[0]["owner_user_id"], alice_id.to_string());
    assert_eq!(mine[0]["id"], machine.to_string());
    assert!(mine[0].get("credential_hash").is_none());
    assert_eq!(
        app.get("/v1/machines", Some(&app.owner_token))
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    app.patch(&path, Some(&alice), json!({"name": "renamed"}))
        .await
        .expect_status(StatusCode::NO_CONTENT);
    app.patch(&path, Some(&alice), json!({"name": "unassigned"}))
        .await
        .expect_status(StatusCode::CONFLICT);
    app.patch(&path, Some(&alice), json!({"name": " "}))
        .await
        .expect_status(StatusCode::BAD_REQUEST);
    app.patch(
        &path,
        Some(&alice),
        json!({"name": "x", "owner_user_id": app.owner_id}),
    )
    .await
    .expect_status(StatusCode::UNPROCESSABLE_ENTITY);
    let resource = app
        .post(
            &publications,
            Some(&alice),
            json!({"kind": "DESKTOP", "name": "Desktop"}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    let resource_path = format!("/v1/resources/{}", resource["id"].as_str().unwrap());
    let grants = format!("{resource_path}/entitlements");
    let entitlement = app
        .post(
            &grants,
            Some(&alice),
            json!({"email":"nonowner@acme.test", "role":"VIEWER"}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    let revoke = format!("/v1/entitlements/{}", entitlement["id"].as_str().unwrap());

    for outsider in [&bob, &foreign_user, &other.owner_token] {
        app.patch(&path, Some(outsider), json!({"name":"stolen"}))
            .await
            .expect_status(StatusCode::NOT_FOUND);
        app.delete(&path, Some(outsider))
            .await
            .expect_status(StatusCode::NOT_FOUND);
        app.post(
            &publications,
            Some(outsider),
            json!({"kind":"DESKTOP","name":"stolen"}),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
        assert!(app
            .get(&publications, Some(outsider))
            .await
            .expect_status(StatusCode::OK)
            .json()
            .as_array()
            .unwrap()
            .is_empty());
        app.patch(&resource_path, Some(outsider), json!({"enabled":false}))
            .await
            .expect_status(StatusCode::NOT_FOUND);
        app.delete(&resource_path, Some(outsider))
            .await
            .expect_status(StatusCode::NOT_FOUND);
        app.post(
            &grants,
            Some(outsider),
            json!({"email":"device-owner@acme.test","role":"ADMIN"}),
        )
        .await
        .expect_status(StatusCode::NOT_FOUND);
        assert!(app
            .get(&grants, Some(outsider))
            .await
            .expect_status(StatusCode::OK)
            .json()
            .as_array()
            .unwrap()
            .is_empty());
        app.delete(&revoke, Some(outsider))
            .await
            .expect_status(StatusCode::NOT_FOUND);
    }
    assert_eq!(
        app.get(&publications, Some(&alice))
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        app.get(&grants, Some(&alice))
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    app.patch(
        &resource_path,
        Some(&alice),
        json!({"name":"New desktop","description":"Updated"}),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    app.delete(&revoke, Some(&alice))
        .await
        .expect_status(StatusCode::NO_CONTENT);
    app.delete(&resource_path, Some(&alice))
        .await
        .expect_status(StatusCode::NO_CONTENT);
    app.delete(&path, Some(&alice))
        .await
        .expect_status(StatusCode::NO_CONTENT);
    app.post("/v1/machines/heartbeat", None, json!({"status":"ONLINE"}))
        .with_machine(&credential)
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn self_enrollment_is_pinned_to_self_and_default_region() {
    let app = App::start().await;
    let (owner, token) = app.user("self@acme.test", "USER").await;
    for body in [
        json!({}),
        json!({"machine_name":""}),
        json!({"machine_name":"  "}),
    ] {
        app.post("/v1/machines/enrollment-tokens", Some(&token), body)
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
    for body in [
        json!({"machine_name":"self","owner_user_id":app.owner_id}),
        json!({"machine_name":"self","region":"default"}),
        json!({"machine_name":"self","region":"elsewhere"}),
    ] {
        app.post("/v1/machines/enrollment-tokens", Some(&token), body)
            .await
            .expect_status(StatusCode::FORBIDDEN);
    }
    let enrollment = app
        .post(
            "/v1/machines/enrollment-tokens",
            Some(&token),
            json!({"machine_name":"self","owner_user_id":owner}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    app.post("/v1/machines/enroll", None, json!({
        "token":enrollment["token"],"name":"impostor","os":"MACOS","noise_public_key":hex::encode(rand_key())
    })).await.expect_status(StatusCode::FORBIDDEN);
    let machine = app.post("/v1/machines/enroll", None, json!({
        "token":enrollment["token"],"name":"self","os":"MACOS","noise_public_key":hex::encode(rand_key())
    })).await.expect_status(StatusCode::CREATED).json();
    app.post("/v1/machines/enroll", None, json!({
        "token":enrollment["token"],"name":"self","os":"MACOS","noise_public_key":hex::encode(rand_key())
    })).await.expect_status(StatusCode::UNAUTHORIZED);
    let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
    let row: (Uuid, String) =
        sqlx::query_as("SELECT owner_user_id, region FROM machines WHERE id=$1")
            .bind(
                machine["machine_id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            )
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, (owner, "default".to_string()));
    app.patch(
        &format!("/v1/users/{owner}"),
        Some(&app.owner_token),
        json!({"disabled":true}),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    app.post(
        "/v1/machines/enrollment-tokens",
        Some(&token),
        json!({"machine_name":"disabled"}),
    )
    .await
    .expect_status(StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn owner_grants_use_exact_enabled_same_tenant_recipients_without_directory_access() {
    let app = App::start().await;
    let other = App::start().await;
    let (_, owner) = app.user("publisher@acme.test", "USER").await;
    let (recipient_id, recipient) = app.user("Recipient@acme.test", "USER").await;
    let (foreign_id, _) = other.user("Recipient@acme.test", "USER").await;
    other.user("foreign-only@acme.test", "USER").await;
    let (machine, _) = app.machine_as("grant-device", &owner).await;
    let resource = app
        .post(
            &format!("/v1/machines/{machine}/resources"),
            Some(&owner),
            json!({"kind":"DESKTOP","name":"Desktop"}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    let path = format!(
        "/v1/resources/{}/entitlements",
        resource["id"].as_str().unwrap()
    );
    let grant = app
        .post(
            &path,
            Some(&owner),
            json!({"email":" recipient@acme.test ","role":"CONTROLLER","allow_file_transfer":true}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    assert_eq!(grant["subject_id"], recipient_id.to_string());
    assert_eq!(grant["subject_kind"], "USER");
    assert_eq!(grant["user_email"], "Recipient@acme.test");
    assert_eq!(grant["user_display_name"], "Recipient@acme.test");
    assert!(grant["group_name"].is_null());
    let grants = app
        .get(&path, Some(&owner))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(grants[0], grant);
    assert!(app
        .get(&path, Some(&other.owner_token))
        .await
        .expect_status(StatusCode::OK)
        .json()
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(list_len(&app, &recipient).await, 1);
    for body in [
        json!({"email":"Recipient","role":"ADMIN"}),
        json!({"email":"foreign-only@acme.test","role":"ADMIN"}),
        json!({"user_id":foreign_id,"role":"ADMIN"}),
        json!({"subject_kind":"USER","subject_id":foreign_id,"role":"ADMIN"}),
        json!({"group_id":Uuid::now_v7(),"role":"ADMIN"}),
    ] {
        app.post(&path, Some(&owner), body)
            .await
            .expect_status(StatusCode::NOT_FOUND);
    }
    for body in [
        json!({"email":"Recipient@acme.test","user_id":recipient_id,"role":"ADMIN"}),
        json!({"subject_kind":"USER","role":"ADMIN"}),
    ] {
        app.post(&path, Some(&owner), body)
            .await
            .expect_status(StatusCode::BAD_REQUEST);
    }
    app.post(
        &path,
        Some(&owner),
        json!({"user_id":recipient_id,"role":"VIEWER"}),
    )
    .await
    .expect_status(StatusCode::CREATED);
    app.get("/v1/users", Some(&owner))
        .await
        .expect_status(StatusCode::FORBIDDEN);
    app.get("/v1/groups", Some(&owner))
        .await
        .expect_status(StatusCode::FORBIDDEN);
    app.delete(
        &format!("/v1/entitlements/{}", grant["id"].as_str().unwrap()),
        Some(&owner),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &recipient).await, 0);
    app.get(
        &format!("/v1/resources/{}", resource["id"].as_str().unwrap()),
        Some(&recipient),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.post(
        "/v1/sessions",
        Some(&recipient),
        json!({"resource_id":resource["id"]}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.patch(
        &format!("/v1/users/{recipient_id}"),
        Some(&app.owner_token),
        json!({"disabled":true}),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    app.post(
        &path,
        Some(&owner),
        json!({"email":"recipient@acme.test","role":"CONTROLLER"}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn owner_access_and_mutations_follow_live_ownership_not_stale_tokens() {
    let app = App::start().await;
    let (_, alice) = app.user("old-owner@acme.test", "USER").await;
    let (bob_id, bob) = app.user("new-owner@acme.test", "USER").await;
    let (gateway, _) = app.infrastructure().await;
    let (machine, credential) = app.machine_as("transfer-owner", &alice).await;
    app.post(
        "/v1/machines/heartbeat",
        None,
        json!({"status":"ONLINE","gateway_id":gateway}),
    )
    .with_machine(&credential)
    .await
    .expect_status(StatusCode::NO_CONTENT);
    let resource = app
        .post(
            &format!("/v1/machines/{machine}/resources"),
            Some(&alice),
            json!({"kind":"DESKTOP","name":"Desktop"}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    let path = format!("/v1/resources/{}", resource["id"].as_str().unwrap());
    let detail = app
        .get(&path, Some(&alice))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(detail["owned"], true);
    assert_eq!(detail["owner_name"], "old-owner@acme.test");
    assert_eq!(detail["role"], "ADMIN");
    assert_eq!(
        detail["policy"],
        json!({"input":true,"audio":true,"clipboard":true,"file_transfer":true})
    );
    let ticket = app
        .post(
            "/v1/sessions",
            Some(&alice),
            json!({"resource_id":resource["id"]}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    assert_eq!(ticket["policy"], detail["policy"]);
    assert_eq!(
        decode_claims(ticket["ticket"].as_str().unwrap())["policy"],
        detail["policy"]
    );
    assert_eq!(
        list_len(&app, &app.owner_token).await,
        0,
        "tenant admin is not automatically entitled"
    );
    app.post(
        "/v1/sessions",
        Some(&app.owner_token),
        json!({"resource_id":resource["id"]}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    let entitlement = app
        .post(
            &format!("{path}/entitlements"),
            Some(&alice),
            json!({"user_id":app.owner_id,"role":"VIEWER"}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
    sqlx::query("UPDATE machines SET owner_user_id=$2 WHERE id=$1")
        .bind(machine)
        .bind(bob_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(list_len(&app, &alice).await, 0);
    assert_eq!(list_len(&app, &bob).await, 1);
    app.get(&path, Some(&alice))
        .await
        .expect_status(StatusCode::NOT_FOUND);
    app.post(
        "/v1/sessions",
        Some(&alice),
        json!({"resource_id":resource["id"]}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.patch(
        &format!("/v1/machines/{machine}"),
        Some(&alice),
        json!({"name":"stolen"}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.delete(&format!("/v1/machines/{machine}"), Some(&alice))
        .await
        .expect_status(StatusCode::NOT_FOUND);
    app.post(
        &format!("/v1/machines/{machine}/resources"),
        Some(&alice),
        json!({"kind":"DESKTOP","name":"Stolen"}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.patch(&path, Some(&alice), json!({"enabled":false}))
        .await
        .expect_status(StatusCode::NOT_FOUND);
    app.delete(&path, Some(&alice))
        .await
        .expect_status(StatusCode::NOT_FOUND);
    app.post(
        &format!("{path}/entitlements"),
        Some(&alice),
        json!({"user_id":bob_id,"role":"ADMIN"}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.delete(
        &format!("/v1/entitlements/{}", entitlement["id"].as_str().unwrap()),
        Some(&alice),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
    app.patch(&path, Some(&bob), json!({"enabled":false}))
        .await
        .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &bob).await, 0);
    app.post(
        "/v1/sessions",
        Some(&bob),
        json!({"resource_id":resource["id"]}),
    )
    .await
    .expect_status(StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn app_consumers_never_receive_launch_metadata_or_a_desktop_session() {
    let app = App::start().await;
    let (_, owner) = app.user("app-owner@acme.test", "USER").await;
    let (_, consumer) = app.user("app-consumer@acme.test", "USER").await;
    let (machine, credential) = app.machine_as("secret-machine", &owner).await;
    app.post("/v1/machines/heartbeat", None, json!({"status":"ONLINE"}))
        .with_machine(&credential)
        .await
        .expect_status(StatusCode::NO_CONTENT);
    let publications = format!("/v1/machines/{machine}/resources");
    let resource = app
        .post(
            &publications,
            Some(&owner),
            json!({
                "kind":"APP","name":"Editor","launch_path":"/private/editor",
                "launch_args":["--private-argument"],"working_dir":"/private/project",
                "window_match":{"title":"private-title"}
            }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    let path = format!("/v1/resources/{}", resource["id"].as_str().unwrap());
    app.patch(&path,Some(&owner),json!({"launch_path":"/private/new-editor","launch_args":["--new"],"working_dir":"/private/new"}))
        .await.expect_status(StatusCode::NO_CONTENT);
    let managed = app
        .get(&publications, Some(&owner))
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(managed[0]["launch_path"], "/private/new-editor");
    assert!(app
        .get(&publications, Some(&consumer))
        .await
        .expect_status(StatusCode::OK)
        .json()
        .as_array()
        .unwrap()
        .is_empty());
    app.patch(&path, Some(&consumer), json!({"launch_path":"/bin/sh"}))
        .await
        .expect_status(StatusCode::NOT_FOUND);
    app.post(
        &format!("{path}/entitlements"),
        Some(&owner),
        json!({"email":"app-consumer@acme.test","role":"CONTROLLER"}),
    )
    .await
    .expect_status(StatusCode::CREATED);
    for caller in [&consumer, &owner] {
        let list = app
            .get("/v1/resources", Some(caller))
            .await
            .expect_status(StatusCode::OK)
            .json();
        let detail = app
            .get(&path, Some(caller))
            .await
            .expect_status(StatusCode::OK)
            .json();
        assert_eq!(list[0], detail);
        assert_eq!(detail["owned"], false);
        assert_eq!(detail["launch_supported"], false);
        for field in [
            "machine_id",
            "os",
            "machine_os",
            "os_version",
            "last_seen_at",
        ] {
            assert!(detail.get(field).unwrap().is_null(), "{field} leaked");
        }
        for field in [
            "launch_path",
            "launch_args",
            "working_dir",
            "window_match",
            "capabilities",
            "noise_public_key",
        ] {
            assert!(detail.get(field).is_none(), "{field} leaked");
        }
        let error = app
            .post(
                "/v1/sessions",
                Some(caller),
                json!({"resource_id":resource["id"]}),
            )
            .await
            .expect_status(StatusCode::CONFLICT)
            .json();
        assert!(error.to_string().contains("isolated APP streaming"));
    }
    assert!(app
        .get("/v1/machines", Some(&consumer))
        .await
        .json()
        .as_array()
        .unwrap()
        .is_empty());
    let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE resource_id=$1")
        .bind(resource["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
}

#[tokio::test]
async fn overlapping_grants_resolve_once_and_match_list_detail_and_admission() {
    let app = App::start().await;
    let (gateway, _) = app.infrastructure().await;
    let (_, resource) = app
        .online_machine_with_desktop("overlapping", Some(gateway))
        .await;
    let (user_id, user) = app.user("multiple@acme.test", "USER").await;
    let group = app
        .post(
            "/v1/groups",
            Some(&app.owner_token),
            json!({"name":"Group"}),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    app.post(
        &format!("/v1/groups/{}/members", group["id"].as_str().unwrap()),
        Some(&app.owner_token),
        json!({"user_id":user_id}),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    let path = format!("/v1/resources/{resource}");
    let group_grant = app.post(&format!("{path}/entitlements"),Some(&app.owner_token),json!({
        "group_id":group["id"],"role":"CONTROLLER","allow_clipboard":false,"allow_file_transfer":true
    })).await.expect_status(StatusCode::CREATED).json();
    let direct = app.post(&format!("{path}/entitlements"),Some(&app.owner_token),json!({
        "subject_kind":"USER","subject_id":user_id,"role":"CONTROLLER","allow_clipboard":true,"allow_file_transfer":false
    })).await.expect_status(StatusCode::CREATED).json();
    for (role, clipboard, files) in [
        ("CONTROLLER", true, false),
        ("CONTROLLER", false, true),
        ("VIEWER", false, false),
    ] {
        let list = app
            .get("/v1/resources", Some(&user))
            .await
            .expect_status(StatusCode::OK)
            .json();
        assert_eq!(list.as_array().unwrap().len(), 1);
        let detail = app
            .get(&path, Some(&user))
            .await
            .expect_status(StatusCode::OK)
            .json();
        assert_eq!(detail, list[0]);
        assert_eq!(detail["role"], role);
        assert_eq!(detail["allow_clipboard"], clipboard);
        assert_eq!(detail["allow_file_transfer"], files);
        let session = app
            .post("/v1/sessions", Some(&user), json!({"resource_id":resource}))
            .await
            .expect_status(StatusCode::CREATED)
            .json();
        assert_eq!(group_grant["group_name"], "Group");
        assert!(group_grant["user_email"].is_null());
        assert!(group_grant["user_display_name"].is_null());
        assert_eq!(session["policy"], detail["policy"]);
        assert_eq!(
            decode_claims(session["ticket"].as_str().unwrap())["policy"],
            detail["policy"]
        );
        if clipboard {
            app.delete(
                &format!("/v1/entitlements/{}", direct["id"].as_str().unwrap()),
                Some(&app.owner_token),
            )
            .await
            .expect_status(StatusCode::NO_CONTENT);
        } else if files {
            app.post(&format!("{path}/entitlements"),Some(&app.owner_token),json!({
                "group_id":group["id"],"role":"VIEWER","allow_clipboard":true,"allow_file_transfer":true,"allow_audio":true
            })).await.expect_status(StatusCode::CREATED);
        }
    }
    app.delete(
        &format!("/v1/entitlements/{}", group_grant["id"].as_str().unwrap()),
        Some(&app.owner_token),
    )
    .await
    .expect_status(StatusCode::NO_CONTENT);
    assert_eq!(list_len(&app, &user).await, 0);
    app.get(&path, Some(&user))
        .await
        .expect_status(StatusCode::NOT_FOUND);
}

/// Decode a JWT payload without verifying: the signature is checked by
/// `TicketSigner`'s own tests, and here we only care about the contents.
fn decode_claims(token: &str) -> Value {
    use base64::Engine as _;
    let payload = token.split('.').nth(1).expect("a JWT has three parts");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .expect("the payload should be base64url");
    serde_json::from_slice(&bytes).expect("the payload should be JSON")
}

/// Register a gateway in a named region and return `(id, credential)`.
async fn gateway_in(app: &App, region: &str) -> (Uuid, String) {
    let suffix = Uuid::now_v7().simple().to_string();
    let body = app
        .post(
            "/v1/gateways",
            Some(BOOTSTRAP),
            json!({
                "name": format!("gw-{suffix}"),
                "public_url": "https://gw.test",
                "quic_addr": format!("{region}.gw.test:7443"),
                "cert_pin": "ab".repeat(32),
                "region": region,
            }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json();
    (
        body["id"].as_str().unwrap().parse().unwrap(),
        body["credential"].as_str().unwrap().to_string(),
    )
}

/// Enrol a machine into a region and return its credential.
async fn machine_in(app: &App, region: &str) -> String {
    let name = format!("m{}", Uuid::now_v7().simple());
    let token = app
        .post(
            "/v1/machines/enrollment-tokens",
            Some(&app.owner_token),
            json!({ "machine_name": name, "region": region }),
        )
        .await
        .expect_status(StatusCode::CREATED)
        .json()["token"]
        .as_str()
        .unwrap()
        .to_string();

    app.post(
        "/v1/machines/enroll",
        None,
        json!({
            "token": token,
            "name": name,
            "os": "MACOS",
            "os_version": "26.0",
            "arch": "arm64",
            "agent_version": "0.1.0",
            "noise_public_key": hex::encode(rand_key()),
        }),
    )
    .await
    .expect_status(StatusCode::CREATED)
    .json()["credential"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Age a node's last report, standing in for a process that has stopped.
async fn silence_gateway(id: Uuid, seconds: i64) {
    let pool = sqlx::PgPool::connect(&database_url()).await.unwrap();
    sqlx::query(
        "UPDATE gateways SET last_seen_at = now() - make_interval(secs => $2) WHERE id = $1",
    )
    .bind(id)
    .bind(seconds as f64)
    .execute(&pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_machine_is_sent_to_a_gateway_in_its_own_region() {
    let app = App::start().await;
    let region = format!("r{}", Uuid::now_v7().simple());
    let (elsewhere, _) = gateway_in(&app, &format!("far-{region}")).await;
    let (home, _) = gateway_in(&app, &region).await;

    let credential = machine_in(&app, &region).await;
    let assigned = app
        .get("/v1/machines/self/gateway", None)
        .with_machine(&credential)
        .await
        .expect_status(StatusCode::OK)
        .json();

    let id: Uuid = assigned["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(id, home, "a machine must not be sent across the world");
    assert_ne!(id, elsewhere);
}

#[tokio::test]
async fn a_gateway_that_has_stopped_reporting_is_not_handed_out() {
    let app = App::start().await;
    let region = format!("r{}", Uuid::now_v7().simple());
    let (dead, _) = gateway_in(&app, &region).await;
    silence_gateway(dead, 600).await;

    let credential = machine_in(&app, &region).await;

    // A dead gateway must never be offered, not even as the only one in the
    // machine's own region: its address would simply never answer, and the
    // agent has no way to tell that from a network fault. Being sent to a
    // live gateway elsewhere is the correct outcome; being sent to this one
    // is not.
    let response = app
        .get("/v1/machines/self/gateway", None)
        .with_machine(&credential)
        .await;
    if response.status == StatusCode::OK {
        let assigned: Uuid = response.json()["id"].as_str().unwrap().parse().unwrap();
        assert_ne!(assigned, dead, "a silent gateway must not be handed out");
    } else {
        assert_eq!(response.status, StatusCode::CONFLICT);
    }

    // A heartbeat brings it back without re-registration.
    let (alive, node_credential) = gateway_in(&app, &region).await;
    silence_gateway(alive, 600).await;
    app.post("/v1/nodes/self/heartbeat", None, json!({ "load": 3 }))
        .with_node(&node_credential)
        .await
        .expect_status(StatusCode::NO_CONTENT);

    let assigned = app
        .get("/v1/machines/self/gateway", None)
        .with_machine(&credential)
        .await
        .expect_status(StatusCode::OK)
        .json();
    assert_eq!(
        assigned["id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        alive
    );
}

#[tokio::test]
async fn only_a_node_may_report_node_liveness() {
    let app = App::start().await;
    let (_, machine_credential) = app.machine("beat-impostor").await;

    app.post(
        "/v1/nodes/self/heartbeat",
        Some(&app.owner_token),
        json!({}),
    )
    .await
    .expect_status(StatusCode::UNAUTHORIZED);

    app.post("/v1/nodes/self/heartbeat", None, json!({}))
        .with_machine(&machine_credential)
        .await
        .expect_status(StatusCode::UNAUTHORIZED);
}
