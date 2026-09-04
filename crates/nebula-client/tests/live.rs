//! The client library against the real thing.
//!
//! The agent's own live test proves the system works when the client is
//! written inline in the test. This proves it works when the client is the
//! code that actually ships: the same sign-in, the same resource list, the
//! same ticket redemption, the same handshake.
//!
//! Video is left as the test pattern here. Decoding is exercised where it
//! belongs — against a real encoder — and a machine running CI has no display
//! to capture and no permission to capture it with.
//!
//! Point `NEBULA_TEST_DATABASE_URL` at a scratch database; the default is a
//! local `nebula_manager_test`.

use std::sync::Arc;
use std::time::Duration;

use ndp_proto::{Channel, MsgFlags, MsgHeader, MsgKind};
use nebula_agent::{enroll, Agent, TestPattern};
use nebula_client::ManagerClient;
use nebula_gateway::{Config as GatewayConfig, Gateway};
use nebula_relay::{Config as RelayConfig, Relay};
use serde_json::{json, Value};
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";
const BOOTSTRAP: &str = "test-bootstrap-token";

struct Deployment {
    http: reqwest::Client,
    manager_url: String,
    owner_token: String,
    slug: String,
    gateway: Arc<Gateway>,
    _relay: Arc<Relay>,
    region: String,
}

impl Deployment {
    async fn start() -> Self {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let db = std::env::var("NEBULA_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres:///nebula_manager_test".into());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager_url = format!("http://{}", listener.local_addr().unwrap());
        let state = nebula_manager::AppState::bootstrap(nebula_manager::Config {
            public_url: manager_url.clone(),
            ..nebula_manager::Config::for_test(db)
        })
        .await
        .expect("the manager should start against the test database");
        tokio::spawn(async move {
            let _ = axum::serve(listener, nebula_manager::routes::router(state)).await;
        });

        // A region of its own, so placement cannot pick up another test's
        // nodes from the shared database.
        let region = format!("r{}", Uuid::now_v7().simple());
        let pair_secret = ndp_signal::generate_secret();
        let http = reqwest::Client::new();

        let bound = Relay::bind(&RelayConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            pair_secret: pair_secret.clone(),
            ..RelayConfig::default()
        })
        .unwrap();
        let registered: Value = http
            .post(format!("{manager_url}/v1/relays"))
            .bearer_auth(BOOTSTRAP)
            .json(&json!({
                "name": format!("relay-{region}"),
                "quic_addr": bound.local_addr().unwrap().to_string(),
                "cert_pin": bound.fingerprint,
                "region": region,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let relay = Arc::new(
            bound
                .with_liveness(
                    &manager_url,
                    registered["credential"].as_str().unwrap(),
                    Duration::from_secs(5),
                )
                .unwrap(),
        );
        let serving = Arc::clone(&relay);
        tokio::spawn(async move { serving.run().await });

        let gateway = Gateway::start(GatewayConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            advertised_addr: String::new(),
            name: format!("gw-{region}"),
            manager_url: manager_url.clone(),
            bootstrap_secret: Some(BOOTSTRAP.into()),
            region: region.clone(),
            pair_secret,
            ..GatewayConfig::default()
        })
        .await
        .expect("the gateway should start");
        let serving = Arc::clone(&gateway);
        tokio::spawn(async move { serving.run().await });

        let slug = format!("t{}", Uuid::now_v7().simple());
        let mut deployment = Self {
            http,
            manager_url,
            owner_token: String::new(),
            slug: slug.clone(),
            gateway,
            _relay: relay,
            region,
        };
        deployment
            .post(
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
            .await;
        deployment.owner_token = deployment
            .post(
                "/v1/auth/login",
                None,
                json!({ "tenant": slug, "email": "owner@acme.test", "password": PASSWORD }),
            )
            .await["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        deployment
    }

    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> Value {
        let mut request = self.http.post(format!("{}{path}", self.manager_url));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.json(&body).send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "POST {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.manager_url))
            .bearer_auth(&self.owner_token)
            .send()
            .await
            .unwrap();
        serde_json::from_str(&response.text().await.unwrap()).unwrap_or(Value::Null)
    }

    /// Enrol an agent and publish its desktop to the owner.
    async fn publish_a_desktop(&self, name: &str) -> String {
        let token = self
            .post(
                "/v1/machines/enrollment-tokens",
                Some(&self.owner_token),
                json!({ "machine_name": name, "region": self.region }),
            )
            .await["token"]
            .as_str()
            .unwrap()
            .to_string();

        let dir = tempfile::tempdir().unwrap();
        let identity = enroll(
            &self.manager_url,
            &token,
            name,
            &dir.path().join("agent.json"),
        )
        .await
        .expect("the machine should enrol");
        let machine = identity.machine_id;

        let agent = Agent::new(identity, Arc::new(TestPattern)).unwrap();
        tokio::spawn(async move { agent.run().await });

        tokio::time::timeout(Duration::from_secs(10), async {
            while self.gateway.tunnels().is_empty().await {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the agent should attach a control tunnel");

        let resource = self
            .post(
                &format!("/v1/machines/{machine}/resources"),
                Some(&self.owner_token),
                json!({ "kind": "DESKTOP", "name": name }),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();
        let me = self.get("/v1/auth/me").await;
        self.post(
            &format!("/v1/resources/{resource}/entitlements"),
            Some(&self.owner_token),
            json!({ "subject_kind": "USER", "subject_id": me["id"], "role": "CONTROLLER" }),
        )
        .await;

        // The agent keeps reading its identity file, so the directory has to
        // outlive this function.
        std::mem::forget(dir);
        resource
    }
}

#[tokio::test]
async fn the_shipped_client_reaches_a_real_agent() {
    let deployment = Deployment::start().await;
    let name = format!("mac-{}", Uuid::now_v7().simple());
    let resource_id = deployment.publish_a_desktop(&name).await;

    // From here on, only the client's own public API is used.
    let client = ManagerClient::login(
        &deployment.manager_url,
        &deployment.slug,
        "owner@acme.test",
        PASSWORD,
    )
    .await
    .expect("the owner should be able to sign in");

    let resources = client.resources().await.unwrap();
    let resource = resources
        .iter()
        .find(|r| r.id == resource_id)
        .expect("the published desktop should be listed");
    assert_eq!(resource.name, name);
    assert!(
        resource.is_online(),
        "a machine with a live tunnel must read as online"
    );

    let ticket = client.open(&resource_id).await.unwrap();
    let connected = nebula_client::connect_to_agent(&ticket)
        .await
        .expect("the client should reach the agent through gateway and relay");

    // The first video frame a joining client sees must be decodable on its
    // own, or there is nothing to show until the next keyframe.
    let mut incoming = connected.incoming;
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message = incoming.recv().await.unwrap().unwrap();
            if message.channel == Channel::Video {
                return message;
            }
        }
    })
    .await
    .expect("the agent should send video");
    assert!(first.header.flags.contains(MsgFlags::KEYFRAME));
    assert!(!first.payload.is_empty());
}

/// The gateway learns that a client has gone by watching the signalling
/// connection close, so a client that redeems its ticket and then drops that
/// connection has told the gateway it left. The session then dies seconds
/// after it starts, which looks like a network fault and is not one.
#[tokio::test]
async fn a_session_outlives_the_ticket_redemption() {
    let deployment = Deployment::start().await;
    let name = format!("mac-{}", Uuid::now_v7().simple());
    let resource_id = deployment.publish_a_desktop(&name).await;

    let client = ManagerClient::login(
        &deployment.manager_url,
        &deployment.slug,
        "owner@acme.test",
        PASSWORD,
    )
    .await
    .unwrap();
    let ticket = client.open(&resource_id).await.unwrap();
    let connected = nebula_client::connect_to_agent(&ticket).await.unwrap();
    let nebula_client::Connected {
        session,
        mut incoming,
        gateway,
    } = connected;

    // Long enough that a teardown triggered by redemption would have landed.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // A round trip, not an arriving frame: frames already queued would keep
    // arriving for a while after the agent stopped serving, so they prove
    // nothing about whether the session is still there.
    session
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 0, 0),
            b"alive?",
        )
        .await
        .expect("the session must still be open two seconds in");

    let pong = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = incoming
                .recv()
                .await
                .expect("the session must not have been torn down")
                .unwrap();
            if message.header.kind == MsgKind::Pong {
                return message;
            }
        }
    })
    .await;
    assert!(
        pong.is_ok(),
        "the agent should still answer two seconds into a session"
    );
    drop(gateway);
}

#[tokio::test]
async fn a_wrong_password_is_refused_in_terms_a_person_can_act_on() {
    let deployment = Deployment::start().await;
    let error = ManagerClient::login(
        &deployment.manager_url,
        &deployment.slug,
        "owner@acme.test",
        "not the password",
    )
    .await
    .expect_err("the wrong password must not sign in")
    .to_string();
    assert!(
        error.contains("not accepted"),
        "the message should say what went wrong, got: {error}"
    );
}

/// Audio is carried in datagrams on its own channel, so it can be lost
/// without holding up the picture — and can also be silently absent without
/// anything failing. This is the check that it is not.
#[tokio::test]
async fn a_session_carries_audio_the_client_can_decode() {
    let deployment = Deployment::start().await;
    let name = format!("mac-{}", Uuid::now_v7().simple());
    let resource_id = deployment.publish_a_desktop(&name).await;

    let client = ManagerClient::login(
        &deployment.manager_url,
        &deployment.slug,
        "owner@acme.test",
        PASSWORD,
    )
    .await
    .unwrap();

    let ticket = client.open(&resource_id).await.unwrap();
    let connected = nebula_client::connect_to_agent(&ticket).await.unwrap();
    let mut incoming = connected.incoming;
    let packet = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message = incoming.recv().await.unwrap().unwrap();
            if message.channel == Channel::Audio {
                return message;
            }
        }
    })
    .await
    .expect("the agent should send audio");

    // Decoding it here rather than checking it is non-empty: bytes on the
    // audio channel prove carriage, not that anything would come out of a
    // speaker.
    let (info, opus) = ndp_proto::AudioFrameInfo::split(&packet.payload)
        .expect("the payload should describe itself");
    assert_eq!(info.channels, 2);
    assert_eq!(info.frame_ms, 20);

    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0.0f32; 960 * 2];
    let frames = decoder
        .decode_float(opus, &mut pcm, false)
        .expect("a real decoder should accept what the agent sent");
    assert_eq!(frames, 960, "a 20 ms packet at 48 kHz is 960 samples");
    assert!(
        pcm[..frames * 2].iter().any(|s| s.abs() > 0.001),
        "the decoded audio should not be silence"
    );
}
