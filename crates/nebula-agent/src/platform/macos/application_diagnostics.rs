//! Explicit, ignored diagnostics for one owned fixture transaction; no transport fallback.

use super::*;
use crate::media::{FrameSink, VideoConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use std::sync::{atomic::AtomicU64, mpsc, Arc};

struct IdentityObservation {
    expected: Identity,
    retained: Option<objc2::rc::Retained<NSRunningApplication>>,
    output: Arc<std::sync::Mutex<std::fs::File>>,
    first_none: bool,
    transaction_phase: String,
}

thread_local! {
    static IDENTITY_OBSERVATION: std::cell::RefCell<Option<IdentityObservation>> =
        const { std::cell::RefCell::new(None) };
}

fn identity_value(expected: &Identity, app: &NSRunningApplication) -> Value {
    let observed = Identity::read(app);
    json!({
        "object": format!("{:p}", app), "terminated": app.isTerminated(),
        "pid": app.processIdentifier(),
        "launch_bits": app.launchDate().map(|date| date.timeIntervalSince1970().to_bits()),
        "bundle": app.bundleURL().and_then(|url| url.path()).map(|path| path.to_string()),
        "canonical_bundle": observed.as_ref().ok().map(|value| &value.bundle),
        "exact_identity_match": observed.as_ref().is_ok_and(|value|
            value.pid == expected.pid && value.launched == expected.launched && value.bundle == expected.bundle),
        "read_error": observed.err().map(|error| error.to_string())
    })
}

fn identity_views(expected: &Identity, factory_was_none: bool) -> Value {
    let factory = if factory_was_none {
        None
    } else {
        NSRunningApplication::runningApplicationWithProcessIdentifier(expected.pid)
    };
    let enumerated: Vec<_> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|app| app.processIdentifier() == expected.pid)
        .map(|app| identity_value(expected, &app))
        .collect();
    let mut executable = [0u8; 4096];
    let count = unsafe {
        proc_pidpath(
            expected.pid,
            executable.as_mut_ptr().cast(),
            executable.len() as u32,
        )
    };
    let end = executable
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(executable.len());
    json!({
        "unix_ms": unix_ms(), "main_thread": unsafe { pthread_main_np() } != 0,
        "thread": std::thread::current().name(), "pid": expected.pid,
        "expected_launch_bits": expected.launched.to_bits(), "expected_bundle": expected.bundle,
        "factory": factory.as_ref().map(|app| identity_value(expected, app)),
        "factory_original_none_not_retried": factory_was_none,
        "workspace_enumerated": enumerated,
        "os_executable": (count > 0).then(|| String::from_utf8_lossy(&executable[..end]).into_owned())
    })
}

fn write_identity_observation(output: &std::sync::Mutex<std::fs::File>, value: &Value) {
    use std::io::Write;
    let mut file = output.lock().unwrap();
    serde_json::to_writer(&mut *file, value).unwrap();
    writeln!(file).unwrap();
    file.flush().unwrap();
}

pub(super) struct IdentityDiagnostic;

impl IdentityDiagnostic {
    pub(super) fn start(expected: &Identity) -> Option<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let root = PathBuf::from(std::env::var_os("NEBULA_IDENTITY_DIAGNOSTIC_ROOT")?)
            .canonicalize()
            .unwrap();
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        assert!(
            root.starts_with(workspace.join("target")),
            "diagnostic root must be project-local target"
        );
        let path = root.join(format!(
            "identity-{}-{}.jsonl",
            expected.pid,
            uuid::Uuid::now_v7()
        ));
        let output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        IDENTITY_OBSERVATION.with(|cell| {
            assert!(cell.borrow().is_none());
            *cell.borrow_mut() = Some(IdentityObservation {
                expected: expected.clone(),
                retained: NSRunningApplication::runningApplicationWithProcessIdentifier(
                    expected.pid,
                ),
                output: Arc::new(std::sync::Mutex::new(output)),
                first_none: false,
                transaction_phase: "initializing".into(),
            });
        });
        eprintln!("identity diagnostic output {}", path.display());
        identity_phase("after-launch");
        Some(Self)
    }
}

impl Drop for IdentityDiagnostic {
    fn drop(&mut self) {
        identity_phase("diagnostic-end");
        IDENTITY_OBSERVATION.with(|cell| cell.borrow_mut().take());
    }
}

pub(super) fn identity_phase(phase: &str) {
    IDENTITY_OBSERVATION.with(|cell| {
        if let Some(observation) = cell.borrow_mut().as_mut() {
            observation.transaction_phase = phase.into();
            let mut value = identity_views(&observation.expected, false);
            value["phase"] = json!(phase);
            value["retained_diagnostic_only"] = observation
                .retained
                .as_ref()
                .map(|app| identity_value(&observation.expected, app))
                .unwrap_or(Value::Null);
            write_identity_observation(&observation.output, &value);
        }
    });
}

pub(super) fn identity_lookup_unavailable(expected: &Identity, phase: &str) {
    IDENTITY_OBSERVATION.with(|cell| {
        let mut borrowed = cell.borrow_mut();
        let Some(observation) = borrowed.as_mut() else {
            return;
        };
        if observation.first_none {
            return;
        }
        observation.first_none = true;
        let mut value = identity_views(expected, true);
        value["phase"] = json!(phase);
        value["first_identity_factory_none"] = json!(true);
        value["transaction_phase"] = json!(observation.transaction_phase);
        value["retained_diagnostic_only"] = observation
            .retained
            .as_ref()
            .map(|app| identity_value(expected, app))
            .unwrap_or(Value::Null);
        write_identity_observation(&observation.output, &value);
        let expected = expected.clone();
        let output = observation.output.clone();
        let (tx, rx) = mpsc::sync_channel(1);
        let block = RcBlock::new(move || {
            let mut value = identity_views(&expected, false);
            value["phase"] = json!("main-thread-read-only-after-first-none");
            write_identity_observation(&output, &value);
            let _ = tx.send(());
        });
        unsafe {
            CFRunLoopPerformBlock(
                CFRunLoopGetMain(),
                kCFRunLoopCommonModes,
                &*block as *const _ as *const c_void,
            );
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
        let observed = rx.recv_timeout(Duration::from_millis(100)).is_ok();
        write_identity_observation(
            &observation.output,
            &json!({
                "phase": "main-thread-query-budget", "completed_within_100ms": observed,
                "original_identity_error_preserved": true, "unix_ms": unix_ms()
            }),
        );
    });
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetMain() -> *const c_void;
    fn CFRunLoopPerformBlock(run_loop: *const c_void, mode: *const c_void, block: *const c_void);
    fn CFRunLoopWakeUp(run_loop: *const c_void);
    static kCFRunLoopCommonModes: *const c_void;
    fn pthread_main_np() -> i32;
}

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
