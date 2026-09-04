//! The whole product, running in one process.
//!
//! A real manager on Postgres, a real gateway, a real relay, the real agent —
//! not a stand-in — and a client that starts with nothing but a password.
//! What it proves is the thing the architecture exists for: a user launches a
//! resource and ends up decrypting frames from a machine whose address they
//! were never told, over a path where no intermediary can read them.
//!
//! Point `NEBULA_TEST_DATABASE_URL` at a scratch database; the default is a
//! local `nebula_manager_test`.

use std::sync::Arc;
use std::time::Duration;

use ndp_crypto::{Initiator, PublicKey, StaticKeypair};
use ndp_proto::{Channel, InputEvent, Modifiers, MsgFlags, MsgHeader, MsgKind};
use ndp_signal::{ClientHello, ClientHelloAck, RelayHello, RelayHelloAck};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Session, TransportConfig, ALPN_RELAY,
    ALPN_SESSION,
};
use nebula_agent::{enroll, session::prologue, Agent, Identity, TestPattern};
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
    gateway: Arc<Gateway>,
    relay: Arc<Relay>,
    region: String,
}

impl Deployment {
    async fn start() -> Self {
        Self::with_heartbeat(Duration::from_secs(20)).await
    }

    async fn with_heartbeat(heartbeat: Duration) -> Self {
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
        let router = nebula_manager::routes::router(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // A region of its own, so placement cannot pick up another test's
        // nodes from the shared database.
        let region = format!("r{}", Uuid::now_v7().simple());
        let pair_secret = ndp_signal::generate_secret();

        // Bind first so the relay can be registered at the address it really
        // listens on, then hand it the credential so it keeps itself alive in
        // the manager's view.
        let bound = Relay::bind(&RelayConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            pair_secret: pair_secret.clone(),
            ..RelayConfig::default()
        })
        .unwrap();

        let http = reqwest::Client::new();
        let response = http
            .post(format!("{manager_url}/v1/relays"))
            .header("Authorization", format!("Bearer {BOOTSTRAP}"))
            .json(&json!({
                "name": format!("relay-{region}"),
                "quic_addr": bound.local_addr().unwrap().to_string(),
                "cert_pin": bound.fingerprint,
                "region": region,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let registered: Value = response.json().await.unwrap();

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
            heartbeat,
            ..GatewayConfig::default()
        })
        .await
        .expect("the gateway should start");
        let serving = Arc::clone(&gateway);
        tokio::spawn(async move { serving.run().await });

        let mut deployment = Self {
            http,
            manager_url,
            owner_token: String::new(),
            gateway,
            relay,
            region,
        };
        deployment.owner_token = deployment.create_tenant().await;
        deployment
    }

    async fn create_tenant(&self) -> String {
        let slug = format!("t{}", Uuid::now_v7().simple());
        self.post(
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
        self.post(
            "/v1/auth/login",
            None,
            json!({ "tenant": slug, "email": "owner@acme.test", "password": PASSWORD }),
        )
        .await["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> Value {
        let mut request = self.http.post(format!("{}{path}", self.manager_url));
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = request.json(&body).send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "POST {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str, token: &str) -> Value {
        let response = self
            .http
            .get(format!("{}{path}", self.manager_url))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        assert!(status.is_success(), "GET {path} -> {status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }
}

/// The manager decides whether a machine may be offered from `last_seen_at`,
/// so an agent that never reports goes offline while it is plainly running,
/// and stays unreachable until something restarts it.
#[tokio::test]
async fn an_attached_agent_keeps_reporting_that_it_is_alive() {
    let deployment = Deployment::with_heartbeat(Duration::from_secs(1)).await;
    let name = format!("mac-{}", Uuid::now_v7().simple());
    let region = deployment.region.clone();
    let token = deployment
        .post(
            "/v1/machines/enrollment-tokens",
            Some(&deployment.owner_token),
            json!({ "machine_name": name, "region": region }),
        )
        .await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let dir = tempfile::tempdir().unwrap();
    let identity = enroll(
        &deployment.manager_url,
        &token,
        &name,
        &dir.path().join("agent.json"),
    )
    .await
    .unwrap();
    let machine = identity.machine_id;
    let agent = Agent::new(identity, Arc::new(TestPattern)).unwrap();
    tokio::spawn(async move { agent.run().await });

    tokio::time::timeout(Duration::from_secs(10), async {
        while deployment.gateway.tunnels().is_empty().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the agent should attach");

    let seen_at = |deployment: &Deployment| {
        let token = deployment.owner_token.clone();
        let url = deployment.manager_url.clone();
        let http = deployment.http.clone();
        async move {
            let machines: Value = http
                .get(format!("{url}/v1/machines"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            machines
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == machine.to_string())
                .expect("the enrolled machine should be listed")["last_seen_at"]
                .as_str()
                .expect("an attached machine has been seen")
                .to_string()
        }
    };

    let first = seen_at(&deployment).await;
    // Several heartbeat intervals, so a single missed one does not fail this.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let later = seen_at(&deployment).await;
    assert!(
        later > first,
        "the agent should keep reporting liveness: {first} then {later}"
    );
}

#[tokio::test]
async fn the_real_agent_serves_a_real_client() {
    let deployment = Deployment::start().await;

    // The administrator issues an enrolment token, exactly as they would for
    // a machine being set up on someone's desk.
    let name = format!("mac-{}", Uuid::now_v7().simple());
    let region = deployment.region.clone();
    let token = deployment
        .post(
            "/v1/machines/enrollment-tokens",
            Some(&deployment.owner_token),
            json!({ "machine_name": name, "region": region }),
        )
        .await["token"]
        .as_str()
        .unwrap()
        .to_string();

    // The agent enrols itself, generating and keeping its own Noise identity.
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("agent.json");
    let identity = enroll(&deployment.manager_url, &token, &name, &state)
        .await
        .expect("the machine should enrol");
    assert!(
        Identity::load(&state).unwrap().is_some(),
        "the identity must be on disk before the agent claims to be enrolled"
    );

    let agent = Agent::new(identity.clone(), Arc::new(TestPattern)).unwrap();
    let agent_key = agent.public_key();
    tokio::spawn(async move { agent.run().await });

    // Give the agent time to ask for a gateway and attach.
    let attached = tokio::time::timeout(Duration::from_secs(10), async {
        while deployment.gateway.tunnels().is_empty().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(attached.is_ok(), "the agent should attach a control tunnel");

    // The administrator publishes the machine's desktop and grants it.
    let machine = identity.machine_id;
    let resource = deployment
        .post(
            &format!("/v1/machines/{machine}/resources"),
            Some(&deployment.owner_token),
            json!({ "kind": "DESKTOP", "name": "Desktop" }),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let me = deployment.get("/v1/auth/me", &deployment.owner_token).await;
    deployment
        .post(
            &format!("/v1/resources/{resource}/entitlements"),
            Some(&deployment.owner_token),
            json!({
                "subject_kind": "USER",
                "subject_id": me["id"],
                "role": "CONTROLLER",
            }),
        )
        .await;

    // From here on this is the client, which knows a password and a resource
    // id and nothing else.
    let available = deployment
        .get("/v1/resources", &deployment.owner_token)
        .await;
    assert_eq!(available.as_array().unwrap().len(), 1);
    assert_eq!(available[0]["machine_status"], "ONLINE");
    assert!(
        available[0].get("machine_id").is_none(),
        "the client is never told which machine serves a resource"
    );

    let ticket = deployment
        .post(
            "/v1/sessions",
            Some(&deployment.owner_token),
            json!({ "resource_id": resource, "client_os": "MACOS" }),
        )
        .await;
    assert_eq!(ticket["agent_key"].as_str().unwrap(), agent_key);

    let client_keys = StaticKeypair::generate();
    let config = TransportConfig::default();

    // Redeem the ticket at the gateway.
    let gw_endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let gw_conn = connect(
        &gw_endpoint,
        ticket["gateway_addr"].as_str().unwrap().parse().unwrap(),
        "localhost",
        CertificateFingerprint::from_hex(ticket["gateway_pin"].as_str().unwrap()).unwrap(),
        ALPN_SESSION,
        &config,
    )
    .await
    .unwrap();
    let (mut send, mut recv) = gw_conn.open_bi().await.unwrap();
    ndp_signal::write_message(
        &mut send,
        &ClientHello {
            ticket: ticket["ticket"].as_str().unwrap().to_string(),
            noise_public_key: hex::encode(client_keys.public().as_bytes()),
            client_version: "0.1.0".into(),
        },
    )
    .await
    .unwrap();

    let ClientHelloAck::Accepted {
        session,
        relay_addr,
        relay_pin,
        pair_token,
        agent_key: named_key,
    } = ndp_signal::read_message::<ClientHelloAck>(&mut recv)
        .await
        .unwrap()
    else {
        panic!("the gateway refused the session");
    };
    assert_eq!(named_key, agent_key);

    // Meet the agent at the relay.
    let relay_endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let relay_conn = connect(
        &relay_endpoint,
        relay_addr.parse().unwrap(),
        "localhost",
        CertificateFingerprint::from_hex(&relay_pin).unwrap(),
        ALPN_RELAY,
        &config,
    )
    .await
    .unwrap();
    let (mut rsend, mut rrecv) = relay_conn.open_bi().await.unwrap();
    ndp_signal::write_message(&mut rsend, &RelayHello { pair_token })
        .await
        .unwrap();
    assert!(matches!(
        ndp_signal::read_message::<RelayHelloAck>(&mut rrecv)
            .await
            .unwrap(),
        RelayHelloAck::Spliced
    ));

    let initiator = Initiator::new(
        &client_keys,
        &PublicKey::from_slice(&hex::decode(&agent_key).unwrap()).unwrap(),
        &prologue(session),
    )
    .unwrap();
    let (client, mut incoming, greeting) = Session::initiate(
        relay_conn,
        initiator,
        ticket["ticket"].as_str().unwrap().as_bytes(),
        &config,
    )
    .await
    .expect("the end-to-end handshake should complete");
    assert_eq!(greeting, b"agent-ready");

    // The first *video* a joining client receives must be something it can
    // decode on its own. Audio shares the session and may well arrive first,
    // which is fine: the two channels are independent by design.
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
    assert!(
        first.header.flags.contains(MsgFlags::KEYFRAME),
        "a client that just joined has nothing to reference"
    );
    assert!(!first.payload.is_empty());

    // And frames should keep coming.
    let mut frames = 1;
    while frames < 5 {
        let next = tokio::time::timeout(Duration::from_secs(5), incoming.recv())
            .await
            .expect("video should keep flowing")
            .unwrap()
            .unwrap();
        if next.channel == Channel::Video {
            frames += 1;
        }
    }

    // Input in the other direction. There is nothing to inject into on a test
    // platform, so what is proven here is carriage and policy, not effect.
    let events = [InputEvent::mouse_move(0.5, 0.5, Modifiers::NONE)];
    client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputBatch, 1, 0),
            &InputEvent::encode_batch(&events),
        )
        .await
        .unwrap();

    // A ping should come back, which is the simplest proof the agent is
    // reading the control channel and not merely blasting video.
    client
        .send(
            Channel::Control,
            MsgHeader::new(MsgKind::Ping, 2, 1234),
            b"",
        )
        .await
        .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message = incoming.recv().await.unwrap().unwrap();
            if message.header.kind == MsgKind::Pong {
                return message;
            }
        }
    })
    .await
    .expect("the agent should answer a ping");
    assert_eq!(pong.header.timestamp_us, 1234);

    let sessions = deployment
        .get("/v1/sessions", &deployment.owner_token)
        .await;
    let recorded = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"].as_str() == Some(session.to_string().as_str()))
        .expect("the manager should have recorded the session");
    assert_eq!(recorded["state"], "ACTIVE");

    client.close(0, b"done");
    deployment.gateway.shutdown();
    deployment.relay.shutdown();
}
