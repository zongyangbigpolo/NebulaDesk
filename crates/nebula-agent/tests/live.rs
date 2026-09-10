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
        let ticket_issuer = "https://published-manager.example.test".to_string();
        let state = nebula_manager::AppState::bootstrap(nebula_manager::Config {
            public_url: ticket_issuer.clone(),
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
            ticket_issuer: Some(ticket_issuer),
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
    let agent = Agent::new(identity, Arc::new(TestPattern::default())).unwrap();
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
    real_client(None).await;
}

/// Build with `scripts/build-seamless-fixture.sh`; point NEBULA_APP_PROBE_PATH
/// at its bundle and NEBULA_APP_PROBE_STATUS_DIR at target/seamless-fixture/status.
/// Requires an explicitly selected isolated NEBULA_TEST_DATABASE_URL.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "requires an idle native desktop, existing permissions, and NEBULA_APP_PROBE_PATH"]
async fn native_app_two_documents_resize_and_close_normally() {
    let database = std::env::var("NEBULA_TEST_DATABASE_URL")
        .expect("native APP regression requires an explicit isolated NEBULA_TEST_DATABASE_URL");
    assert!(
        !database.trim().is_empty(),
        "native APP regression requires a nonempty isolated NEBULA_TEST_DATABASE_URL"
    );
    let path = std::env::var("NEBULA_APP_PROBE_PATH").expect(
        "explicitly set NEBULA_APP_PROBE_PATH to the controlled two-document fixture bundle",
    );
    assert!(path.ends_with(".app"), "publish a fixture app bundle");
    tokio::time::timeout(Duration::from_secs(150), real_client(Some(&path)))
        .await
        .expect("native APP regression exceeded its overall deadline");
}

async fn real_client(application: Option<&str>) {
    #[cfg(target_os = "macos")]
    let fixture_start = application.map(|_| native_app::FixtureDiscovery::snapshot());
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
    let dir = if application.is_some() {
        tempfile::tempdir_in(".").unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    let state = dir.path().join("agent.json");
    let identity = enroll(&deployment.manager_url, &token, &name, &state)
        .await
        .expect("the machine should enrol");
    assert!(
        Identity::load(&state).unwrap().is_some(),
        "the identity must be on disk before the agent claims to be enrolled"
    );

    let platform: Arc<dyn nebula_agent::Platform> = match application {
        #[cfg(target_os = "macos")]
        Some(_) => nebula_agent::platform::native(),
        #[cfg(not(target_os = "macos"))]
        Some(_) => panic!("native APP regression is macOS-only"),
        None => Arc::new(TestPattern::default()),
    };
    let agent = Agent::new(identity.clone(), platform).unwrap();
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

    // Publish and grant the selected resource through the real manager.
    let machine = identity.machine_id;
    let resource = deployment
        .post(
            &format!("/v1/machines/{machine}/resources"),
            Some(&deployment.owner_token),
            match application {
                Some(path) => json!({
                    "kind": "APP", "name": "Native two-document regression",
                    "launch_path": path, "launch_args": []
                }),
                None => json!({ "kind": "DESKTOP", "name": "Desktop" }),
            },
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

    // The client selects a resource; desktop details include its machine ID,
    // but connecting still uses the resource rather than a machine address.
    let available = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let available = deployment
                .get(
                    if application.is_some() {
                        "/v1/resources?application_windows=true"
                    } else {
                        "/v1/resources"
                    },
                    &deployment.owner_token,
                )
                .await;
            if application.is_none() || available[0]["launch_supported"] == true {
                break available;
            }
            // Tunnel attachment precedes the Agent's asynchronous capability heartbeat.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the native Agent should report its usable application capability");
    assert_eq!(available.as_array().unwrap().len(), 1);
    assert_eq!(available[0]["machine_status"], "ONLINE");
    if application.is_some() {
        assert!(available[0]["machine_id"].is_null());
        assert_eq!(available[0]["launch_supported"], true);
        for feature in ["audio", "clipboard", "file_transfer"] {
            assert_eq!(available[0]["policy"][feature], false);
        }
    } else {
        assert_eq!(available[0]["machine_id"], machine.to_string());
    }
    assert_eq!(available[0]["owned"], false);
    assert!(available[0].get("credential").is_none());
    assert!(available[0].get("launch_path").is_none());

    let ticket = deployment
        .post(
            "/v1/sessions",
            Some(&deployment.owner_token),
            if application.is_some() {
                json!({ "resource_id": resource, "client_os": "MACOS",
                    "application_windows": true })
            } else {
                json!({ "resource_id": resource, "client_os": "MACOS" })
            },
        )
        .await;
    assert_eq!(ticket["agent_key"].as_str().unwrap(), agent_key);
    if application.is_some() {
        assert_eq!(available[0]["kind"], "APP");
        assert_eq!(available[0]["id"], resource);
        assert_eq!(ticket["application_windows"], true);
    }

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
            application_windows: application.is_some(),
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

    #[cfg(target_os = "macos")]
    if application.is_some() {
        native_app::exercise(&client, &mut incoming, fixture_start.unwrap()).await;
        client.close(0, b"done");
        deployment.gateway.shutdown();
        deployment.relay.shutdown();
        return;
    }

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

    // The media handshake and the gateway's HTTP state report are independent.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let sessions = deployment
                .get("/v1/sessions", &deployment.owner_token)
                .await;
            let recorded = sessions
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"].as_str() == Some(session.to_string().as_str()))
                .expect("the manager should have recorded the session");
            if recorded["state"] != "PENDING" {
                assert_eq!(recorded["state"], "ACTIVE");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the gateway should report ACTIVE to the manager");

    client.close(0, b"done");
    deployment.gateway.shutdown();
    deployment.relay.shutdown();
}

#[cfg(target_os = "macos")]
mod native_app {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use ndp_proto::application::{
        ApplicationFailureReason, ApplicationHostOs, ApplicationMessage as App, SurfaceFrameInfo,
        SurfaceInfo, SurfaceRegistry, APPLICATION_PROTOCOL_VERSION, MAX_APPLICATION_SURFACES,
    };
    use ndp_proto::control::ByeReason;
    use ndp_proto::{
        Caps, ControlMessage, DisplayGeometry, FeatureFlags, InputKind, KeyCode, MouseButton,
        VideoCodec, VideoFrameInfo,
    };
    use ndp_transport::SessionReceiver;
    use shiguredo_video_toolbox::{
        DecodedFrame, Decoder, DecoderCodec, DecoderConfig, PixelFormat,
    };

    // Measured native starts cost up to 4.9s per window and run serially;
    // allow both starts plus fresh keyframe decoding, but never extend on retry.
    const CAPTURE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(20);

    #[derive(Clone, Copy)]
    struct TimingMark {
        instant: tokio::time::Instant,
        unix_ms: u128,
    }

    impl TimingMark {
        fn now() -> Self {
            Self {
                instant: tokio::time::Instant::now(),
                unix_ms: SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            }
        }
    }

    // Observation latency includes fixture reporting/polling, not just injection.
    // Declared delays are intentional sleeps, excluding scheduler overshoot.
    fn timing(event: &str, start: TimingMark, last_send: Option<TimingMark>, delays_ms: u64) {
        let observed = TimingMark::now();
        eprintln!(
            "native APP timing {event}: start_unix_ms={} observed_unix_ms={} elapsed_ms={:.3} intentional_delays_ms={} last_send_unix_ms={:?} last_send_to_observed_ms={:?}",
            start.unix_ms, observed.unix_ms,
            observed.instant.duration_since(start.instant).as_secs_f64() * 1000.0,
            delays_ms, last_send.map(|mark| mark.unix_ms),
            last_send.map(|mark| observed.instant.duration_since(mark.instant).as_secs_f64() * 1000.0)
        );
    }

    #[derive(serde::Deserialize)]
    struct FixtureStatus {
        pid: u32,
        key_down_codes: Vec<u16>,
        key_up_codes: Vec<u16>,
        mouse_events: Vec<FixtureMouseEvent>,
        windows: Vec<FixtureWindow>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureWindow {
        id: Value,
        title: String,
        window_number: i64,
        dirty: bool,
        text: String,
        reset_presses: u64,
        reset_button: FixturePoint,
        scroll_offset: f64,
        scroll_target: FixturePoint,
        slider_value: f64,
        slider_drag: FixtureDrag,
        miniaturized: bool,
    }

    #[derive(Clone, Copy, serde::Deserialize)]
    struct FixturePoint {
        x: f32,
        y: f32,
    }

    impl FixturePoint {
        fn pointer(self, kind: InputKind, button: MouseButton) -> InputEvent {
            assert!(
                self.x.is_finite()
                    && self.y.is_finite()
                    && (0.0..=1.0).contains(&self.x)
                    && (0.0..=1.0).contains(&self.y)
            );
            let mut event = InputEvent::mouse_move(self.x, self.y, Modifiers::NONE);
            event.kind = kind;
            event.button = button;
            event
        }
    }

    #[derive(serde::Deserialize)]
    struct FixtureDrag {
        start: FixturePoint,
        end: FixturePoint,
        end_in_window: FixturePoint,
        expected_value: f64,
        value_tolerance: f64,
        point_tolerance: f64,
    }

    #[derive(serde::Deserialize)]
    struct FixtureMouseEvent {
        #[serde(rename = "type")]
        kind: u32,
        document_id: Option<Value>,
        window_number: i64,
        local_x: f64,
        local_y: f64,
    }

    impl FixtureStatus {
        fn window(&self, id: &Value) -> &FixtureWindow {
            let matches: Vec<_> = self
                .windows
                .iter()
                .filter(|window| window.id == *id)
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "fixture document ID must remain unambiguous"
            );
            matches[0]
        }
    }

    pub(super) struct FixtureDiscovery {
        directory: PathBuf,
        existing: BTreeSet<PathBuf>,
        started: SystemTime,
    }

    impl FixtureDiscovery {
        pub(super) fn snapshot() -> Self {
            // The fixture writer must use this same directory; LaunchServices
            // does not receive an environment override from this harness.
            let directory = std::env::var_os("NEBULA_APP_PROBE_STATUS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            let existing = Self::paths(&directory).into_iter().collect();
            eprintln!(
                "native APP fixture status directory: {}",
                directory.display()
            );
            Self {
                directory,
                existing,
                started: SystemTime::now(),
            }
        }

        fn paths(directory: &Path) -> Vec<PathBuf> {
            std::fs::read_dir(directory)
                .unwrap_or_else(|error| {
                    panic!(
                        "read fixture status directory {} (NEBULA_APP_PROBE_STATUS_DIR): {error}",
                        directory.display()
                    )
                })
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| fixture_pid(path).is_some())
                .collect()
        }

        fn discover(&self) -> Option<Fixture> {
            let candidates: Vec<_> = Self::paths(&self.directory)
                .into_iter()
                .filter_map(|path| {
                    if self.existing.contains(&path) {
                        return None;
                    }
                    let metadata = std::fs::symlink_metadata(&path).ok()?;
                    if !metadata.is_file() || metadata.modified().ok()? < self.started {
                        return None;
                    }
                    let fixture = Fixture {
                        pid: fixture_pid(&path)?,
                        path,
                    };
                    fixture.read()?;
                    Some(fixture)
                })
                .collect();
            assert!(
                candidates.len() <= 1,
                "ambiguous newly launched fixtures; refusing native input"
            );
            candidates.into_iter().next()
        }
    }

    fn fixture_pid(path: &Path) -> Option<u32> {
        let pid = path
            .file_name()?
            .to_str()?
            .strip_prefix("nebula-seamless-fixture-status-")?
            .strip_suffix(".json")?
            .parse::<u32>()
            .ok()?;
        (pid > 0).then_some(pid)
    }

    struct Fixture {
        pid: u32,
        path: PathBuf,
    }

    impl Fixture {
        fn read(&self) -> Option<FixtureStatus> {
            let metadata = std::fs::symlink_metadata(&self.path).ok()?;
            assert!(metadata.is_file() && metadata.len() <= 128 * 1024);
            let status: FixtureStatus =
                serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
            assert_eq!(status.pid, self.pid, "filename PID must match fixture JSON");
            assert!(status.key_down_codes.len() <= 128 && status.key_up_codes.len() <= 128);
            assert!(status.mouse_events.len() <= 32);
            Some(status)
        }

        async fn wait(
            &self,
            probe: &mut Probe,
            client: &Session,
            incoming: &mut SessionReceiver,
            description: &str,
            satisfied: impl Fn(&FixtureStatus) -> bool,
        ) -> FixtureStatus {
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    if let Some(status) = self.read() {
                        if satisfied(&status) {
                            return status;
                        }
                    }
                    probe.receive(client, incoming).await;
                    assert_eq!(
                        probe.live.len(),
                        2,
                        "input must not create or remove surfaces"
                    );
                }
            })
            .await
            .unwrap_or_else(|_| panic!("fixture did not confirm {description}"))
        }
    }

    #[derive(Default)]
    struct Media {
        sequence: Option<u32>,
        received_sequences: BTreeSet<u32>,
        generation: u32,
        decoder: Option<Decoder>,
        pictures: usize,
        total_pictures: usize,
        picture_hash: Option<blake3::Hash>,
    }

    impl Media {
        fn invalidate(&mut self) {
            self.decoder = None;
            self.pictures = 0;
            self.picture_hash = None;
        }
    }

    struct Probe {
        started: TimingMark,
        registry: SurfaceRegistry,
        live: BTreeSet<u8>,
        removed: BTreeSet<u8>,
        media: BTreeMap<u8, Media>,
        unavailable: BTreeSet<u8>,
        minimize_target: Option<u8>,
        recovering: BTreeMap<u8, tokio::time::Instant>,
        stage: &'static str,
        fixture_pid: Option<u32>,
        last_event: String,
        hello: bool,
        ready: bool,
        bye: bool,
        sequence: u32,
    }

    impl Probe {
        fn new() -> Self {
            Self {
                started: TimingMark::now(),
                registry: SurfaceRegistry::new(MAX_APPLICATION_SURFACES).unwrap(),
                live: BTreeSet::new(),
                removed: BTreeSet::new(),
                media: BTreeMap::new(),
                unavailable: BTreeSet::new(),
                minimize_target: None,
                recovering: BTreeMap::new(),
                stage: "initial capture / APP negotiation",
                fixture_pid: None,
                last_event: String::new(),
                hello: false,
                ready: false,
                bye: false,
                sequence: 0,
            }
        }

        fn stage(&mut self, stage: &'static str) {
            self.stage = stage;
            eprintln!("native APP stage: {stage}");
        }

        fn recovery_deadline(&self) -> tokio::time::Instant {
            self.recovering
                .values()
                .copied()
                .min()
                .map(|started| started + CAPTURE_RECOVERY_TIMEOUT)
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(15))
        }

        async fn send(&mut self, client: &Session, message: ControlMessage) {
            let kind = match &message {
                ControlMessage::Hello { .. } => MsgKind::Hello,
                ControlMessage::Application(command) => {
                    self.registry.validate_command(command).unwrap();
                    MsgKind::Application
                }
                ControlMessage::Bye { .. } => MsgKind::Bye,
                _ => panic!("unexpected test command"),
            };
            client
                .send(
                    Channel::Control,
                    MsgHeader::new(kind, self.sequence, 0),
                    &message.encode().unwrap(),
                )
                .await
                .expect("encrypted APP control send");
            self.sequence += 1;
        }

        async fn input(&mut self, client: &Session, id: u8, mut event: InputEvent) {
            event.display = id;
            self.send(
                client,
                ControlMessage::Application(App::Input {
                    surface_id: id,
                    geometry_generation: self.surface(id).geometry_generation,
                    event: event.to_bytes(),
                }),
            )
            .await;
        }

        async fn receive(&mut self, client: &Session, incoming: &mut SessionReceiver) {
            let deadline = self.recovery_deadline();
            assert!(
                tokio::time::Instant::now() < deadline,
                "native APP stage {}: surface recovery exceeded {:?} despite ongoing traffic",
                self.stage,
                CAPTURE_RECOVERY_TIMEOUT
            );
            let message = tokio::time::timeout_at(deadline, incoming.recv())
                .await
                .unwrap_or_else(|_| panic!(
                    "native APP stage {}: traffic stalled or surface failed to resume \
                     with five current decoded pictures within {:?}; unavailable={:?}, recovering={:?}",
                    self.stage, CAPTURE_RECOVERY_TIMEOUT, self.unavailable, self.recovering.keys().collect::<Vec<_>>()
                ))
                .expect("transport ended before the final SurfaceRemove and normal Bye")
                .expect("native APP transport failed");
            match message.channel {
                Channel::Control => {
                    let control = ControlMessage::decode(&message.payload).unwrap();
                    self.last_event = format!("{control:?}");
                    match control {
                        ControlMessage::Application(App::Hello {
                            protocol_version,
                            max_surfaces,
                            host_os,
                            ..
                        }) => {
                            assert_eq!(message.header.kind, MsgKind::Application);
                            assert!(!self.hello && !self.ready && self.live.is_empty());
                            assert_eq!(protocol_version, APPLICATION_PROTOCOL_VERSION);
                            assert_eq!(host_os, ApplicationHostOs::Macos);
                            self.registry = SurfaceRegistry::new(max_surfaces).unwrap();
                            self.hello = true;
                        }
                        ControlMessage::HelloAck {
                            caps, active_codec, ..
                        } => {
                            assert_eq!(message.header.kind, MsgKind::HelloAck);
                            assert!(self.hello && !self.ready && self.live.is_empty());
                            assert_eq!(active_codec, VideoCodec::H264);
                            assert_eq!(caps.video_codecs, [VideoCodec::H264]);
                            assert_eq!(caps.features, FeatureFlags::APPLICATION_WINDOWS);
                            assert!(caps.audio_codecs.is_empty());
                            self.ready = true;
                        }
                        ControlMessage::Application(App::SurfaceUpsert { surface }) => {
                            assert_eq!(message.header.kind, MsgKind::Application);
                            assert!(self.ready, "surface appeared before APP Hello/HelloAck");
                            assert!(!surface.modal && surface.parent_surface_id.is_none());
                            if surface.minimized {
                                assert_eq!(
                                    self.minimize_target,
                                    Some(surface.surface_id),
                                    "only the explicitly minimized document may be minimized"
                                );
                            }
                            let id = surface.surface_id;
                            let generation = surface.geometry_generation;
                            let minimized = surface.minimized;
                            let was_minimized =
                                self.registry.get(id).is_some_and(|old| old.minimized);
                            self.registry.upsert(surface).unwrap();
                            self.live.insert(id);
                            if self.unavailable.remove(&id) {
                                eprintln!("native APP stage {}: surface {id} resumed metadata, generation {generation}; awaiting fresh keyframe", self.stage);
                            }
                            if let Some(media) = self.media.get_mut(&id) {
                                if minimized || was_minimized || media.generation != generation {
                                    media.invalidate();
                                }
                            }
                            if minimized {
                                // An intentional pause needs no capture recovery
                                // until Focus restores the same authorized surface.
                                self.recovering.remove(&id);
                            } else if was_minimized {
                                self.recovering.insert(id, tokio::time::Instant::now());
                            }
                            assert!(
                                self.live.len() + self.removed.len() <= 2,
                                "the two-document fixture exposed an extra native surface; \
                                 do not hide capture-status/AX widgets with title filtering"
                            );
                            if !minimized {
                                self.send(
                                    client,
                                    ControlMessage::Application(App::RequestKeyframe {
                                        surface_id: id,
                                        geometry_generation: generation,
                                    }),
                                )
                                .await;
                            }
                        }
                        ControlMessage::Application(App::SurfaceUnavailable {
                            surface_id,
                            reason,
                        }) => {
                            assert_eq!(message.header.kind, MsgKind::Application);
                            assert!(self.ready && self.live.contains(&surface_id));
                            assert!(self.registry.get(surface_id).is_some());
                            assert_eq!(reason, ApplicationFailureReason::SurfaceUnavailable);
                            // Capture loss is not window destruction. Repeated
                            // notices must not extend the recovery deadline.
                            self.unavailable.insert(surface_id);
                            if !self.surface(surface_id).minimized {
                                self.recovering
                                    .entry(surface_id)
                                    .or_insert_with(tokio::time::Instant::now);
                            }
                            self.media.entry(surface_id).or_default().invalidate();
                            eprintln!("native APP stage {}: surface {surface_id} temporarily unavailable; retaining its ID, allowing {:?} to resume", self.stage, CAPTURE_RECOVERY_TIMEOUT);
                        }
                        ControlMessage::Application(App::SurfaceRemove { surface_id }) => {
                            assert_eq!(message.header.kind, MsgKind::Application);
                            assert!(self.ready);
                            self.registry.remove(surface_id).unwrap();
                            assert!(self.live.remove(&surface_id));
                            assert!(self.removed.insert(surface_id));
                            self.unavailable.remove(&surface_id);
                            self.recovering.remove(&surface_id);
                        }
                        ControlMessage::Bye { reason, .. } => {
                            assert_eq!(message.header.kind, MsgKind::Bye);
                            assert_eq!(reason, ByeReason::UserClosed);
                            assert!(self.ready && self.live.is_empty());
                            assert_eq!(self.removed.len(), 2, "Bye must follow both removes");
                            self.bye = true;
                        }
                        other => panic!("unexpected native APP control: {other:?}"),
                    }
                }
                Channel::Video => {
                    assert_eq!(message.header.kind, MsgKind::VideoFrame);
                    let (video, scoped) = VideoFrameInfo::split(&message.payload).unwrap();
                    let (frame, avcc) = SurfaceFrameInfo::split(scoped).unwrap();
                    self.last_event = format!(
                        "video surface={} generation={} surface_sequence={}",
                        video.display, frame.geometry_generation, frame.surface_sequence
                    );
                    assert_eq!(video.codec, VideoCodec::H264);
                    assert!(video.display < MAX_APPLICATION_SURFACES);
                    assert!(u32::from(video.width) <= 16_384);
                    assert!(u32::from(video.height) <= 16_384);
                    let media = self.media.entry(video.display).or_default();
                    assert!(
                        media.received_sequences.insert(frame.surface_sequence),
                        "surface {} reused a sequence, including across resize",
                        video.display
                    );
                    let units = avcc_units(avcc);
                    // Video frames use independent QUIC streams: arrival order
                    // need not be sequence order even on the loopback relay.
                    if media
                        .sequence
                        .is_some_and(|last| frame.surface_sequence < last)
                    {
                        return;
                    }
                    let gap = media
                        .sequence
                        .is_some_and(|last| frame.surface_sequence != last + 1);
                    media.sequence = Some(frame.surface_sequence);
                    if self.unavailable.contains(&video.display) {
                        return;
                    }

                    // Control and video are separate ordered streams. Discard
                    // in-flight old/retired or not-yet-announced geometry just
                    // as a native client does; only current media counts.
                    let Ok(surface) = self.registry.validate_frame(video.display, &frame) else {
                        return;
                    };
                    if surface.minimized {
                        return;
                    }
                    assert!(self.ready);
                    assert_eq!(
                        (u32::from(video.width), u32::from(video.height)),
                        (surface.width, surface.height)
                    );
                    if media.generation != frame.geometry_generation {
                        media.generation = frame.geometry_generation;
                        media.invalidate();
                    }
                    if gap && !message.header.flags.contains(MsgFlags::KEYFRAME) {
                        media.decoder = None;
                        self.send(
                            client,
                            ControlMessage::Application(App::RequestKeyframe {
                                surface_id: video.display,
                                geometry_generation: frame.geometry_generation,
                            }),
                        )
                        .await;
                        return;
                    }
                    if message.header.flags.contains(MsgFlags::KEYFRAME) {
                        let sps = units.iter().find(|nal| nal[0] & 31 == 7).expect("H264 SPS");
                        let pps = units.iter().find(|nal| nal[0] & 31 == 8).expect("H264 PPS");
                        assert!(units.iter().any(|nal| nal[0] & 31 == 5), "H264 IDR");
                        if media.decoder.is_none() {
                            media.decoder = Some(
                                Decoder::new(DecoderConfig {
                                    codec: DecoderCodec::H264 {
                                        sps,
                                        pps,
                                        nalu_len_bytes: 4,
                                    },
                                    pixel_format: PixelFormat::Nv12,
                                })
                                .expect("independent surface VideoToolbox decoder"),
                            );
                        }
                    }
                    let Some(decoder) = media.decoder.as_mut() else {
                        return;
                    };
                    if let Some(picture) = decoder.decode(avcc).expect("valid H264 picture") {
                        let DecodedFrame::Nv12(picture) = picture else {
                            panic!("expected requested NV12 decoder output");
                        };
                        assert_eq!(
                            (picture.width() as u32, picture.height() as u32),
                            (surface.width, surface.height),
                            "decoded picture must match current geometry, not its sibling"
                        );
                        assert!(!picture.y_plane().is_empty() && !picture.uv_plane().is_empty());
                        media.picture_hash = Some(blake3::hash(picture.y_plane()));
                        media.pictures += 1;
                        media.total_pictures += 1;
                        if media.total_pictures == 1 {
                            timing(
                                &format!("first_decoded_surface_{}", video.display),
                                self.started,
                                None,
                                0,
                            );
                        }
                        if media.pictures >= 5 && self.recovering.remove(&video.display).is_some() {
                            eprintln!("native APP stage {}: surface {} recovered with five current decoded pictures", self.stage, video.display);
                        }
                    }
                }
                other => panic!("APP leaked a global channel: {other:?}"),
            }
        }

        fn surface(&self, id: u8) -> SurfaceInfo {
            self.registry
                .get(id)
                .expect("authorized live surface")
                .clone()
        }

        fn pictures(&self, id: u8) -> usize {
            let Some(surface) = self.registry.get(id) else {
                return 0;
            };
            self.media
                .get(&id)
                .filter(|media| {
                    !surface.minimized
                        && !self.unavailable.contains(&id)
                        && media.generation == surface.geometry_generation
                })
                .map_or(0, |media| media.pictures)
        }

        fn total_pictures(&self, id: u8) -> usize {
            self.media.get(&id).map_or(0, |media| media.total_pictures)
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            if !self.bye || std::thread::panicking() {
                eprintln!("native APP failed/cancelled: stage={}, fixture_pid={:?}, hello={}, ready={}, live={:?}, removed={:?}, unavailable={:?}, last_event={}",
                    self.stage, self.fixture_pid, self.hello, self.ready, self.live, self.removed, self.unavailable, self.last_event);
                for (id, media) in &self.media {
                    eprintln!("surface {id}: metadata={:?}, media_generation={}, sequence={:?}, current_pictures={}, total_pictures={}, recovery_elapsed={:?}",
                        self.registry.get(*id), media.generation, media.sequence, media.pictures, media.total_pictures,
                        self.recovering.get(id).map(tokio::time::Instant::elapsed));
                }
            }
        }
    }

    fn avcc_units(mut bytes: &[u8]) -> Vec<&[u8]> {
        let mut units = Vec::new();
        while !bytes.is_empty() {
            assert!(bytes.len() >= 4, "truncated AVCC length");
            let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
            bytes = &bytes[4..];
            assert!(
                length > 0 && length <= bytes.len(),
                "invalid AVCC NAL length"
            );
            let (nal, rest) = bytes.split_at(length);
            assert_eq!(nal[0] & 0x80, 0, "H264 forbidden bit");
            units.push(nal);
            bytes = rest;
        }
        assert!(!units.is_empty(), "empty H264 access unit");
        units
    }

    async fn scoped_input(
        probe: &mut Probe,
        client: &Session,
        incoming: &mut SessionReceiver,
        discovery: FixtureDiscovery,
        first: u8,
        second: u8,
    ) -> (Fixture, Value, Value) {
        probe.stage("fixture discovery");
        let fixture = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(fixture) = discovery.discover() {
                    return fixture;
                }
                probe.receive(client, incoming).await;
            }
        })
        .await
        .expect("a new PID-specific fixture status file must appear after test start");
        probe.fixture_pid = Some(fixture.pid);
        let before = fixture
            .wait(probe, client, incoming, "two clean documents", |status| {
                status.windows.len() == 2 && status.windows.iter().all(|window| !window.dirty)
            })
            .await;
        let document_id = |surface: SurfaceInfo| {
            let matches: Vec<_> = before
                .windows
                .iter()
                .filter(|window| window.title == surface.title)
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "only correlate exact titles supplied by this controlled fixture"
            );
            assert!(!matches[0].id.is_null());
            matches[0].id.clone()
        };
        let selected = document_id(probe.surface(first));
        let sibling = document_id(probe.surface(second));
        assert_ne!(selected, sibling);
        assert!(
            before.key_down_codes.is_empty() && before.key_up_codes.is_empty(),
            "new fixture must have no prior keyboard input"
        );
        probe.stage("focus first document");
        let focus_started = TimingMark::now();
        let mut last_key_send = focus_started;
        probe
            .send(
                client,
                ControlMessage::Application(App::Focus {
                    surface_id: first,
                    geometry_generation: probe.surface(first).geometry_generation,
                }),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        probe.stage("scoped A/B/C key input and native key-code evidence");
        for key in [KeyCode::A, KeyCode(0x05), KeyCode(0x06)] {
            for kind in [InputKind::KeyDown, InputKind::KeyUp] {
                probe
                    .input(client, first, InputEvent::key(kind, key, Modifiers::NONE))
                    .await;
                last_key_send = TimingMark::now();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
        let typed = fixture
            .wait(
                probe,
                client,
                incoming,
                "native A/B/C down and up codes",
                |status| status.key_down_codes == [0, 11, 8] && status.key_up_codes == [0, 11, 8],
            )
            .await;
        timing(
            "focus_to_abc_evidence",
            focus_started,
            Some(last_key_send),
            100 + 6 * 30,
        );
        assert_eq!(typed.window(&sibling).text, before.window(&sibling).text);
        assert_eq!(typed.window(&sibling).dirty, before.window(&sibling).dirty);
        probe.stage("dismiss completion popup with scoped Escape");
        for kind in [InputKind::KeyDown, InputKind::KeyUp] {
            probe
                .input(
                    client,
                    first,
                    InputEvent::key(kind, KeyCode::ESCAPE, Modifiers::NONE),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let settled = tokio::time::Instant::now() + Duration::from_millis(350);
        while tokio::time::Instant::now() < settled {
            probe.receive(client, incoming).await;
            assert_eq!(probe.live, BTreeSet::from([first, second]));
        }
        let dismissed = fixture
            .wait(
                probe,
                client,
                incoming,
                "native Escape down/up after A/B/C",
                |status| {
                    status.key_down_codes == [0, 11, 8, 53] && status.key_up_codes == [0, 11, 8, 53]
                },
            )
            .await;
        let window = dismissed.window(&selected);
        let presses = window.reset_presses;
        let (x, y) = (window.reset_button.x, window.reset_button.y);
        probe.stage("scoped Reset mouse input and NSButton evidence");
        let reset_started = TimingMark::now();
        let mut last_mouse_send = reset_started;
        assert!(
            x.is_finite() && y.is_finite() && (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)
        );
        for kind in [
            InputKind::MouseMove,
            InputKind::MouseDown,
            InputKind::MouseUp,
        ] {
            let mut event = InputEvent::mouse_move(x, y, Modifiers::NONE);
            event.kind = kind;
            if kind != InputKind::MouseMove {
                event.button = MouseButton::Left;
            }
            probe.input(client, first, event).await;
            last_mouse_send = TimingMark::now();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let reset = fixture
            .wait(
                probe,
                client,
                incoming,
                "the actual Reset NSButton action",
                |status| {
                    let window = status.window(&selected);
                    window.reset_presses > presses && !window.dirty && window.text.is_empty()
                },
            )
            .await;
        timing(
            "reset_click_evidence",
            reset_started,
            Some(last_mouse_send),
            3 * 50,
        );
        assert_eq!(reset.window(&selected).reset_presses, presses + 1);
        assert_eq!(reset.window(&sibling).text, before.window(&sibling).text);
        assert_eq!(reset.window(&sibling).dirty, before.window(&sibling).dirty);
        assert_eq!(
            reset.window(&sibling).reset_presses,
            before.window(&sibling).reset_presses
        );

        probe.stage("scoped wheel changes actual scroll-view content offset");
        let wheel_started = TimingMark::now();
        let mut last_wheel_send = wheel_started;
        let scroll_before = reset.window(&selected).scroll_offset;
        assert!(scroll_before.is_finite());
        let target = reset.window(&selected).scroll_target;
        for kind in [InputKind::MouseMove, InputKind::Wheel] {
            let mut event = target.pointer(kind, MouseButton::None);
            if kind == InputKind::Wheel {
                event.scroll_y = -6.0;
            }
            probe.input(client, first, event).await;
            last_wheel_send = TimingMark::now();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let scrolled = fixture
            .wait(
                probe,
                client,
                incoming,
                "actual scroll-view content offset change",
                |status| {
                    let offset = status.window(&selected).scroll_offset;
                    offset.is_finite() && offset >= 0.0 && offset != scroll_before
                },
            )
            .await;
        timing(
            "wheel_scroll_evidence",
            wheel_started,
            Some(last_wheel_send),
            2 * 50,
        );
        assert_eq!(
            scrolled.window(&sibling).scroll_offset,
            reset.window(&sibling).scroll_offset
        );
        assert_eq!(
            scrolled.window(&sibling).slider_value,
            reset.window(&sibling).slider_value
        );
        assert_eq!(
            scrolled.window(&selected).slider_value,
            reset.window(&selected).slider_value
        );
        eprintln!(
            "fixture PID {}: scroll offset {} -> {}",
            fixture.pid,
            scroll_before,
            scrolled.window(&selected).scroll_offset
        );

        probe.stage("scoped held-left-button drag changes actual slider value");
        let drag_started = TimingMark::now();
        let slider_before = scrolled.window(&selected).slider_value;
        assert!(slider_before.is_finite() && (0.0..=1.0).contains(&slider_before));
        let points = &scrolled.window(&selected).slider_drag;
        assert!(points.expected_value.is_finite() && (0.0..=1.0).contains(&points.expected_value));
        assert!(
            points.value_tolerance.is_finite()
                && points.value_tolerance > 0.0
                && points.value_tolerance < (points.expected_value - slider_before).abs()
        );
        assert!(points.point_tolerance.is_finite() && points.point_tolerance > 0.0);
        assert_ne!(
            (points.start.x, points.start.y),
            (points.end.x, points.end.y)
        );
        for (kind, button) in [
            (InputKind::MouseMove, MouseButton::None),
            (InputKind::MouseDown, MouseButton::Left),
        ] {
            probe
                .input(client, first, points.start.pointer(kind, button))
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for step in 1..=4 {
            let fraction = step as f32 / 4.0;
            let position = FixturePoint {
                x: points.start.x + (points.end.x - points.start.x) * fraction,
                y: points.start.y + (points.end.y - points.start.y) * fraction,
            };
            probe
                .input(
                    client,
                    first,
                    position.pointer(InputKind::MouseDrag, MouseButton::Left),
                )
                .await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        probe
            .input(
                client,
                first,
                points.end.pointer(InputKind::MouseUp, MouseButton::Left),
            )
            .await;
        let last_drag_send = TimingMark::now();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let dragged = fixture
            .wait(
                probe,
                client,
                incoming,
                "final slider endpoint value and selected-document MouseDrag/MouseUp",
                |status| {
                    let value = status.window(&selected).slider_value;
                    let at_end = |event: &FixtureMouseEvent| {
                        event.document_id.as_ref() == Some(&selected)
                            && event.window_number == scrolled.window(&selected).window_number
                            && (event.local_x - f64::from(points.end_in_window.x)).abs()
                                <= points.point_tolerance
                            && (event.local_y - f64::from(points.end_in_window.y)).abs()
                                <= points.point_tolerance
                    };
                    let Some(up_index) = status
                        .mouse_events
                        .iter()
                        .rposition(|event| event.document_id.as_ref() == Some(&selected))
                    else {
                        return false;
                    };
                    // AppKit NSEventType: leftMouseUp=2, leftMouseDragged=6.
                    status.mouse_events[up_index].kind == 2
                        && at_end(&status.mouse_events[up_index])
                        && status.mouse_events[..up_index]
                            .iter()
                            .any(|event| event.kind == 6 && at_end(event))
                        && value.is_finite()
                        && (0.0..=1.0).contains(&value)
                        && value != slider_before
                        && (value - points.expected_value).abs() <= points.value_tolerance
                },
            )
            .await;
        timing(
            "slider_drag_evidence",
            drag_started,
            Some(last_drag_send),
            7 * 50,
        );
        assert_eq!(
            dragged.window(&sibling).slider_value,
            reset.window(&sibling).slider_value
        );
        assert_eq!(
            dragged.window(&sibling).scroll_offset,
            reset.window(&sibling).scroll_offset
        );
        for id in [&selected, &sibling] {
            assert_eq!(dragged.window(id).text, reset.window(id).text);
            assert_eq!(dragged.window(id).dirty, reset.window(id).dirty);
            assert_eq!(
                dragged.window(id).reset_presses,
                reset.window(id).reset_presses
            );
        }
        eprintln!(
            "fixture PID {}: completed slider drag {} -> {} (expected {}, tolerance {}), final MouseDrag/MouseUp at window-local ({}, {})",
            fixture.pid,
            slider_before,
            dragged.window(&selected).slider_value,
            points.expected_value,
            points.value_tolerance,
            points.end_in_window.x,
            points.end_in_window.y
        );
        eprintln!(
            "fixture PID {} confirmed scoped native keys, Reset click, wheel scrolling and slider drag",
            fixture.pid
        );
        (fixture, selected, sibling)
    }

    // Raw protocol/media assertions complement, not replace, native Client
    // screenshot acceptance. Reset through actual scoped input leaves both
    // documents clean so normal Close requires no save-sheet interaction.
    pub(super) async fn exercise(
        client: &Session,
        incoming: &mut SessionReceiver,
        fixture_start: FixtureDiscovery,
    ) {
        let mut probe = Probe::new();
        timing("control_hello_start", probe.started, None, 0);
        probe
            .send(
                client,
                ControlMessage::Hello {
                    caps: Caps {
                        video_codecs: vec![VideoCodec::H264],
                        audio_codecs: vec![],
                        displays: vec![DisplayGeometry {
                            width: 600,
                            height: 412,
                            scale: 1.0,
                            refresh_hz: 30,
                        }],
                        audio: Default::default(),
                        color: Default::default(),
                        max_bitrate_bps: 8_000_000,
                        features: FeatureFlags::APPLICATION_WINDOWS,
                    },
                    client_version: "native-app-live-regression".into(),
                    client_os: "macos".into(),
                },
            )
            .await;

        tokio::time::timeout(Duration::from_secs(40), async {
            while probe.live.len() != 2 || !probe.live.iter().all(|id| probe.pictures(*id) >= 5) {
                probe.receive(client, incoming).await;
            }
        })
        .await
        .expect("exactly two documents must independently decode");
        timing(
            "two_windows_ready_five_pictures_each",
            probe.started,
            None,
            0,
        );
        let ids: Vec<_> = probe.live.iter().copied().collect();
        let (first, second) = (ids[0], ids[1]);
        probe.stage("initial steady two-document media");
        let counts = [probe.total_pictures(first), probe.total_pictures(second)];
        let steady = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < steady || !probe.recovering.is_empty() {
            probe.receive(client, incoming).await;
            assert_eq!(
                probe.live.len(),
                2,
                "both documents stay live during steady traffic"
            );
        }
        for (index, id) in ids.iter().enumerate() {
            assert!(probe.total_pictures(*id) >= counts[index] + 5);
            assert!(
                probe.pictures(*id) >= 5,
                "steady traffic requires current decoded pictures"
            );
            assert!(
                probe.media[id].received_sequences.contains(&0),
                "each surface must start its own counter, not use the global header sequence"
            );
        }
        assert_ne!(
            probe.media[&first].picture_hash,
            probe.media[&second].picture_hash
        );

        let (fixture, selected_document, sibling_document) =
            scoped_input(&mut probe, client, incoming, fixture_start, first, second).await;

        probe.stage("post-input recovery before resize");
        tokio::time::timeout(Duration::from_secs(25), async {
            while !probe.recovering.is_empty()
                || probe.pictures(first) < 5
                || probe.pictures(second) < 5
            {
                probe.receive(client, incoming).await;
                assert_eq!(probe.live, BTreeSet::from([first, second]));
            }
        })
        .await
        .expect("both current document generations must decode before resize");
        let before_resize = [probe.surface(first), probe.surface(second)];
        probe.stage("resize first document and decode new geometry");
        let resized = (before_resize[0].width + 160, before_resize[0].height + 100);
        probe
            .send(
                client,
                ControlMessage::Application(App::Resize {
                    surface_id: first,
                    geometry_generation: before_resize[0].geometry_generation,
                    width: resized.0,
                    height: resized.1,
                }),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                probe.receive(client, incoming).await;
                assert_eq!(probe.live.len(), 2);
                let mut sibling = probe.surface(second);
                assert!(sibling.geometry_generation >= before_resize[1].geometry_generation);
                // A capture restart may advance only the sibling's generation;
                // its identity, title and all actual geometry must stay unchanged.
                sibling.geometry_generation = before_resize[1].geometry_generation;
                assert_eq!(
                    sibling, before_resize[1],
                    "resize must not change its sibling"
                );
                let surface = probe.surface(first);
                if surface.geometry_generation > before_resize[0].geometry_generation
                    && (surface.width, surface.height) == resized
                    && probe.pictures(first) >= 5
                    && probe.recovering.is_empty()
                {
                    break;
                }
            }
        })
        .await
        .expect("resized document must publish a new generation and valid new-sized pictures");

        // APP v1 exposes Minimize and Focus (which restores on macOS), without
        // a separate Restore command or per-command capability bit.
        probe.stage("Minimize first document / real NSWindow.isMiniaturized");
        let before_minimize = [probe.surface(first), probe.surface(second)];
        let minimize_started = TimingMark::now();
        probe.minimize_target = Some(first);
        probe
            .send(
                client,
                ControlMessage::Application(App::Minimize {
                    surface_id: first,
                    geometry_generation: before_minimize[0].geometry_generation,
                }),
            )
            .await;
        let minimize_sent = TimingMark::now();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                probe.receive(client, incoming).await;
                assert_eq!(probe.live, BTreeSet::from([first, second]));
                assert!(
                    probe.removed.is_empty(),
                    "Minimize must not retire either surface ID"
                );
                if let Some(status) = fixture.read() {
                    assert!(!status.window(&sibling_document).miniaturized);
                    if status.window(&selected_document).miniaturized
                        && probe.surface(first).minimized
                    {
                        break;
                    }
                }
            }
        })
        .await
        .expect("Minimize must update both real NSWindow state and retained surface metadata");
        timing(
            "minimize_native_evidence",
            minimize_started,
            Some(minimize_sent),
            0,
        );
        let paused_count = probe.total_pictures(first);
        let sibling_count = probe.total_pictures(second);
        probe.stage("sibling streams while first document remains minimized");
        let steady = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < steady {
            probe.receive(client, incoming).await;
            assert_eq!(probe.live, BTreeSet::from([first, second]));
            assert!(probe.removed.is_empty());
            assert!(probe.surface(first).minimized && !probe.surface(second).minimized);
        }
        assert_eq!(
            probe.total_pictures(first),
            paused_count,
            "minimized/in-flight media must not count as ready"
        );
        assert_eq!(probe.pictures(first), 0);
        assert!(
            probe.total_pictures(second) >= sibling_count + 5 && probe.pictures(second) >= 5,
            "sibling must keep decoding while the first window is minimized"
        );
        let minimized_status = fixture.read().expect("fixture status while minimized");
        assert!(minimized_status.window(&selected_document).miniaturized);
        assert!(!minimized_status.window(&sibling_document).miniaturized);

        probe.stage("Focus restores minimized document / fresh current decoded media");
        let minimized = probe.surface(first);
        assert!(
            minimized.geometry_generation > before_minimize[0].geometry_generation,
            "minimization must invalidate the previous capture generation"
        );
        let restore_started = TimingMark::now();
        probe
            .send(
                client,
                ControlMessage::Application(App::Focus {
                    surface_id: first,
                    geometry_generation: minimized.geometry_generation,
                }),
            )
            .await;
        let restore_sent = TimingMark::now();
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                probe.receive(client, incoming).await;
                assert_eq!(probe.live, BTreeSet::from([first, second]));
                assert!(
                    probe.removed.is_empty(),
                    "Focus must restore, not replace, the minimized surface"
                );
                if let Some(status) = fixture.read() {
                    assert!(!status.window(&sibling_document).miniaturized);
                    if !status.window(&selected_document).miniaturized
                        && !probe.surface(first).minimized
                        && probe.surface(first).geometry_generation > minimized.geometry_generation
                        && probe.pictures(first) >= 5
                        && probe.total_pictures(first) >= paused_count + 5
                        && probe.recovering.is_empty()
                    {
                        break;
                    }
                }
            }
        })
        .await
        .expect("Focus must restore real NSWindow and fresh decoded media on the same surface ID");
        timing(
            "focus_restore_native_and_media_evidence",
            restore_started,
            Some(restore_sent),
            0,
        );
        for (index, id) in [first, second].into_iter().enumerate() {
            let mut restored = probe.surface(id);
            assert!(restored.geometry_generation >= before_minimize[index].geometry_generation);
            restored.geometry_generation = before_minimize[index].geometry_generation;
            assert_eq!(
                restored, before_minimize[index],
                "minimize/restore must preserve document identity and geometry"
            );
        }
        probe.minimize_target = None;
        eprintln!("fixture PID {} confirmed Minimize and Focus restore for surface {first}, with sibling {second} streaming and no Remove", fixture.pid);

        probe.stage("normal Close first document");
        let surface = probe.surface(first);
        probe
            .send(
                client,
                ControlMessage::Application(App::Close {
                    surface_id: first,
                    geometry_generation: surface.geometry_generation,
                }),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while !probe.removed.contains(&first) {
                probe.receive(client, incoming).await;
                assert!(
                    probe.live.contains(&second),
                    "normal Close removed the wrong document"
                );
            }
        })
        .await
        .expect("first document must close normally");
        probe.stage("remaining document continues decoding");
        let remaining_count = probe.total_pictures(second);
        let steady = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < steady || !probe.recovering.is_empty() {
            probe.receive(client, incoming).await;
            assert_eq!(probe.live, BTreeSet::from([second]));
            assert!(!probe.bye, "closing one document must not end the session");
        }
        assert!(
            probe.total_pictures(second) >= remaining_count + 5 && probe.pictures(second) >= 5,
            "sibling capture must keep decoding"
        );

        probe.stage("normal Close last document / final Remove and Bye");
        let surface = probe.surface(second);
        probe
            .send(
                client,
                ControlMessage::Application(App::Close {
                    surface_id: second,
                    geometry_generation: surface.geometry_generation,
                }),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while !probe.bye {
                probe.receive(client, incoming).await;
            }
        })
        .await
        .expect("last normal Close must deliver Remove then UserClosed Bye");
        probe
            .send(
                client,
                ControlMessage::Bye {
                    reason: ByeReason::UserClosed,
                    detail: None,
                },
            )
            .await;
    }
}
