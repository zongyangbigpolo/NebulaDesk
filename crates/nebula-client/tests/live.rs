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
    /// Publish a desktop and hand back the clipboard the agent shares, so a
    /// test can act as the person sitting at the machine.
    async fn publish_a_desktop_with_a_clipboard(
        &self,
        name: &str,
    ) -> (String, nebula_agent::clipboard::MemoryClipboard) {
        let board = nebula_agent::clipboard::MemoryClipboard::default();
        let resource = self
            .publish_a_desktop_on(
                name,
                TestPattern {
                    clipboard: board.clone(),
                },
            )
            .await;
        (resource, board)
    }

    async fn publish_a_desktop(&self, name: &str) -> String {
        self.publish_a_desktop_on(name, TestPattern::default())
            .await
    }

    async fn publish_a_desktop_on(&self, name: &str, platform: TestPattern) -> String {
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

        let agent = Agent::new(identity, Arc::new(platform)).unwrap();
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
            json!({
                "subject_kind": "USER",
                "subject_id": me["id"],
                "role": "CONTROLLER",
                "allow_file_transfer": true,
            }),
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

/// Copying on the remote machine should make the content available here, and
/// copying here should put it on the remote machine — without either side
/// bouncing the other's content back.
#[tokio::test]
async fn the_clipboard_crosses_in_both_directions() {
    use ndp_proto::{ClipboardDataHeader, ClipboardFormat, ClipboardOffer, ClipboardRequest};
    use nebula_agent::clipboard::Contents;

    let deployment = Deployment::start().await;
    let name = format!("mac-{}", Uuid::now_v7().simple());
    let (resource_id, board) = deployment.publish_a_desktop_with_a_clipboard(&name).await;

    let client = ManagerClient::login(
        &deployment.manager_url,
        &deployment.slug,
        "owner@acme.test",
        PASSWORD,
    )
    .await
    .unwrap();

    let ticket = client.open(&resource_id).await.unwrap();
    assert!(
        ticket.policy.clipboard,
        "a controller's entitlement grants the clipboard"
    );

    let connected = nebula_client::connect_to_agent(&ticket).await.unwrap();
    let session = connected.session;
    let mut incoming = connected.incoming;

    /// Wait for a clipboard message, ignoring the media flowing past.
    async fn next(
        incoming: &mut nebula_client::ConnectedReceiver,
    ) -> Result<ndp_transport::Incoming, tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = incoming.recv().await.unwrap().unwrap();
                if message.channel == Channel::Clipboard {
                    return message;
                }
            }
        })
        .await
    }

    // The Noise greeting precedes native source/clipboard startup. Wait for
    // the worker to answer an offer before making a local copy, otherwise its
    // initial snapshot may correctly treat the copy as pre-session content.
    session
        .send(
            Channel::Clipboard,
            MsgHeader::new(MsgKind::ClipboardOffer, 0, 0),
            &serde_json::to_vec(&ClipboardOffer {
                offer_id: 0,
                formats: vec![ClipboardFormat::Text],
                size_hint: 0,
            })
            .unwrap(),
        )
        .await
        .unwrap();
    let ready = next(&mut incoming)
        .await
        .expect("the clipboard worker should finish its initial snapshot");
    assert_eq!(ready.header.kind, MsgKind::ClipboardRequest);
    let ready: ClipboardRequest = serde_json::from_slice(&ready.payload).unwrap();
    assert_eq!(ready.offer_id, 0);

    // --- the machine's clipboard reaches the client -----------------------
    board.put(Contents::text("copied on the machine"));

    let offer = next(&mut incoming)
        .await
        .expect("the copy should be offered");
    assert_eq!(offer.header.kind, MsgKind::ClipboardOffer);
    let offer: ClipboardOffer = serde_json::from_slice(&offer.payload).unwrap();
    assert_eq!(offer.formats, vec![ClipboardFormat::Text]);

    session
        .send(
            Channel::Clipboard,
            MsgHeader::new(MsgKind::ClipboardRequest, 0, 0),
            &serde_json::to_vec(&ClipboardRequest {
                offer_id: offer.offer_id,
                format: ClipboardFormat::Text,
            })
            .unwrap(),
        )
        .await
        .unwrap();

    let data = next(&mut incoming).await.expect("the request is answered");
    assert_eq!(data.header.kind, MsgKind::ClipboardData);
    let (header, bytes) = ClipboardDataHeader::split(&data.payload).unwrap();
    assert_eq!(header.offer_id, offer.offer_id);
    assert_eq!(bytes, b"copied on the machine");

    // --- and the client's clipboard reaches the machine -------------------
    session
        .send(
            Channel::Clipboard,
            MsgHeader::new(MsgKind::ClipboardOffer, 1, 0),
            &serde_json::to_vec(&ClipboardOffer {
                offer_id: 1,
                formats: vec![ClipboardFormat::Text],
                size_hint: 20,
            })
            .unwrap(),
        )
        .await
        .unwrap();

    let request = next(&mut incoming).await.expect("the agent should want it");
    assert_eq!(request.header.kind, MsgKind::ClipboardRequest);
    let request: ClipboardRequest = serde_json::from_slice(&request.payload).unwrap();
    assert_eq!(request.offer_id, 1);

    let header = ClipboardDataHeader {
        offer_id: 1,
        format: ClipboardFormat::Text,
    };
    session
        .send(
            Channel::Clipboard,
            MsgHeader::new(MsgKind::ClipboardData, 2, 0),
            &header.payload(b"copied on the client"),
        )
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        while board.peek() != Some(Contents::text("copied on the client")) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the machine's clipboard should end up with what was copied here");

    // The loop that would otherwise never stop: the agent must not turn
    // around and announce back the content it was just handed.
    let bounced = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let message = incoming.recv().await.unwrap().unwrap();
            if message.channel == Channel::Clipboard {
                return message;
            }
        }
    })
    .await;
    assert!(
        bounced.is_err(),
        "the agent announced back what the client just sent it"
    );
}

/// A file dropped onto the session should land intact on the other machine.
#[tokio::test]
async fn a_file_crosses_the_session_intact() {
    use ndp_proto::{FileAck, FileChunkHeader, FileOffer};

    // The agent under test runs in this process, so point its downloads at
    // scratch space rather than the account's real Downloads folder.
    let downloads = tempfile::tempdir().unwrap();
    std::env::set_var("NEBULA_DOWNLOADS", downloads.path());

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
    assert!(ticket.policy.file_transfer, "the entitlement granted it");

    let connected = nebula_client::connect_to_agent(&ticket).await.unwrap();
    let session = connected.session;
    let mut incoming = connected.incoming;

    // Big enough to need several chunks and a window that has to be
    // reopened, and not a round multiple of either.
    let contents: Vec<u8> = (0..nebula_agent::files::CHUNK * 5 + 123)
        .map(|i| (i % 251) as u8)
        .collect();
    let offer = FileOffer {
        transfer_id: 1,
        name: "handover.bin".into(),
        size: contents.len() as u64,
        blake3: blake3::hash(&contents).to_hex().to_string(),
        modified_secs: None,
    };
    let mut seq = 0u32;
    let mut send = |kind: MsgKind, payload: Vec<u8>| {
        seq += 1;
        let session = session.clone();
        async move {
            session
                .send(Channel::File, MsgHeader::new(kind, seq, 0), &payload)
                .await
                .unwrap();
        }
    };

    send(MsgKind::FileOffer, serde_json::to_vec(&offer).unwrap()).await;

    let ack = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let message = incoming.recv().await.unwrap().unwrap();
            if message.channel == Channel::File {
                return message;
            }
        }
    })
    .await
    .expect("the agent should agree to receive the file");
    assert_eq!(ack.header.kind, MsgKind::FileAck);
    let ack = FileAck::decode(&ack.payload).unwrap();
    assert_eq!(ack.transfer_id, 1);
    assert!(
        ack.window > 0,
        "an accepted file comes with room to send it"
    );

    for (index, chunk) in contents.chunks(nebula_agent::files::CHUNK).enumerate() {
        let header = FileChunkHeader {
            transfer_id: 1,
            offset: (index * nebula_agent::files::CHUNK) as u64,
        };
        let mut payload = header.to_bytes().to_vec();
        payload.extend_from_slice(chunk);
        send(MsgKind::FileChunk, payload).await;
    }

    let landed = downloads.path().join("handover.bin");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if std::fs::read(&landed).is_ok_and(|got| got == contents) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the file should arrive byte for byte");

    // And never under its final name before it is whole: a half-written
    // file that looks finished is worse than one that never arrived.
    assert!(
        !downloads.path().join("handover.nebulapart").exists(),
        "the partial file should have been renamed away"
    );
}
