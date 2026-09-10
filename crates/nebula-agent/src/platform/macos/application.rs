//! Owned-instance LaunchServices launch, AX relationship checks and SCK windows.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{c_void, CString};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use block2::RcBlock;
use ndp_proto::application::ApplicationMessage;
use ndp_proto::InputEvent;
use nebula_common::application::{
    ApplicationUnavailableReason, APPLICATION_PROTOCOL_VERSION, MAX_APPLICATION_SURFACES,
};
use nebula_common::{ApplicationCapability, ApplicationLaunch};
use objc2_app_kit::{
    NSApplicationActivationOptions, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{NSArray, NSError, NSString, NSURL};
use screencapturekit::shareable_content::SCShareableContent;

use crate::application::{ApplicationBackend, NativeSurface};
use crate::media::VideoSource;

use super::{capture, input};

/// Probe prerequisites without displaying consent prompts.
pub fn capability() -> ApplicationCapability {
    // Probe permissions, native API availability and the real encoder here.
    // Expensive window enumeration belongs to authenticated discovery, not every heartbeat.
    let reason = if !capture::screen_capture_allowed() || !input::trusted() {
        Some(ApplicationUnavailableReason::PermissionDenied)
    } else if capture::application_encoder_available().is_err() {
        Some(ApplicationUnavailableReason::BackendUnavailable)
    } else {
        None
    };
    ApplicationCapability {
        supported: reason.is_none(),
        protocol_version: APPLICATION_PROTOCOL_VERSION,
        max_surfaces: if reason.is_none() {
            MAX_APPLICATION_SURFACES
        } else {
            0
        },
        reason,
        global_menu_supported: false,
    }
}

#[derive(Clone)]
struct Identity {
    pid: i32,
    launched: f64,
    bundle: PathBuf,
}

impl Identity {
    fn read(app: &NSRunningApplication) -> anyhow::Result<Self> {
        anyhow::ensure!(!app.isTerminated(), "application exited");
        let launched = app
            .launchDate()
            .ok_or_else(|| anyhow::anyhow!("no launch identity"))?
            .timeIntervalSince1970();
        let path = app
            .bundleURL()
            .and_then(|url| url.path())
            .ok_or_else(|| anyhow::anyhow!("no application bundle identity"))?;
        Ok(Self {
            pid: app.processIdentifier(),
            launched,
            bundle: PathBuf::from(path.to_string()).canonicalize()?,
        })
    }

    fn check(&self) -> anyhow::Result<()> {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            .ok_or_else(|| anyhow::anyhow!("owned application exited"))?;
        let current = Self::read(&app)?;
        anyhow::ensure!(
            current.launched == self.launched && current.bundle == self.bundle,
            "application process identity changed"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Bounds {
    fn matches(self, other: Self) -> bool {
        [
            self.x - other.x,
            self.y - other.y,
            self.width - other.width,
            self.height - other.height,
        ]
        .iter()
        .all(|v| v.abs() < 1.0)
    }

    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && (2.0..=8192.0).contains(&self.width)
            && (2.0..=8192.0).contains(&self.height)
    }
}

#[derive(Clone)]
struct Window {
    cg_id: u32,
    resumable_from: Option<u32>,
    bounds: Bounds,
    surface: NativeSurface,
}

#[derive(Debug, thiserror::Error)]
#[error("AX attribute {attribute} unavailable (status {status})")]
struct AxAttributeError {
    attribute: String,
    status: i32,
}

impl AxAttributeError {
    fn is_readiness(&self) -> bool {
        // InvalidUIElement, CannotComplete and NoValue occur while a newly
        // launched app publishes its AX server, or while that server is busy.
        // AttributeUnsupported/API-disabled are not startup readiness.
        matches!(self.status, -25202 | -25204 | -25212)
    }
}

fn owns_surface_content(
    owner_pid: i32,
    root_pid: i32,
    children: &[(i32, bool)],
    activation_owned: bool,
) -> bool {
    root_pid == owner_pid
        && if children.is_empty() {
            activation_owned
        } else {
            children
                .iter()
                .any(|(pid, associated)| *pid == owner_pid && *associated)
        }
}

/// A launch owns only a newly created PID with a stable LaunchServices identity.
pub struct MacApplication {
    identity: Identity,
    windows: BTreeMap<u64, Window>,
    unavailable: BTreeSet<u64>,
    retired: BTreeSet<u32>,
    next_id: u64,
    input: Option<(u64, u32, input::ApplicationInput)>,
    first_window_deadline: Instant,
    seen_window: bool,
}

impl MacApplication {
    /// Launch a trusted .app using separate argv, never a shell.
    pub fn launch(launch: &ApplicationLaunch) -> anyhow::Result<Self> {
        launch.validate()?;
        anyhow::ensure!(
            launch.working_dir.is_none(),
            "working directory is unsupported for LaunchServices applications"
        );
        anyhow::ensure!(
            capability().is_supported(),
            "application prerequisites are unavailable"
        );
        let bundle = PathBuf::from(&launch.launch_path).canonicalize()?;
        anyhow::ensure!(
            bundle.is_dir() && bundle.extension().is_some_and(|s| s == "app"),
            "published macOS application must be a .app bundle"
        );
        let workspace = NSWorkspace::sharedWorkspace();
        let existing: BTreeSet<i32> = workspace
            .runningApplications()
            .iter()
            .map(|app| app.processIdentifier())
            .collect();
        let configuration = NSWorkspaceOpenConfiguration::configuration();
        configuration.setCreatesNewApplicationInstance(true);
        configuration.setAllowsRunningApplicationSubstitution(false);
        configuration.setPromptsUserIfNeeded(false);
        configuration.setAddsToRecentItems(false);
        configuration.setActivates(true);
        let args: Vec<_> = launch
            .launch_args
            .iter()
            .map(|arg| NSString::from_str(arg))
            .collect();
        configuration.setArguments(&NSArray::from_retained_slice(&args));
        let url = NSURL::fileURLWithPath_isDirectory(
            &NSString::from_str(&bundle.to_string_lossy()),
            true,
        );
        let (complete, completion) = std::sync::mpsc::sync_channel(1);
        let handler = RcBlock::new(move |app: *mut NSRunningApplication, error: *mut NSError| {
            // AppKit keeps callback arguments alive throughout this invocation;
            // only plain Rust identity data leaves the callback.
            let result = if !error.is_null() || app.is_null() {
                Err(anyhow::anyhow!(
                    "LaunchServices refused the application launch"
                ))
            } else {
                unsafe { Identity::read(&*app) }
            };
            let _ = complete.try_send(result);
        });
        workspace.openApplicationAtURL_configuration_completionHandler(
            &url,
            &configuration,
            Some(&handler),
        );
        let identity = completion
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| anyhow::anyhow!("application launch timed out"))??;
        anyhow::ensure!(
            !existing.contains(&identity.pid) && identity.bundle == bundle,
            "LaunchServices reused or substituted an unowned application"
        );
        identity.check()?;
        Ok(Self {
            identity,
            windows: BTreeMap::new(),
            unavailable: BTreeSet::new(),
            retired: BTreeSet::new(),
            next_id: 1,
            input: None,
            first_window_deadline: Instant::now() + Duration::from_secs(15),
            seen_window: false,
        })
    }

    fn checked_window(
        &self,
        native_id: u64,
        generation: u32,
        focus: bool,
    ) -> anyhow::Result<(Window, Ax)> {
        let window = self
            .windows
            .get(&native_id)
            .ok_or_else(|| anyhow::anyhow!("unknown application surface"))?;
        anyhow::ensure!(
            window.surface.geometry_generation == generation,
            "stale surface geometry"
        );
        let element = validate_window(&self.identity, window, focus)?;
        Ok((window.clone(), element))
    }

    fn ax_not_ready(&mut self, error: anyhow::Error) -> anyhow::Result<Vec<NativeSurface>> {
        if !error
            .downcast_ref::<AxAttributeError>()
            .is_some_and(AxAttributeError::is_readiness)
        {
            return Err(error);
        }
        if !self.seen_window {
            if Instant::now() >= self.first_window_deadline {
                return Err(error
                    .context("application AX windows were not ready before the startup deadline"));
            }
            // No surface has been authorized yet, so there is nothing to capture
            // or inject into while LaunchServices/AX initialization catches up.
            return Ok(Vec::new());
        }
        if self.unavailable.is_empty() {
            tracing::warn!(%error, pid = self.identity.pid,
                "application AX temporarily unavailable; suspending authorized captures");
        }
        self.release_input();
        self.unavailable = self.windows.keys().copied().collect();
        Ok(self
            .windows
            .values()
            .map(|window| window.surface.clone())
            .collect())
    }
}

fn validate_window(identity: &Identity, window: &Window, focus: bool) -> anyhow::Result<Ax> {
    identity.check()?;
    let (pid, bounds) = live_window(window.cg_id)?;
    anyhow::ensure!(
        pid == identity.pid && window.bounds == bounds,
        "surface disappeared or moved"
    );
    let app = Ax::application(identity.pid)?;
    let mut candidates = app.window_tree()?;
    let mut matching = candidates
        .iter()
        .enumerate()
        .filter(|(_, (w, _))| w.bounds().is_ok_and(|b| b.matches(window.bounds)));
    let (index, (element, _)) = matching
        .next()
        .ok_or_else(|| anyhow::anyhow!("surface AX identity unavailable"))?;
    anyhow::ensure!(matching.next().is_none(), "ambiguous AX surface identity");
    anyhow::ensure!(
        element.owns_content(identity.pid)?,
        "surface content belongs to another process"
    );
    if focus {
        anyhow::ensure!(
            !element.boolean("AXMinimized").unwrap_or(false),
            "surface is minimized"
        );
        anyhow::ensure!(
            app.boolean("AXFrontmost").unwrap_or(false),
            "application lost focus"
        );
        // AXFocusedWindow commonly remains the document while its sheet owns
        // keyboard focus. Never send process-directed keys through that parent.
        let children = element.children("AXChildren")?;
        let mut modal_blocked = false;
        for child in children {
            modal_blocked |= child.string("AXRole")? == "AXSheet";
        }
        let mut modal_scope = BTreeSet::new();
        let mut ancestor = Some(index);
        while let Some(current) = ancestor {
            anyhow::ensure!(modal_scope.insert(current), "cyclic AX modal ancestry");
            ancestor = candidates[current].1;
        }
        for (candidate_index, (candidate, parent)) in candidates.iter().enumerate() {
            if parent.is_none()
                && !modal_scope.contains(&candidate_index)
                && candidate.boolean("AXModal")?
            {
                modal_blocked = true;
            }
        }
        let mut nearest = None;
        let mut focused_element = Some(app.attribute("AXFocusedUIElement")?);
        for depth in 0..32 {
            let Some(current) = focused_element else {
                break;
            };
            let role = current.string("AXRole")?;
            if role == "AXWindow" || role == "AXSheet" {
                nearest = Some(unsafe { CFEqual(current.0, element.0) });
                break;
            }
            anyhow::ensure!(depth < 31, "focused AX ancestry exceeds bound");
            focused_element = Some(current.attribute("AXParent")?);
        }
        anyhow::ensure!(
            focus_matches_surface(modal_blocked, nearest),
            "surface is modal-blocked or lost focus"
        );
    }
    Ok(candidates.swap_remove(index).0)
}

fn focus_matches_surface(modal_blocked: bool, nearest_surface_matches: Option<bool>) -> bool {
    !modal_blocked && nearest_surface_matches == Some(true)
}

fn live_window(id: u32) -> anyhow::Result<(i32, Bounds)> {
    let windows = Ax(unsafe { CGWindowListCopyWindowInfo(8, id) });
    anyhow::ensure!(
        !windows.0.is_null() && unsafe { CFArrayGetCount(windows.0) } == 1,
        "surface unavailable"
    );
    let info = unsafe { CFArrayGetValueAtIndex(windows.0, 0) };
    let owner = unsafe { CFDictionaryGetValue(info, kCGWindowOwnerPID) };
    let geometry = unsafe { CFDictionaryGetValue(info, kCGWindowBounds) };
    anyhow::ensure!(
        !owner.is_null() && !geometry.is_null(),
        "surface identity unavailable"
    );
    let mut pid = 0i32;
    let mut rectangle = [0f64; 4];
    anyhow::ensure!(
        unsafe {
            CFNumberGetValue(owner, 3, (&mut pid as *mut i32).cast())
                && CGRectMakeWithDictionaryRepresentation(geometry, rectangle.as_mut_ptr().cast())
        },
        "surface geometry unavailable"
    );
    Ok((
        pid,
        Bounds {
            x: rectangle[0],
            y: rectangle[1],
            width: rectangle[2],
            height: rectangle[3],
        },
    ))
}

impl ApplicationBackend for MacApplication {
    fn can_resume_capture(&self, native_id: u64, from: u32, to: u32) -> bool {
        self.windows.get(&native_id).is_some_and(|window| {
            window.resumable_from == Some(from)
                && window.surface.geometry_generation == to
                && !window.surface.minimized
                && !self.unavailable.contains(&native_id)
                && self.identity.check().is_ok()
                && live_window(window.cg_id)
                    .is_ok_and(|(pid, bounds)| pid == self.identity.pid && bounds == window.bounds)
        })
    }

    fn unavailable(&self) -> Vec<u64> {
        self.unavailable.iter().copied().collect()
    }

    fn snapshot(&mut self) -> anyhow::Result<Vec<NativeSurface>> {
        if self.seen_window
            && NSRunningApplication::runningApplicationWithProcessIdentifier(self.identity.pid)
                .is_none_or(|app| app.isTerminated())
        {
            self.release_input();
            self.windows.clear();
            self.unavailable.clear();
            return Ok(Vec::new());
        }
        self.identity.check()?;
        let content =
            SCShareableContent::get().map_err(|_| anyhow::anyhow!("surface enumeration failed"))?;
        let app = Ax::application(self.identity.pid)?;
        let tree = match app.window_tree() {
            Ok(tree) => tree,
            Err(error) => return self.ax_not_ready(error),
        };
        let mut current = BTreeMap::new();
        let previously_unavailable = std::mem::take(&mut self.unavailable);
        let mut ax_to_native = Vec::new();
        let mut parents = Vec::new();
        let mut rejected = BTreeSet::new();
        for window in content.windows().iter().filter(|w| {
            w.owning_application()
                .is_some_and(|app| app.process_id() == self.identity.pid)
        }) {
            let cg_id = window.window_id();
            if self.retired.contains(&cg_id) {
                continue;
            }
            let frame = window.frame();
            let bounds = Bounds {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            };
            if !bounds.valid() {
                continue;
            }
            let mut matching = tree
                .iter()
                .enumerate()
                .filter(|(_, (ax, _))| ax.bounds().is_ok_and(|b| b.matches(bounds)));
            let Some((ax_index, (element, parent))) = matching.next() else {
                continue;
            };
            if matching.next().is_some() {
                continue;
            }
            let owned = match element.owns_content(self.identity.pid) {
                Ok(owned) => owned,
                Err(error)
                    if error
                        .downcast_ref::<AxAttributeError>()
                        .is_some_and(AxAttributeError::is_readiness) =>
                {
                    // An unready candidate is not authority to suspend unrelated
                    // verified windows, nor to publish a new candidate.
                    if let Some(old) = self.windows.values().find(|old| old.cg_id == cg_id) {
                        self.unavailable.insert(old.surface.native_id);
                        current.insert(old.surface.native_id, old.clone());
                    }
                    tracing::debug!(%error, window = cg_id, "candidate AX content temporarily unavailable");
                    continue;
                }
                Err(error) => return Err(error),
            };
            if !owned {
                rejected.insert(cg_id);
                continue;
            }
            let previous = self.windows.values().find(|w| w.cg_id == cg_id);
            let native_id = if let Some(old) = previous {
                old.surface.native_id
            } else {
                let id = self.next_id;
                self.next_id = self
                    .next_id
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("native IDs exhausted"))?;
                id
            };
            let minimized = element.boolean("AXMinimized").unwrap_or(false);
            let resumable_from = previous
                .filter(|old| {
                    old.bounds == bounds
                        && old.surface.minimized == minimized
                        && previously_unavailable.contains(&old.surface.native_id)
                })
                .map(|old| old.surface.geometry_generation);
            let generation = match previous {
                Some(old)
                    if old.bounds == bounds
                        && old.surface.minimized == minimized
                        && resumable_from.is_none() =>
                {
                    old.surface.geometry_generation
                }
                Some(old) => old
                    .surface
                    .geometry_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("geometry exhausted"))?,
                None => 1,
            };
            // Encode at logical 1x, even-sized NV12 dimensions. The metadata
            // describes these exact dimensions, not an unrelated display scale.
            let width = (bounds.width.round() as u32).max(2) & !1;
            let height = (bounds.height.round() as u32).max(2) & !1;
            let surface = NativeSurface {
                native_id,
                parent: None,
                title: window.title().unwrap_or_default(),
                width,
                height,
                scale: 1.0,
                geometry_generation: generation,
                modal: parent.is_some() || element.boolean("AXModal").unwrap_or(false),
                minimized,
            };
            current.insert(
                native_id,
                Window {
                    cg_id,
                    resumable_from,
                    bounds,
                    surface,
                },
            );
            ax_to_native.push((ax_index, native_id));
            parents.push((native_id, *parent));
        }
        for (id, parent) in parents {
            if let Some(parent) = parent {
                let parent_id = ax_to_native
                    .iter()
                    .find(|(index, _)| *index == parent)
                    .map(|(_, id)| *id);
                // A sheet is authorized only with its proven parent present.
                if let Some(parent_id) = parent_id {
                    current.get_mut(&id).expect("new surface").surface.parent = Some(parent_id);
                } else {
                    current.remove(&id);
                }
            }
        }
        for old in self.windows.values() {
            if let std::collections::btree_map::Entry::Vacant(entry) =
                current.entry(old.surface.native_id)
            {
                if rejected.contains(&old.cg_id) {
                    self.retired.insert(old.cg_id);
                    continue;
                }
                // AX and SCK are independent snapshots. During a resize or
                // minimize they can disagree briefly; only a vanished native
                // handle retires the session ID. Input still checks live bounds.
                if live_window(old.cg_id).is_ok_and(|(pid, _)| pid == self.identity.pid) {
                    let mut preserved = old.clone();
                    let candidates: Vec<_> = tree
                        .iter()
                        .filter(|(ax, _)| ax.bounds().is_ok_and(|b| b.matches(old.bounds)))
                        .collect();
                    if candidates.len() == 1 {
                        if let Ok(minimized) = candidates[0].0.boolean("AXMinimized") {
                            if preserved.surface.minimized != minimized {
                                preserved.surface.geometry_generation = preserved
                                    .surface
                                    .geometry_generation
                                    .checked_add(1)
                                    .ok_or_else(|| anyhow::anyhow!("geometry exhausted"))?;
                            }
                            preserved.surface.minimized = minimized;
                        } else {
                            self.unavailable.insert(old.surface.native_id);
                        }
                    } else {
                        self.unavailable.insert(old.surface.native_id);
                    }
                    if !preserved.surface.minimized {
                        self.unavailable.insert(old.surface.native_id);
                    }
                    entry.insert(preserved);
                } else {
                    self.retired.insert(old.cg_id);
                }
            }
        }
        loop {
            let dependents: Vec<_> = current
                .values()
                .filter(|window| {
                    !self.unavailable.contains(&window.surface.native_id)
                        && window
                            .surface
                            .parent
                            .is_some_and(|parent| self.unavailable.contains(&parent))
                })
                .map(|window| window.surface.native_id)
                .collect();
            if dependents.is_empty() {
                break;
            }
            self.unavailable.extend(dependents);
        }
        self.windows = current;
        if !self.windows.is_empty() {
            self.seen_window = true;
        }
        anyhow::ensure!(
            self.seen_window || Instant::now() < self.first_window_deadline,
            "application produced no authorized windows"
        );
        if self
            .input
            .as_ref()
            .is_some_and(|(id, generation, _)| self.checked_window(*id, *generation, true).is_err())
        {
            self.release_input();
        }
        Ok(self.windows.values().map(|w| w.surface.clone()).collect())
    }

    fn video(&mut self, native_id: u64) -> anyhow::Result<Box<dyn VideoSource>> {
        let window = self
            .windows
            .get(&native_id)
            .ok_or_else(|| anyhow::anyhow!("unknown application surface"))?;
        anyhow::ensure!(!window.surface.minimized, "surface is minimized");
        validate_window(&self.identity, window, false)?;
        Ok(Box::new(capture::MacVideo::window(
            window.cg_id,
            self.identity.pid,
        )))
    }

    fn input(&mut self, native_id: u64, generation: u32, event: &InputEvent) -> anyhow::Result<()> {
        let result = (|| {
            let (window, _) = self.checked_window(native_id, generation, true)?;
            if self
                .input
                .as_ref()
                .is_none_or(|(id, revision, _)| *id != native_id || *revision != generation)
            {
                self.release_input();
                let identity = self.identity.clone();
                let alive = identity.clone();
                let injector = input::ApplicationInput::new(
                    identity.pid,
                    window.cg_id,
                    move || alive.check().is_ok(),
                    move || {
                        validate_window(&identity, &window, true)?;
                        let b = window.bounds;
                        Ok((b.x, b.y, b.width, b.height))
                    },
                )?;
                self.input = Some((native_id, generation, injector));
            }
            self.input
                .as_ref()
                .expect("scoped injector")
                .2
                .inject(event)
        })();
        if matches!(
            event.kind,
            ndp_proto::InputKind::MouseDown | ndp_proto::InputKind::MouseUp
        ) {
            match &result {
                Ok(()) => {
                    tracing::info!(native_id, generation, kind = ?event.kind, "scoped mouse event posted")
                }
                Err(error) => {
                    tracing::warn!(native_id, generation, kind = ?event.kind, %error, "scoped mouse event rejected")
                }
            }
        }
        if result.is_err() {
            self.release_input();
        }
        result
    }

    fn operate(&mut self, native_id: u64, command: &ApplicationMessage) -> anyhow::Result<()> {
        let (_, generation) = command
            .command_target()
            .ok_or_else(|| anyhow::anyhow!("not an application command"))?;
        self.release_input();
        let (_, element) = self.checked_window(native_id, generation, false)?;
        match command {
            ApplicationMessage::Focus { .. } => {
                if element.boolean("AXMinimized").unwrap_or(false) {
                    element.set_boolean("AXMinimized", false)?;
                }
                let app = NSRunningApplication::runningApplicationWithProcessIdentifier(
                    self.identity.pid,
                )
                .ok_or_else(|| anyhow::anyhow!("owned application exited"))?;
                #[allow(deprecated)]
                let activated = app
                    .activateWithOptions(NSApplicationActivationOptions::ActivateIgnoringOtherApps);
                anyhow::ensure!(activated, "application activation failed");
                element.perform("AXRaise")?;
                // Sheets need not expose AXMain. Raising the exact AX element
                // is the native focus operation; input verifies the result.
                let _ = element.set_boolean("AXMain", true);
            }
            ApplicationMessage::Resize { width, height, .. } => {
                anyhow::ensure!(
                    (64..=8192).contains(width) && (64..=8192).contains(height),
                    "resize exceeds native bounds"
                );
                element.set_size(f64::from(*width), f64::from(*height))?;
            }
            ApplicationMessage::Close { .. } => {
                element.attribute("AXCloseButton")?.perform("AXPress")?
            }
            ApplicationMessage::Minimize { .. } => element.set_boolean("AXMinimized", true)?,
            _ => anyhow::bail!("unsupported native operation"),
        }
        Ok(())
    }

    fn release_input(&mut self) {
        self.input = None;
    }
    fn stop(&mut self) {
        self.release_input();
        self.windows.clear();
    }
}

impl Drop for MacApplication {
    fn drop(&mut self) {
        self.stop();
    }
}

// AX handles remain on the calling native worker. Every Copy/Create result has
// one CF owner; bounds/booleans are type checked before calling typed CF APIs.
struct Ax(*const c_void);

impl Ax {
    fn process_id(&self) -> anyhow::Result<i32> {
        anyhow::ensure!(
            unsafe { CFGetTypeID(self.0) == AXUIElementGetTypeID() },
            "invalid AX element"
        );
        let mut pid = 0i32;
        anyhow::ensure!(
            unsafe { AXUIElementGetPid(self.0, &mut pid) } == 0 && pid > 0,
            "AX element process identity unavailable"
        );
        Ok(pid)
    }

    fn owns_content(&self, owner_pid: i32) -> anyhow::Result<bool> {
        let root_pid = self.process_id()?;
        if root_pid != owner_pid {
            return Ok(false);
        }
        let children = match self.children("AXChildren") {
            Ok(children) => children,
            Err(error)
                if error
                    .downcast_ref::<AxAttributeError>()
                    .is_some_and(|error| matches!(error.status, -25205 | -25212)) =>
            {
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        let mut content = Vec::new();
        for child in &children {
            let child_pid = child.process_id()?;
            let mut associated = false;
            for name in ["AXWindow", "AXTopLevelUIElement"] {
                match child.attribute(name) {
                    Ok(window) => associated |= unsafe { CFEqual(window.0, self.0) },
                    Err(error)
                        if error
                            .downcast_ref::<AxAttributeError>()
                            .is_some_and(|error| matches!(error.status, -25205 | -25212)) => {}
                    Err(error) => return Err(error),
                }
                // Eligibility requires one app-owned forward association, not
                // an exhaustive walk of every dynamically changing control.
                if child_pid == owner_pid && associated {
                    return Ok(true);
                }
            }
            content.push((child_pid, associated));
        }
        // AX remote-element proxies can report the app PID for system chrome.
        // Require a forward AXWindow/AXTopLevelUIElement association, not just
        // that proxy's PID/AXParent. Real sheets bind through TopLevelUIElement.
        let activation_owned = if children.is_empty() {
            match self.attribute("AXActivationPoint") {
                Ok(value) => {
                    let mut point = [0f64; 2];
                    let bounds = self.bounds()?;
                    let valid_point = unsafe {
                        CFGetTypeID(value.0) == AXValueGetTypeID()
                            && AXValueGetType(value.0) == 1
                            && AXValueGetValue(value.0, 1, point.as_mut_ptr().cast())
                    };
                    valid_point
                        && point[0].is_finite()
                        && point[1].is_finite()
                        && point[0] >= bounds.x
                        && point[0] < bounds.x + bounds.width
                        && point[1] >= bounds.y
                        && point[1] < bounds.y + bounds.height
                }
                Err(error)
                    if error
                        .downcast_ref::<AxAttributeError>()
                        .is_some_and(|error| error.status == -25205) =>
                {
                    false
                }
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        Ok(owns_surface_content(
            owner_pid,
            root_pid,
            &content,
            activation_owned,
        ))
    }

    fn application(pid: i32) -> anyhow::Result<Self> {
        let value = Self(unsafe { AXUIElementCreateApplication(pid) });
        anyhow::ensure!(!value.0.is_null(), "AX application unavailable");
        unsafe {
            AXUIElementSetMessagingTimeout(value.0, 0.2);
        }
        Ok(value)
    }

    fn attribute(&self, name: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            unsafe { CFGetTypeID(self.0) == AXUIElementGetTypeID() },
            "invalid AX element"
        );
        unsafe {
            AXUIElementSetMessagingTimeout(self.0, 0.2);
        }
        let key = cf_string(name)?;
        let mut value = std::ptr::null();
        let status = unsafe { AXUIElementCopyAttributeValue(self.0, key.0, &mut value) };
        if status != 0 || value.is_null() {
            return Err(AxAttributeError {
                attribute: name.into(),
                status: if status == 0 { -25212 } else { status },
            }
            .into());
        }
        Ok(Self(value))
    }

    fn children(&self, name: &str) -> anyhow::Result<Vec<Self>> {
        let array = self.attribute(name)?;
        anyhow::ensure!(
            unsafe { CFGetTypeID(array.0) == CFArrayGetTypeID() },
            "AX attribute is not an array"
        );
        let count = unsafe { CFArrayGetCount(array.0) };
        anyhow::ensure!((0..=256).contains(&count), "AX collection too large");
        let mut values = Vec::new();
        for i in 0..count {
            let value = unsafe { CFArrayGetValueAtIndex(array.0, i) };
            anyhow::ensure!(!value.is_null(), "invalid AX child");
            unsafe {
                CFRetain(value);
            }
            values.push(Self(value));
        }
        Ok(values)
    }

    fn window_tree(&self) -> anyhow::Result<Vec<(Self, Option<usize>)>> {
        let mut windows: Vec<_> = self
            .children("AXWindows")?
            .into_iter()
            .map(|w| (w, None))
            .collect();
        let mut index = 0;
        while index < windows.len() {
            anyhow::ensure!(windows.len() <= 64, "AX window hierarchy exceeds bound");
            let children = windows[index].0.children("AXChildren").unwrap_or_default();
            for child in children {
                if child.string("AXRole").is_ok_and(|role| role == "AXSheet") {
                    if let Some(existing) = windows
                        .iter()
                        .position(|(existing, _)| unsafe { CFEqual(existing.0, child.0) })
                    {
                        anyhow::ensure!(existing != index, "cyclic AX sheet relationship");
                        windows[existing].1 = Some(index);
                    } else {
                        windows.push((child, Some(index)));
                    }
                }
            }
            index += 1;
        }
        Ok(windows)
    }

    fn string(&self, name: &str) -> anyhow::Result<String> {
        let value = self.attribute(name)?;
        anyhow::ensure!(
            unsafe { CFGetTypeID(value.0) == CFStringGetTypeID() },
            "invalid AX string"
        );
        let mut bytes = [0i8; 2048];
        anyhow::ensure!(
            unsafe {
                CFStringGetCString(
                    value.0,
                    bytes.as_mut_ptr(),
                    bytes.len() as isize,
                    0x08000100,
                )
            },
            "AX string too long"
        );
        Ok(unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }

    fn boolean(&self, name: &str) -> anyhow::Result<bool> {
        let value = self.attribute(name)?;
        anyhow::ensure!(
            unsafe { CFGetTypeID(value.0) == CFBooleanGetTypeID() },
            "invalid AX boolean"
        );
        Ok(unsafe { CFBooleanGetValue(value.0) })
    }

    fn bounds(&self) -> anyhow::Result<Bounds> {
        let position = self.attribute("AXPosition")?;
        let size = self.attribute("AXSize")?;
        let mut origin = [0f64; 2];
        let mut dimensions = [0f64; 2];
        anyhow::ensure!(
            unsafe {
                CFGetTypeID(position.0) == AXValueGetTypeID()
                    && CFGetTypeID(size.0) == AXValueGetTypeID()
                    && AXValueGetType(position.0) == 1
                    && AXValueGetType(size.0) == 2
                    && AXValueGetValue(position.0, 1, origin.as_mut_ptr().cast())
                    && AXValueGetValue(size.0, 2, dimensions.as_mut_ptr().cast())
            },
            "invalid AX geometry"
        );
        Ok(Bounds {
            x: origin[0],
            y: origin[1],
            width: dimensions[0],
            height: dimensions[1],
        })
    }

    fn set_boolean(&self, name: &str, value: bool) -> anyhow::Result<()> {
        let key = cf_string(name)?;
        let status = unsafe {
            AXUIElementSetAttributeValue(
                self.0,
                key.0,
                if value {
                    kCFBooleanTrue
                } else {
                    kCFBooleanFalse
                },
            )
        };
        anyhow::ensure!(status == 0, "native window operation unavailable");
        Ok(())
    }

    fn set_size(&self, width: f64, height: f64) -> anyhow::Result<()> {
        let key = cf_string("AXSize")?;
        let size = [width, height];
        let value = Self(unsafe { AXValueCreate(2, size.as_ptr().cast()) });
        anyhow::ensure!(!value.0.is_null(), "invalid resize value");
        anyhow::ensure!(
            unsafe { AXUIElementSetAttributeValue(self.0, key.0, value.0) } == 0,
            "native resize unavailable"
        );
        Ok(())
    }

    fn perform(&self, action: &str) -> anyhow::Result<()> {
        let action = cf_string(action)?;
        anyhow::ensure!(
            unsafe { AXUIElementPerformAction(self.0, action.0) } == 0,
            "native window action unavailable"
        );
        Ok(())
    }
}

impl Drop for Ax {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
}

fn cf_string(value: &str) -> anyhow::Result<Ax> {
    let value = CString::new(value)?;
    let result =
        Ax(unsafe { CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), 0x08000100) });
    anyhow::ensure!(!result.0.is_null(), "CF string allocation failed");
    Ok(result)
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *const c_void;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementGetPid(element: *const c_void, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(element: *const c_void, seconds: f32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: *const c_void,
        name: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: *const c_void,
        name: *const c_void,
        value: *const c_void,
    ) -> i32;
    fn AXUIElementPerformAction(element: *const c_void, action: *const c_void) -> i32;
    fn AXValueGetType(value: *const c_void) -> u32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: *const c_void, kind: u32, out: *mut c_void) -> bool;
    fn AXValueCreate(kind: u32, value: *const c_void) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRetain(value: *const c_void) -> *const c_void;
    fn CFRelease(value: *const c_void);
    fn CFEqual(a: *const c_void, b: *const c_void) -> bool;
    fn CFGetTypeID(value: *const c_void) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(value: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(value: *const c_void, index: isize) -> *const c_void;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: *const c_void) -> bool;
    static kCFBooleanTrue: *const c_void;
    static kCFBooleanFalse: *const c_void;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        value: *const i8,
        encoding: u32,
    ) -> *const c_void;
    fn CFStringGetCString(
        value: *const c_void,
        buffer: *mut i8,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, kind: isize, value: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(options: u32, window: u32) -> *const c_void;
    fn CGRectMakeWithDictionaryRepresentation(
        dictionary: *const c_void,
        rectangle: *mut c_void,
    ) -> bool;
    static kCGWindowOwnerPID: *const c_void;
    static kCGWindowBounds: *const c_void;
}

#[cfg(test)]
mod tests {
    #[test]
    fn modal_sheet_rejects_parent_focus_and_requires_the_nearest_surface() {
        // AXFocusedWindow may still report the document in all these cases;
        // it must never override the nearest focused sheet or missing proof.
        assert!(!super::focus_matches_surface(true, Some(true)));
        assert!(!super::focus_matches_surface(true, Some(false)));
        assert!(!super::focus_matches_surface(false, Some(false)));
        assert!(!super::focus_matches_surface(false, None));
        assert!(super::focus_matches_surface(false, Some(true)));
    }

    use super::*;
    #[test]
    fn geometry_match_rejects_nonfinite_and_moved_windows() {
        let b = Bounds {
            x: 12.0,
            y: 40.0,
            width: 640.0,
            height: 480.0,
        };
        assert!(b.valid() && b.matches(b));
        assert!(!b.matches(Bounds { x: 20.0, ..b }));
        assert!(!Bounds {
            width: f64::NAN,
            ..b
        }
        .valid());
    }

    #[test]
    fn cross_process_capture_controls_are_not_application_surfaces() {
        // Observed sharing-control shell: root PID matched the fixture, while
        // its only AXButton belonged to ThemeWidgetControlViewService.
        assert!(!owns_surface_content(
            31193,
            31193,
            &[(31205, false)],
            false
        ));
        assert!(!owns_surface_content(
            33140,
            33140,
            &[(33140, false)],
            false
        ));
        assert!(!owns_surface_content(31193, 31205, &[], true));
        // Small dialogs, tool windows and custom-drawn canvases are not
        // excluded by dimensions, title, modality or absence of AX children.
        assert!(owns_surface_content(31193, 31193, &[(31193, true)], false));
        assert!(owns_surface_content(31193, 31193, &[], true));
        assert!(!owns_surface_content(31193, 31193, &[], false));
        assert!(owns_surface_content(
            31193,
            31193,
            &[(31193, true), (31205, false)],
            false
        ));
    }

    #[test]
    fn same_pid_proxy_needs_forward_window_ownership_and_real_sheets_remain_valid() {
        // Reproduced with native3: AXUIElementGetPid masks the remote provider,
        // but both forward window references remain absent on capture chrome.
        assert!(!owns_surface_content(33140, 33140, &[(33140, false)], true));
        // Real sheet buttons reference the parent in AXWindow and the sheet
        // itself in AXTopLevelUIElement; either direct association is accepted.
        assert!(owns_surface_content(33140, 33140, &[(33140, true)], false));
    }

    #[test]
    fn ax_readiness_errors_are_distinct_from_unsupported_and_denied_attributes() {
        for status in [-25202, -25204, -25212] {
            assert!(AxAttributeError {
                attribute: "AXWindows".into(),
                status
            }
            .is_readiness());
        }
        for status in [-25200, -25201, -25205, -25208, -25211] {
            assert!(!AxAttributeError {
                attribute: "AXWindows".into(),
                status
            }
            .is_readiness());
        }
    }

    #[test]
    fn ax_startup_wait_is_bounded_and_busy_owned_windows_become_unavailable() {
        let error = || {
            anyhow::Error::new(AxAttributeError {
                attribute: "AXWindows".into(),
                status: -25212,
            })
        };
        let mut app = MacApplication {
            identity: Identity {
                pid: -1,
                launched: 0.0,
                bundle: PathBuf::from("/fixture.app"),
            },
            windows: BTreeMap::new(),
            unavailable: BTreeSet::new(),
            retired: BTreeSet::new(),
            next_id: 1,
            input: None,
            first_window_deadline: Instant::now() + Duration::from_secs(15),
            seen_window: false,
        };
        assert!(app.ax_not_ready(error()).unwrap().is_empty());
        assert!(
            app.windows.is_empty(),
            "startup must not invent an authorized surface"
        );
        assert!(app
            .ax_not_ready(
                AxAttributeError {
                    attribute: "AXWindows".into(),
                    status: -25205,
                }
                .into()
            )
            .is_err());
        app.first_window_deadline = Instant::now() - Duration::from_secs(1);
        assert!(app.ax_not_ready(error()).is_err());
        app.seen_window = true;
        app.windows.insert(
            42,
            Window {
                cg_id: 100,
                resumable_from: None,
                bounds: Bounds {
                    x: 10.0,
                    y: 20.0,
                    width: 640.0,
                    height: 480.0,
                },
                surface: NativeSurface {
                    native_id: 42,
                    parent: None,
                    title: "Owned".into(),
                    width: 640,
                    height: 480,
                    scale: 1.0,
                    geometry_generation: 7,
                    modal: false,
                    minimized: false,
                },
            },
        );
        let pending = app.ax_not_ready(error()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].native_id, pending[0].geometry_generation),
            (42, 7)
        );
        assert_eq!(app.unavailable(), vec![42]);
        assert!(
            app.retired.is_empty(),
            "temporary AX failure must not retire an owned ID"
        );
    }

    #[test]
    #[ignore = "launches explicit task fixture, opens its save sheet and injects scoped Escape"]
    fn native_owned_modal_focus_probe() {
        use ndp_proto::{InputKind, KeyCode, Modifiers};
        let path = PathBuf::from(std::env::var("NEBULA_APP_PROBE_PATH").unwrap());
        assert_eq!(path.file_name().unwrap(), "NebulaSeamlessFixture.app");
        let status_directory = std::env::var_os("NEBULA_APP_PROBE_STATUS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| path.parent().unwrap().join("status"));
        let mut app = MacApplication::launch(&ApplicationLaunch {
            launch_path: path.to_string_lossy().into_owned(),
            launch_args: vec!["--dirty-first".into()],
            working_dir: None,
        })
        .unwrap();
        eprintln!("modal regression fixture PID {}", app.identity.pid);
        let status_path = status_directory.join(format!(
            "nebula-seamless-fixture-status-{}.json",
            app.identity.pid
        ));
        let fixture_windows = || {
            let status: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
            status["windows"].as_array().unwrap().clone()
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        let parent = loop {
            if let Some(parent) = app
                .snapshot()
                .unwrap()
                .into_iter()
                .find(|surface| surface.title == "Nebula acceptance document 1")
            {
                break parent;
            }
            assert!(Instant::now() < deadline, "fixture document unavailable");
            std::thread::sleep(Duration::from_millis(25));
        };
        app.operate(
            parent.native_id,
            &ApplicationMessage::Close {
                surface_id: 0,
                geometry_generation: parent.geometry_generation,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let sheet = loop {
            if let Some(sheet) = app
                .snapshot()
                .unwrap()
                .into_iter()
                .find(|surface| surface.modal && surface.parent == Some(parent.native_id))
            {
                if app
                    .checked_window(sheet.native_id, sheet.geometry_generation, true)
                    .is_ok()
                {
                    break sheet;
                }
            }
            assert!(
                Instant::now() < deadline,
                "save sheet did not acquire scoped native focus"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        for key in [KeyCode::ENTER, KeyCode::ESCAPE] {
            for kind in [InputKind::KeyDown, InputKind::KeyUp] {
                let error = app
                    .input(
                        parent.native_id,
                        parent.geometry_generation,
                        &InputEvent::key(kind, key, Modifiers::NONE),
                    )
                    .unwrap_err();
                assert!(error.to_string().contains("modal-blocked"), "{error}");
            }
        }
        assert!(app
            .snapshot()
            .unwrap()
            .iter()
            .any(|surface| surface.native_id == sheet.native_id));
        // Escape activates the fixture's Cancel button. The sheet, not
        // its blocked document, is the sole authorized keyboard destination.
        app.input(
            sheet.native_id,
            sheet.geometry_generation,
            &InputEvent::key(InputKind::KeyDown, KeyCode::ESCAPE, Modifiers::NONE),
        )
        .unwrap();
        // The sheet may disappear before key-up; rejection releases held input.
        let _ = app.input(
            sheet.native_id,
            sheet.geometry_generation,
            &InputEvent::key(InputKind::KeyUp, KeyCode::ESCAPE, Modifiers::NONE),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current = app.snapshot().unwrap();
            assert!(current
                .iter()
                .any(|surface| surface.native_id == parent.native_id));
            if current
                .iter()
                .all(|surface| surface.native_id != sheet.native_id)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "scoped sheet Escape did not cancel the save dialog"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        let windows = fixture_windows();
        let dirty = windows.iter().find(|window| window["id"] == 1).unwrap();
        assert_eq!(dirty["dirty"], true, "Cancel must preserve unsaved changes");
        assert_eq!(dirty["cancelled_closes"], 1);
        app.operate(
            parent.native_id,
            &ApplicationMessage::Close {
                surface_id: 0,
                geometry_generation: parent.geometry_generation,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let discarded_sheet = loop {
            if let Some(sheet) = app
                .snapshot()
                .unwrap()
                .into_iter()
                .find(|surface| surface.modal && surface.parent == Some(parent.native_id))
            {
                if let Ok((_, element)) =
                    app.checked_window(sheet.native_id, sheet.geometry_generation, true)
                {
                    let mut pending = vec![element];
                    let mut buttons = Vec::new();
                    let mut visited = 0;
                    while let Some(element) = pending.pop() {
                        visited += 1;
                        assert!(visited <= 256, "fixture sheet exceeds AX traversal bound");
                        if element
                            .string("AXRole")
                            .is_ok_and(|role| role == "AXButton")
                            && element
                                .string("AXTitle")
                                .is_ok_and(|title| title == "Discard")
                        {
                            buttons.push(element);
                        } else {
                            pending.extend(element.children("AXChildren").unwrap_or_default());
                        }
                    }
                    assert_eq!(
                        buttons.len(),
                        1,
                        "expected one owned fixture Discard button"
                    );
                    buttons[0].perform("AXPress").unwrap();
                    break sheet.native_id;
                }
            }
            assert!(
                Instant::now() < deadline,
                "dirty document did not reopen its save sheet"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let remaining = loop {
            let current = app.snapshot().unwrap();
            let windows = fixture_windows();
            if current.iter().all(|surface| {
                surface.native_id != parent.native_id && surface.native_id != discarded_sheet
            }) && windows.iter().all(|window| window["id"] != 1)
            {
                assert_eq!(
                    windows.len(),
                    1,
                    "Discard must preserve the sibling document"
                );
                assert_eq!(windows[0]["id"], 2);
                assert_eq!(current.len(), 1);
                break current[0].clone();
            }
            assert!(
                Instant::now() < deadline,
                "Discard must close its document without another Close"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        app.operate(
            remaining.native_id,
            &ApplicationMessage::Close {
                surface_id: 0,
                geometry_generation: remaining.geometry_generation,
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fixture_windows().is_empty() {
            assert!(
                Instant::now() < deadline,
                "final windowWillClose did not update fixture state"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        app.stop();
    }

    #[test]
    #[ignore = "read-only AX inspection of explicit NEBULA_APP_PROBE_PID; never launches an app"]
    fn native_existing_surface_eligibility_probe() {
        let pid: i32 = std::env::var("NEBULA_APP_PROBE_PID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid > 0 && input::trusted());
        let app = Ax::application(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let tree = loop {
            match app.window_tree() {
                Ok(tree) => break tree,
                Err(error)
                    if error
                        .downcast_ref::<AxAttributeError>()
                        .is_some_and(AxAttributeError::is_readiness)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("existing process AX inspection failed: {error}"),
            }
        };
        let mut rejected = 0;
        let mut admitted = 0;
        let mut sheets = 0;
        for (element, _) in tree {
            let accepted = element.owns_content(pid).unwrap();
            let role = element.string("AXRole").unwrap();
            eprintln!(
                "AX eligibility: role={role}, bounds={:?}, accepted={accepted}",
                element.bounds().unwrap()
            );
            if accepted {
                admitted += 1;
                if role == "AXSheet" {
                    sheets += 1;
                }
            } else {
                rejected += 1;
            }
        }
        assert!(admitted > 0, "no application surface was preserved");
        if std::env::var_os("NEBULA_APP_PROBE_EXPECT_REJECTED").is_some() {
            assert!(rejected > 0, "no hosted capture chrome was rejected");
        }
        if std::env::var_os("NEBULA_APP_PROBE_EXPECT_EXISTING_SHEET").is_some() {
            assert!(sheets > 0, "the real save sheet must remain eligible");
        }
    }

    #[tokio::test]
    #[ignore = "captures explicit NEBULA_APP_PROBE_PID windows; never launches or injects input"]
    async fn native_existing_capture_restart_probe() {
        use crate::media::VideoSource;
        use shiguredo_video_toolbox::{Decoder, DecoderCodec, DecoderConfig, PixelFormat};
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        let pid: i32 = std::env::var("NEBULA_APP_PROBE_PID")
            .unwrap()
            .parse()
            .unwrap();
        assert!(pid > 0 && input::trusted() && capture::screen_capture_allowed());
        let app = Ax::application(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let tree = loop {
            match app.window_tree() {
                Ok(tree) => break tree,
                Err(error)
                    if error
                        .downcast_ref::<AxAttributeError>()
                        .is_some_and(AxAttributeError::is_readiness)
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("existing capture probe AX inspection failed: {error}"),
            }
        };
        let content = SCShareableContent::get().unwrap();
        let mut windows = Vec::new();
        for window in content.windows().iter().filter(|window| {
            window
                .owning_application()
                .is_some_and(|owner| owner.process_id() == pid)
        }) {
            let frame = window.frame();
            let bounds = Bounds {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            };
            let candidates: Vec<_> = tree
                .iter()
                .filter(|(ax, _)| ax.bounds().is_ok_and(|b| b.matches(bounds)))
                .collect();
            if bounds.valid() && candidates.len() == 1 && candidates[0].0.owns_content(pid).unwrap()
            {
                let (owner, live_bounds) = live_window(window.window_id()).unwrap();
                assert_eq!(owner, pid);
                assert_eq!(live_bounds, bounds);
                windows.push((window.clone(), bounds));
            }
        }
        assert_eq!(
            windows.len(),
            2,
            "probe requires exactly two authorized fixture windows"
        );
        for generation in 1..=3 {
            let mut captures = Vec::new();
            for (window, bounds) in &windows {
                let id = window.window_id();
                let (owner, live_bounds) = live_window(id).unwrap();
                assert_eq!(owner, pid);
                assert_eq!(live_bounds, *bounds);
                let mut source = capture::MacVideo::window(id, pid);
                let (sink, frames) = tokio::sync::mpsc::channel(3);
                source
                    .start(
                        crate::media::VideoConfig {
                            width: bounds.width.round() as u32 & !1,
                            height: bounds.height.round() as u32 & !1,
                            fps: 30,
                            bitrate: 2_000_000,
                        },
                        sink.into(),
                    )
                    .unwrap();
                captures.push((id, source, frames, 0, None::<Decoder>));
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                for _ in 0..8 {
                    for (id, _, frames, count, decoder) in &mut captures {
                        let frame = frames.recv().await.expect("native encoder closed");
                        assert!(!frame.data.is_empty());
                        if decoder.is_none() {
                            assert!(frame.keyframe);
                            let mut bytes = frame.data.as_slice();
                            let mut units = Vec::new();
                            while !bytes.is_empty() {
                                let size = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
                                units.push(&bytes[4..4 + size]);
                                bytes = &bytes[4 + size..];
                            }
                            *decoder = Some(Decoder::new(DecoderConfig {
                                codec: DecoderCodec::H264 {
                                    sps: units.iter().find(|unit| unit[0] & 31 == 7).unwrap(),
                                    pps: units.iter().find(|unit| unit[0] & 31 == 8).unwrap(),
                                    nalu_len_bytes: 4,
                                },
                                pixel_format: PixelFormat::Nv12,
                            }).unwrap());
                        }
                        if decoder.as_mut().unwrap().decode(&frame.data).unwrap().is_some() {
                            *count += 1;
                        }
                        if *count == 1 {
                            eprintln!("native restart generation={generation}, window={id}: first decoded picture, keyframe={}", frame.keyframe);
                        }
                    }
                }
            }).await.expect("native restart did not restore two steady video streams");
            for (id, source, _, count, _) in &mut captures {
                assert!(
                    *count >= 5,
                    "each restarted stream must decode at least five pictures"
                );
                eprintln!("native restart generation={generation}, window={id}: {count} decoded pictures; stopping");
                source.stop();
            }
        }
    }

    #[test]
    #[ignore = "requires existing native grants; accepts NEBULA_APPLICATION_PROBE_BUNDLE fixture"]
    fn native_owned_launch_window_capture_probe() {
        let minimum = std::env::var("NEBULA_APPLICATION_PROBE_MIN_SURFACES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);
        assert!((1..=32).contains(&minimum));
        let input_probe = std::env::var_os("NEBULA_APPLICATION_PROBE_INPUT").is_some();
        let save_probe = std::env::var_os("NEBULA_APPLICATION_PROBE_EXPECT_SAVE_SHEET").is_some();
        let launch = ApplicationLaunch {
            launch_path: std::env::var("NEBULA_APP_PROBE_PATH")
                .or_else(|_| std::env::var("NEBULA_APPLICATION_PROBE_BUNDLE"))
                .unwrap_or_else(|_| "/System/Applications/TextEdit.app".into()),
            launch_args: vec![],
            working_dir: None,
        };
        assert!(
            capability().is_supported(),
            "native prerequisites are unavailable; probe never changes grants"
        );
        let existing: BTreeSet<_> = SCShareableContent::get()
            .unwrap()
            .windows()
            .iter()
            .map(|window| window.window_id())
            .collect();
        let mut app = MacApplication::launch(&launch).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let surfaces = loop {
            let surfaces = app.snapshot().unwrap();
            if surfaces.len() >= minimum {
                break surfaces;
            }
            assert!(Instant::now() < deadline, "no owned application surface");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(
            app.windows
                .values()
                .all(|window| !existing.contains(&window.cg_id)),
            "the probe must never attach to a pre-existing window"
        );
        for surface in &surfaces {
            let mut video = app.video(surface.native_id).unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::channel(3);
            video
                .start(
                    crate::media::VideoConfig {
                        width: surface.width,
                        height: surface.height,
                        ..Default::default()
                    },
                    tx.into(),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let frame = loop {
                if let Ok(frame) = rx.try_recv() {
                    break frame;
                }
                assert!(Instant::now() < deadline, "no isolated native window frame");
                std::thread::sleep(Duration::from_millis(25));
            };
            assert!(frame.keyframe && !frame.data.is_empty());
            video.stop();
            let event = InputEvent::mouse_move(0.5, 0.5, ndp_proto::Modifiers::NONE);
            assert!(app
                .input(u64::MAX, surface.geometry_generation, &event)
                .is_err());
            assert!(app
                .input(surface.native_id, surface.geometry_generation + 1, &event)
                .is_err());
            if input_probe {
                app.operate(
                    surface.native_id,
                    &ApplicationMessage::Focus {
                        surface_id: 0,
                        geometry_generation: surface.geometry_generation,
                    },
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if app
                        .input(surface.native_id, surface.geometry_generation, &event)
                        .is_ok()
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "owned-surface focus/input probe failed"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
        if save_probe {
            let parent = &surfaces[0];
            app.operate(
                parent.native_id,
                &ApplicationMessage::Close {
                    surface_id: 0,
                    geometry_generation: parent.geometry_generation,
                },
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let current = app.snapshot().unwrap();
                if current
                    .iter()
                    .any(|surface| surface.modal && surface.parent == Some(parent.native_id))
                {
                    assert!(
                        current
                            .iter()
                            .any(|surface| surface.native_id == parent.native_id),
                        "save confirmation must not prematurely retire its document"
                    );
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture did not expose an authorized save sheet"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        app.stop();
    }
}
