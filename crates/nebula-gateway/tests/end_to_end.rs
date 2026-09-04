//! The whole control and data path, in one process.
//!
//! This is the acceptance test for the architecture rather than for any one
//! crate: a real manager on Postgres, a real gateway, a real relay, an agent
//! holding an outbound control tunnel, and a client that starts from nothing
//! but a password and ends up exchanging encrypted frames with a machine it
//! was never told the address of.
//!
//! Point `NEBULA_TEST_DATABASE_URL` at a scratch database; the default is a
//! local `nebula_manager_test`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ndp_crypto::{Initiator, Responder, StaticKeypair};
use ndp_proto::{Channel, MsgHeader, MsgKind};
use ndp_signal::{
    AgentHello, AgentHelloAck, ClientHello, ClientHelloAck, GatewayMessage, RelayHello,
    RelayHelloAck, SessionRequest,
};
use ndp_transport::{
    client_endpoint, connect, CertificateFingerprint, Session, SessionReceiver, TransportConfig,
    ALPN_AGENT_GATEWAY, ALPN_RELAY, ALPN_SESSION,
};
use nebula_gateway::{Config as GatewayConfig, Gateway};
use nebula_relay::{Config as RelayConfig, Relay};
use serde_json::{json, Value};
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";
const BOOTSTRAP: &str = "test-bootstrap-token";

/// The Noise prologue binding a handshake to one session.
///
/// Both peers derive it from the session id they were each told separately,
/// so a handshake replayed into a different session fails outright.
fn prologue(session: &str) -> Vec<u8> {
    format!("ndp/3 session {session}").into_bytes()
}

/// A whole deployment, running in this process.
struct World {
    http: reqwest::Client,
    manager_url: String,
    owner_token: String,
    gateway: Arc<Gateway>,
    gateway_addr: SocketAddr,
    relay: Arc<Relay>,
}

impl World {
    async fn start() -> Self {
        let db = std::env::var("NEBULA_TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres:///nebula_manager_test".into());

        // The manager is reached at its bound address but issues tickets under
        // its public URL, which is how any deployment bigger than one laptop
        // is arranged. Keeping the two different here means every test in this
        // file would fail if the gateway went back to assuming they match.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let manager_url = format!("http://{}", listener.local_addr().unwrap());
        let issuer = "https://manager.public.test".to_owned();

        let state = nebula_manager::AppState::bootstrap(nebula_manager::Config {
            public_url: issuer.clone(),
            ..nebula_manager::Config::for_test(db)
        })
        .await
        .expect("the manager should start against the test database");
        let router = nebula_manager::routes::router(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        // Every test shares one database, so placement is only deterministic
        // if this deployment's nodes are the only ones in its region.
        let region = format!("r{}", Uuid::now_v7().simple());
        let pair_secret = ndp_signal::generate_secret();

        let relay = Arc::new(
            Relay::bind(&RelayConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                pair_secret: pair_secret.clone(),
                ..RelayConfig::default()
            })
            .unwrap(),
        );
        let running = Arc::clone(&relay);
        tokio::spawn(async move { running.run().await });

        let http = reqwest::Client::new();
        let world = Self {
            http: http.clone(),
            manager_url: manager_url.clone(),
            owner_token: String::new(),
            // Placeholders, replaced below once the gateway exists.
            gateway: Gateway::start(GatewayConfig {
                listen: "127.0.0.1:0".parse().unwrap(),
                advertised_addr: String::new(),
                name: format!("gw-{region}"),
                manager_url: manager_url.clone(),
                ticket_issuer: Some(issuer.clone()),
                bootstrap_secret: Some(BOOTSTRAP.into()),
                region: region.clone(),
                pair_secret: pair_secret.clone(),
                ..GatewayConfig::default()
            })
            .await
            .expect("the gateway should start"),
            gateway_addr: "127.0.0.1:0".parse().unwrap(),
            relay: Arc::clone(&relay),
        };

        // The relay is registered after it binds, because its pin and port
        // are only known once it has.
        let response = http
            .post(format!("{manager_url}/v1/relays"))
            .header("Authorization", format!("Bearer {BOOTSTRAP}"))
            .json(&json!({
                "name": format!("relay-{region}"),
                "quic_addr": relay.local_addr().unwrap().to_string(),
                "cert_pin": relay.fingerprint,
                "region": region,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201, "{}", response.text().await.unwrap());

        let gateway_addr = world.gateway.local_addr().unwrap();
        let serving = Arc::clone(&world.gateway);
        tokio::spawn(async move { serving.run().await });

        let mut world = World {
            gateway_addr,
            ..world
        };
        world.owner_token = world.create_tenant().await;
        world
    }

    async fn create_tenant(&self) -> String {
        let slug = format!("t{}", Uuid::now_v7().simple());
        self.post_as(
            "/v1/tenants",
            BOOTSTRAP,
            json!({
                "name": "Acme",
                "slug": slug,
                "owner_email": "owner@acme.test",
                "owner_password": PASSWORD,
                "owner_display_name": "Owner",
            }),
        )
        .await;

        let body = self
            .post(
                "/v1/auth/login",
                None,
                json!({ "tenant": slug, "email": "owner@acme.test", "password": PASSWORD }),
            )
            .await;
        body["access_token"].as_str().unwrap().to_string()
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

    async fn post_as(&self, path: &str, token: &str, body: Value) -> Value {
        self.post(path, Some(token), body).await
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

    /// Enrol a machine, publish its desktop, and let the owner launch it.
    async fn provision(&self, agent_key: &StaticKeypair) -> Provisioned {
        let name = format!("mac-{}", Uuid::now_v7().simple());
        let token = self
            .post_as(
                "/v1/machines/enrollment-tokens",
                &self.owner_token,
                json!({ "machine_name": name }),
            )
            .await["token"]
            .as_str()
            .unwrap()
            .to_string();

        let enrolled = self
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
                    "noise_public_key": hex::encode(agent_key.public().as_bytes()),
                }),
            )
            .await;
        let machine: Uuid = enrolled["machine_id"].as_str().unwrap().parse().unwrap();
        let credential = enrolled["credential"].as_str().unwrap().to_string();

        let resource = self
            .post_as(
                &format!("/v1/machines/{machine}/resources"),
                &self.owner_token,
                json!({ "kind": "DESKTOP", "name": "Desktop" }),
            )
            .await["id"]
            .as_str()
            .unwrap()
            .to_string();

        let me = self.get("/v1/auth/me", &self.owner_token).await;
        self.post_as(
            &format!("/v1/resources/{resource}/entitlements"),
            &self.owner_token,
            json!({
                "subject_kind": "USER",
                "subject_id": me["id"],
                "role": "CONTROLLER",
                "allow_clipboard": true,
                "allow_audio": true,
            }),
        )
        .await;

        Provisioned {
            machine,
            credential,
            resource,
        }
    }
}

struct Provisioned {
    machine: Uuid,
    credential: String,
    resource: String,
}

/// A stand-in for the real agent: enough of one to prove the path works.
struct Agent {
    requests: tokio::sync::mpsc::Receiver<SessionRequest>,
    conn: quinn::Connection,
    _endpoint: quinn::Endpoint,
}

impl Agent {
    /// Shut the tunnel down the way an agent going offline would.
    fn detach(&self) {
        self.conn.close(0u32.into(), b"agent shutting down");
    }
}

impl Agent {
    /// Open the outbound control tunnel and keep it.
    async fn attach(world: &World, machine: Uuid, credential: &str) -> Self {
        let cfg = TransportConfig::default();
        let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let pin = CertificateFingerprint::from_hex(&world.gateway.fingerprint).unwrap();
        let conn = connect(
            &endpoint,
            world.gateway_addr,
            "localhost",
            pin,
            ALPN_AGENT_GATEWAY,
            &cfg,
        )
        .await
        .expect("the agent should reach the gateway");

        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        ndp_signal::write_message(
            &mut send,
            &AgentHello {
                machine_id: nebula_common::MachineId::from_uuid(machine),
                credential: credential.to_string(),
                agent_version: "0.1.0".into(),
            },
        )
        .await
        .unwrap();

        let ack: AgentHelloAck = ndp_signal::read_message(&mut recv).await.unwrap();
        let AgentHelloAck::Accepted { .. } = ack else {
            panic!("the gateway refused the agent tunnel: {ack:?}");
        };

        let (tx, requests) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            // The agent must keep reading its tunnel for its whole life; the
            // gateway pushes session requests down it unprompted.
            while let Ok(message) = ndp_signal::read_message::<GatewayMessage>(&mut recv).await {
                if let GatewayMessage::StartSession(request) = message {
                    if tx.send(request).await.is_err() {
                        break;
                    }
                }
            }
            drop(send);
        });

        Self {
            requests,
            conn,
            _endpoint: endpoint,
        }
    }

    /// Serve the next session the gateway asks for.
    async fn serve_next(&mut self, keys: StaticKeypair) -> (Session, SessionReceiver) {
        let request = tokio::time::timeout(Duration::from_secs(10), self.requests.recv())
            .await
            .expect("the gateway should push a session request")
            .expect("the tunnel should still be open");

        let cfg = TransportConfig::default();
        let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let pin = CertificateFingerprint::from_hex(&request.relay_pin).unwrap();
        let conn = connect(
            &endpoint,
            request.relay_addr.parse().unwrap(),
            "localhost",
            pin,
            ALPN_RELAY,
            &cfg,
        )
        .await
        .unwrap();

        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        ndp_signal::write_message(
            &mut send,
            &RelayHello {
                pair_token: request.pair_token.clone(),
            },
        )
        .await
        .unwrap();

        let session_id = request.session.to_string();
        let policy = request.policy;
        tokio::spawn(async move {
            // Hold the endpoint open for as long as the session lasts.
            let _endpoint = endpoint;
            std::future::pending::<()>().await;
        });

        let ack: RelayHelloAck = ndp_signal::read_message(&mut recv).await.unwrap();
        assert!(matches!(ack, RelayHelloAck::Spliced), "{ack:?}");

        let responder = Responder::new(&keys, &prologue(&session_id)).unwrap();
        Session::accept(
            conn,
            responder,
            move |ticket| {
                // The agent enforces the policy the manager decided; it never
                // asks anyone, and it is never told who the user is.
                assert!(policy.input, "this session was granted control");
                assert!(!ticket.is_empty());
                Ok(b"agent-ready".to_vec())
            },
            &TransportConfig::default(),
        )
        .await
        .expect("the agent side of the handshake should complete")
    }
}

#[tokio::test]
async fn a_user_reaches_a_machine_they_were_never_told_the_address_of() {
    let world = World::start().await;

    let agent_keys = StaticKeypair::generate();
    let provisioned = world.provision(&agent_keys).await;
    let agent = Agent::attach(&world, provisioned.machine, &provisioned.credential).await;

    // The client's whole starting position: a token and a resource id. It has
    // never heard of the machine, the gateway or the relay.
    let ticket = world
        .post_as(
            "/v1/sessions",
            &world.owner_token,
            json!({ "resource_id": provisioned.resource, "client_os": "MACOS" }),
        )
        .await;
    let session_id = ticket["session_id"].as_str().unwrap().to_string();
    assert_eq!(
        ticket["gateway_addr"].as_str().unwrap(),
        world.gateway_addr.to_string(),
        "the manager should place the session on the gateway holding the tunnel"
    );

    // Redeem the ticket at the gateway.
    let client_keys = StaticKeypair::generate();
    let cfg = TransportConfig::default();
    let gw_endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let gw_conn = connect(
        &gw_endpoint,
        ticket["gateway_addr"].as_str().unwrap().parse().unwrap(),
        "localhost",
        CertificateFingerprint::from_hex(ticket["gateway_pin"].as_str().unwrap()).unwrap(),
        ALPN_SESSION,
        &cfg,
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

    let ack: ClientHelloAck = ndp_signal::read_message(&mut recv).await.unwrap();
    let ClientHelloAck::Accepted {
        relay_addr,
        relay_pin,
        pair_token,
        agent_key,
        ..
    } = ack
    else {
        panic!("the gateway refused the session: {ack:?}");
    };

    // The agent's key came from the manager via the ticket, so the client is
    // not trusting the gateway or the relay for it.
    assert_eq!(agent_key, hex::encode(agent_keys.public().as_bytes()));
    assert_eq!(agent_key, ticket["agent_key"].as_str().unwrap());

    let agent_side = tokio::spawn(agent_serve(agent_keys.clone(), agent));

    // Meet the agent at the relay.
    let relay_endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
    let relay_conn = connect(
        &relay_endpoint,
        relay_addr.parse().unwrap(),
        "localhost",
        CertificateFingerprint::from_hex(&relay_pin).unwrap(),
        ALPN_RELAY,
        &cfg,
    )
    .await
    .unwrap();
    let (mut rsend, mut rrecv) = relay_conn.open_bi().await.unwrap();
    ndp_signal::write_message(&mut rsend, &RelayHello { pair_token })
        .await
        .unwrap();
    let relay_ack: RelayHelloAck = ndp_signal::read_message(&mut rrecv).await.unwrap();
    assert!(matches!(relay_ack, RelayHelloAck::Spliced), "{relay_ack:?}");

    let initiator = Initiator::new(
        &client_keys,
        &ndp_crypto::PublicKey::from_slice(&hex::decode(&agent_key).unwrap()).unwrap(),
        &prologue(&session_id),
    )
    .unwrap();
    let (client, mut client_rx, greeting) = Session::initiate(
        relay_conn,
        initiator,
        ticket["ticket"].as_str().unwrap().as_bytes(),
        &TransportConfig::default(),
    )
    .await
    .expect("the end-to-end handshake should complete");
    assert_eq!(greeting, b"agent-ready");

    let (agent_session, mut agent_rx) = agent_side.await.unwrap();

    // A frame of "video" in one direction and a keystroke in the other.
    let frame = vec![0x42u8; 200_000];
    agent_session
        .send(
            Channel::Video,
            MsgHeader::new(MsgKind::VideoFrame, 1, 0),
            &frame,
        )
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), client_rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got.channel, Channel::Video);
    assert_eq!(got.payload, frame);

    client
        .send(
            Channel::Input,
            MsgHeader::new(MsgKind::InputEvent, 1, 0),
            &[1u8; 32],
        )
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), agent_rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got.channel, Channel::Input);
    assert_eq!(got.payload, vec![1u8; 32]);

    // The manager should be able to account for what just happened.
    let sessions = world.get("/v1/sessions", &world.owner_token).await;
    let mine = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"].as_str() == Some(session_id.as_str()))
        .expect("the session should be recorded");
    assert_eq!(mine["state"], "ACTIVE");

    world.gateway.shutdown();
    world.relay.shutdown();
}

/// Split out so the agent's work can run concurrently with the client's.
async fn agent_serve(keys: StaticKeypair, mut agent: Agent) -> (Session, SessionReceiver) {
    agent.serve_next(keys).await
}

#[tokio::test]
async fn a_ticket_cannot_be_redeemed_twice() {
    let world = World::start().await;
    let agent_keys = StaticKeypair::generate();
    let provisioned = world.provision(&agent_keys).await;
    let _agent = Agent::attach(&world, provisioned.machine, &provisioned.credential).await;

    let ticket = world
        .post_as(
            "/v1/sessions",
            &world.owner_token,
            json!({ "resource_id": provisioned.resource }),
        )
        .await;
    let token = ticket["ticket"].as_str().unwrap().to_string();
    let addr: SocketAddr = ticket["gateway_addr"].as_str().unwrap().parse().unwrap();
    let pin = CertificateFingerprint::from_hex(ticket["gateway_pin"].as_str().unwrap()).unwrap();

    let redeem = |token: String| async move {
        let cfg = TransportConfig::default();
        let endpoint = client_endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = connect(&endpoint, addr, "localhost", pin, ALPN_SESSION, &cfg)
            .await
            .unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        ndp_signal::write_message(
            &mut send,
            &ClientHello {
                ticket: token,
                noise_public_key: hex::encode([9u8; 32]),
                client_version: "0.1.0".into(),
            },
        )
        .await
        .unwrap();
        let ack: ClientHelloAck = ndp_signal::read_message(&mut recv).await.unwrap();
        ack
    };

    assert!(matches!(
        redeem(token.clone()).await,
        ClientHelloAck::Accepted { .. }
    ));
    assert!(
        matches!(redeem(token).await, ClientHelloAck::Rejected { .. }),
        "a captured ticket must be worthless once it has been used"
    );

    world.gateway.shutdown();
    world.relay.shutdown();
}

#[tokio::test]
async fn a_session_is_refused_when_the_machine_has_no_tunnel() {
    let world = World::start().await;
    let agent_keys = StaticKeypair::generate();
    let provisioned = world.provision(&agent_keys).await;

    // Attach and then drop the tunnel, so the machine is enrolled and was
    // recently online but is not reachable now.
    {
        let agent = Agent::attach(&world, provisioned.machine, &provisioned.credential).await;
        agent.detach();
    }
    // Let the gateway notice the connection went.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let response = world
        .http
        .post(format!("{}/v1/sessions", world.manager_url))
        .header("Authorization", format!("Bearer {}", world.owner_token))
        .json(&json!({ "resource_id": provisioned.resource }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        409,
        "a machine whose agent has gone must not be offered"
    );

    world.gateway.shutdown();
    world.relay.shutdown();
}
