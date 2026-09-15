//! Explicit, ignored diagnostics for one owned fixture transaction; no transport fallback.

use super::*;
use crate::media::{FrameSink, VideoConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::sync::{atomic::AtomicU64, mpsc, Arc};

#[derive(Clone, Serialize, Deserialize)]
struct Peer {
    pid: i32,
    launched_bits: u64,
    bundle: PathBuf,
    cg_id: u32,
    directory: PathBuf,
}

impl Peer {
    fn identity(&self) -> Identity {
        Identity {
            pid: self.pid,
            launched: f64::from_bits(self.launched_bits),
            bundle: self.bundle.clone(),
        }
    }

    fn fresh_window(&self) -> anyhow::Result<Ax> {
        self.identity().check()?;
        let (pid, bounds) = live_window(self.cg_id)?;
        anyhow::ensure!(pid == self.pid, "diagnostic window owner changed");
        let mut matching = Ax::application(self.pid)?
            .window_tree()?
            .into_iter()
            .filter(|(window, parent)| {
                parent.is_none() && window.bounds().is_ok_and(|b| b.matches(bounds))
            })
            .map(|(window, _)| window);
        let window = matching
            .next()
            .ok_or_else(|| anyhow::anyhow!("diagnostic window absent"))?;
        anyhow::ensure!(matching.next().is_none(), "ambiguous diagnostic window");
        anyhow::ensure!(window.owns_content(self.pid)?, "unowned diagnostic content");
        anyhow::ensure!(
            window.string("AXRole")? == "AXWindow",
            "unexpected diagnostic role"
        );
        Ok(window)
    }
}

fn unix_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        * 1000.0
}

fn sample(check: &str, read: impl FnOnce() -> anyhow::Result<bool>) -> Value {
    let begin = unix_ms();
    let started = Instant::now();
    let result = read();
    let error = result.as_ref().err();
    let native = error.and_then(|error| error.downcast_ref::<AxAttributeError>());
    json!({
        "check": check, "begin_unix_ms": begin, "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        "ok": result.is_ok(), "matches_expected": result.as_ref().ok(),
        "status": native.map(|error| error.status),
        "error": error.map(|error| error.to_string().chars().take(160).collect::<String>()),
        "observer_pid": std::process::id(),
        "thread": std::thread::current().name()
    })
}

fn observe(peer: &Peer, window: &Ax) -> anyhow::Result<Vec<Value>> {
    peer.identity().check()?;
    // The first query is simultaneous across clients. A later application
    // query distinguishes a persistent failure from transition-time recovery.
    let role = sample("window.AXRole", || {
        window.string("AXRole").map(|role| role == "AXWindow")
    });
    std::thread::sleep(Duration::from_millis(100));
    let hidden = sample("fresh-application.AXHidden", || {
        Ax::application(peer.pid)?.boolean("AXHidden").map(|v| !v)
    });
    Ok(vec![role, hidden])
}

fn await_file(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "diagnostic handshake timed out: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct ObserverChild(Child);
impl Drop for ObserverChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            // Only this read-only diagnostic subprocess, never the fixture or a user's app.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct FixtureCleanup(Identity);
impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if self.0.check().is_ok() {
            if let Some(app) =
                NSRunningApplication::runningApplicationWithProcessIdentifier(self.0.pid)
            {
                let _ = app.terminate();
            }
        }
    }
}

#[test]
#[ignore = "internal read-only peer for the explicit AX transaction experiment"]
fn native_ax_transaction_peer() {
    let config = std::env::var_os("NEBULA_AX_TRANSACTION_PEER_CONFIG")
        .expect("explicit peer config required");
    let peer: Peer = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    let window = peer.fresh_window().unwrap();
    std::fs::write(peer.directory.join("child-ready"), b"ready").unwrap();
    await_file(&peer.directory.join("go"), Duration::from_secs(15));
    let observations = observe(&peer, &window).unwrap();
    std::fs::write(
        peer.directory.join("child-result.json"),
        serde_json::to_vec_pretty(&observations).unwrap(),
    )
    .unwrap();
}

#[test]
#[ignore = "one minimize transaction on an explicit independent fixture; compares three AX clients"]
fn native_ax_transaction_compare_clients() {
    run_transaction(false);
}

#[test]
#[ignore = "one owned minimize; tests fresh pre-mutation proof with the unchanged 200ms refresh cadence"]
fn native_minimize_fresh_snapshot_phase_probe() {
    run_transaction(true);
}

fn run_transaction(align_refresh: bool) {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let _lease = crate::application::ControllerLease::acquire(true).unwrap();
    let bundle =
        PathBuf::from(std::env::var_os("NEBULA_APP_PROBE_PATH").expect("explicit fixture path"));
    assert_eq!(bundle.file_name().unwrap(), "NebulaSeamlessFixture.app");
    let root = PathBuf::from(
        std::env::var_os("NEBULA_AX_TRANSACTION_ROOT")
            .expect("explicit project-local diagnostic root"),
    );
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    assert!(root.canonicalize().unwrap().starts_with(workspace));
    let directory = root.join(format!("run-{}", uuid::Uuid::now_v7()));
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
    }
    let mut app = MacApplication::launch(&ApplicationLaunch {
        launch_path: bundle.to_string_lossy().into_owned(),
        launch_args: vec!["--ax-transaction-observation".into()],
        working_dir: None,
    })
    .unwrap();
    let cleanup = FixtureCleanup(app.identity.clone());
    eprintln!(
        "AX transaction owned fixture PID {}, launch {}, bundle {:?}, records {:?}",
        app.identity.pid, app.identity.launched, app.identity.bundle, directory
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        app.snapshot().unwrap();
        if app.windows.len() == 2 && app.unavailable.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "fixture authorization did not become ready"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let target = app
        .windows
        .values()
        .find(|w| w.surface.title == "Nebula acceptance document 1")
        .unwrap()
        .clone();
    let sibling = app
        .windows
        .values()
        .find(|w| w.surface.title == "Nebula acceptance document 2")
        .unwrap()
        .clone();
    let mut videos = Vec::new();
    let mut receivers = Vec::new();
    for window in [&target, &sibling] {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let mut video = app.video(window.surface.native_id).unwrap();
        video
            .start(
                VideoConfig {
                    width: window.surface.width,
                    height: window.surface.height,
                    fps: 15,
                    bitrate: 2_000_000,
                },
                FrameSink::application(
                    tx,
                    Arc::new(AtomicU64::new(1)),
                    Arc::new(AtomicU64::new(0)),
                ),
            )
            .unwrap();
        videos.push(video);
        receivers.push(rx);
    }
    for receiver in &mut receivers {
        let deadline = Instant::now() + Duration::from_secs(10);
        while receiver.try_recv().is_err() {
            assert!(
                Instant::now() < deadline,
                "native capture produced no frame"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if align_refresh {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.snapshot().unwrap();
            if app.unavailable.is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "pre-mutation snapshot did not fully verify"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        let verified_at = Instant::now();
        let verified_unix_ms = unix_ms();
        let target = app.windows[&target.surface.native_id].clone();
        let sibling = app.windows[&sibling.surface.native_id].clone();
        let began = unix_ms();
        let operation = app.operate(
            target.surface.native_id,
            &ApplicationMessage::Minimize {
                surface_id: 0,
                geometry_generation: target.surface.geometry_generation,
            },
        );
        let operation_end = unix_ms();
        // Match the existing worker's refresh period, measured from actual
        // completed verification, not from the mutation or a cached result.
        let due = verified_at + Duration::from_millis(200);
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let refresh_begin = unix_ms();
        let snapshot = app.snapshot();
        let refresh_end = unix_ms();
        let continuous = snapshot.is_ok()
            && !app.unavailable.contains(&sibling.surface.native_id)
            && app
                .windows
                .get(&sibling.surface.native_id)
                .is_some_and(|window| {
                    window.surface.geometry_generation == sibling.surface.geometry_generation
                });
        let result = json!({
            "pid":app.identity.pid, "launched":app.identity.launched,
            "fresh_verified_unix_ms":verified_unix_ms, "minimize_begin_unix_ms":began,
            "minimize_end_unix_ms":operation_end, "operation_ok":operation.is_ok(),
            "operation_error":operation.as_ref().err().map(ToString::to_string),
            "refresh_begin_unix_ms":refresh_begin, "refresh_end_unix_ms":refresh_end,
            "refresh_ok":snapshot.is_ok(), "unavailable":app.unavailable(),
            "sibling_id":sibling.surface.native_id, "sibling_generation":sibling.surface.geometry_generation,
            "sibling_still_verified":continuous, "cadence_ms":200, "continuity_budget_ms":250,
        });
        std::fs::write(
            directory.join("phase-result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        eprintln!("AX fresh-snapshot phase result {result}");
        for video in &mut videos {
            video.stop();
        }
        drop(cleanup);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.identity.check().is_ok() {
            assert!(
                Instant::now() < deadline,
                "owned phase fixture normal Quit did not complete"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        operation.unwrap();
        snapshot.unwrap();
        assert!(
            continuous,
            "fresh pre-mutation proof did not preserve sibling authority"
        );
        return;
    }
    let peer = Peer {
        pid: app.identity.pid,
        launched_bits: app.identity.launched.to_bits(),
        bundle: app.identity.bundle.clone(),
        cg_id: sibling.cg_id,
        directory: directory.clone(),
    };
    let retained = sibling.ax_identity.as_ref().unwrap().element();
    let baseline = peer.fresh_window().unwrap();
    let same_pointer = retained.0 == baseline.0;
    assert!(unsafe { CFEqual(retained.0, baseline.0) });
    let retained_copy = sibling.ax_identity.as_ref().unwrap().clone();
    let thread_peer = peer.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (go_tx, go_rx) = mpsc::sync_channel(1);
    let fresh_thread = std::thread::Builder::new()
        .name("fresh-AX-client-thread".into())
        .spawn(move || {
            let window = thread_peer.fresh_window().unwrap();
            assert!(retained_copy.matches(&window));
            ready_tx.send(window.0 == retained_copy.0).unwrap();
            go_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            observe(&thread_peer, &window).unwrap()
        })
        .unwrap();
    let same_thread_pointer = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let config_path = directory.join("peer.json");
    std::fs::write(&config_path, serde_json::to_vec(&peer).unwrap()).unwrap();
    let stdout = std::fs::File::create(directory.join("child.log")).unwrap();
    let mut child = ObserverChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "platform::macos::application::diagnostics::native_ax_transaction_peer",
                "--nocapture",
            ])
            .env("NEBULA_AX_TRANSACTION_PEER_CONFIG", &config_path)
            .stdout(stdout.try_clone().unwrap())
            .stderr(stdout)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    eprintln!("AX transaction observer child PID {}", child.0.id());
    await_file(&directory.join("child-ready"), Duration::from_secs(10));
    assert_eq!(retained.string("AXRole").unwrap(), "AXWindow");
    // Identical validation and public setter to operate(Minimize); retain the
    // actual AX return code rather than collapsing it into an operation error.
    let (_, element) = app
        .checked_window(
            target.surface.native_id,
            target.surface.geometry_generation,
            false,
        )
        .unwrap();
    let key = cf_string("AXMinimized").unwrap();
    let minimize_begin = unix_ms();
    let started = Instant::now();
    let status = unsafe { AXUIElementSetAttributeValue(element.0, key.0, kCFBooleanTrue) };
    let minimize = json!({"begin_unix_ms": minimize_begin, "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0, "status":status});
    let discovery = sample("post-minimize.AXWindows", || {
        Ax::application(peer.pid)?
            .children("AXWindows")
            .map(|_| true)
    });
    std::fs::write(directory.join("go"), b"go").unwrap();
    go_tx.send(()).unwrap();
    let retained_observations = observe(&peer, &retained).unwrap();
    let fresh_observations = fresh_thread.join().unwrap();
    await_file(&directory.join("child-result.json"), Duration::from_secs(5));
    let independent: Value =
        serde_json::from_slice(&std::fs::read(directory.join("child-result.json")).unwrap())
            .unwrap();
    assert!(child.0.wait().unwrap().success());
    std::thread::sleep(Duration::from_millis(500));
    let timeline = bundle
        .parent()
        .unwrap()
        .join("status")
        .join(format!("ax-transaction-{}.json", peer.pid));
    let heartbeat: Value = serde_json::from_slice(&std::fs::read(&timeline).unwrap()).unwrap();
    let result = json!({
        "identity": {"pid":peer.pid,"launched":peer.identity().launched,"bundle":peer.bundle},
        "minimize":minimize, "discovery":discovery,
        "same_pointer_fresh_same_thread":same_pointer,
        "same_pointer_fresh_other_thread":same_thread_pointer,
        "retained":retained_observations, "fresh_thread":fresh_observations,
        "independent_process":independent, "fixture_heartbeat":heartbeat
    });
    std::fs::write(
        directory.join("result.json"),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    eprintln!(
        "AX transaction result {}",
        json!({
            "minimize":result["minimize"],"discovery":result["discovery"],
            "same_pointer_fresh_same_thread":same_pointer,"same_pointer_fresh_other_thread":same_thread_pointer,
            "retained":result["retained"],"fresh_thread":result["fresh_thread"],"independent_process":result["independent_process"]
        })
    );
    for video in &mut videos {
        video.stop();
    }
    drop(cleanup);
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.identity.check().is_ok() {
        assert!(
            Instant::now() < deadline,
            "owned fixture normal Quit did not complete"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
