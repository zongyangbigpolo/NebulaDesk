//! Input injection on macOS.
//!
//! Everything here goes through `CGEvent` posted to the HID event tap, which
//! is the same path a physical device takes: events land in the window server
//! before any application sees them, so they work in every app rather than
//! only in ones that cooperate.
//!
//! This requires the Accessibility permission. macOS grants it per signed
//! binary and then silently drops events without it, giving no error at the
//! call site — a session where the picture moves but the mouse does not is
//! otherwise a very confusing thing to debug.
//!
//! Keyboard injection is physical/remote-layout based. Unicode metadata and
//! client-side IME composition are not injected as committed text.

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use ndp_proto::{InputEvent, InputKind, KeyCode, Modifiers, MouseButton};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::{NSPoint, NSProcessInfo};
use std::time::{Duration, Instant};

use crate::media::InputInjector;

/// Injects input into the local desktop.
///
/// CoreGraphics event objects are not safe to move between threads, and the
/// session that feeds this injector is a tokio task that can be scheduled
/// anywhere. So every CoreGraphics call is confined to one thread of its own
/// and reached by channel. That also serialises injection, which is required
/// for correctness rather than merely tidy: two events posted concurrently
/// can arrive at the window server out of order, turning a click-drag into a
/// click somewhere else.
pub struct MacInput {
    events: Option<std::sync::mpsc::Sender<InputEvent>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl MacInput {
    /// Start the injection thread.
    ///
    /// Fails when Accessibility has not been granted. Checking is worth the
    /// call: without it the injector builds, the session runs, every event
    /// is posted, the window server discards all of them, and nothing
    /// anywhere says why the remote pointer never moves.
    pub fn new() -> anyhow::Result<Self> {
        anyhow::ensure!(trusted(), ACCESSIBILITY);

        let (events, inbox) = std::sync::mpsc::channel::<InputEvent>();
        let (ready, started) = std::sync::mpsc::channel::<anyhow::Result<()>>();

        let worker = std::thread::Builder::new()
            .name("nebula-input".into())
            .spawn(move || {
                let mut desktop = match Desktop::new() {
                    Ok(desktop) => {
                        let _ = ready.send(Ok(()));
                        desktop
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                // Ends when the sender is dropped, which is what releases
                // whatever the session left held.
                while let Ok(event) = inbox.recv() {
                    if let Err(error) = desktop.apply(&event) {
                        tracing::debug!(%error, "could not inject an event");
                    }
                }
            })?;

        started
            .recv()
            .map_err(|_| anyhow::anyhow!("the input thread died before it started"))??;

        Ok(Self {
            events: Some(events),
            worker: Some(worker),
        })
    }
}

impl Drop for MacInput {
    fn drop(&mut self) {
        drop(self.events.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl InputInjector for MacInput {
    fn inject(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        self.events
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the input thread has shut down"))?
            .send(*event)
            .map_err(|_| anyhow::anyhow!("the input thread has shut down"))
    }
}

type ScopedBounds = (f64, f64, f64, f64);

/// Synchronous, process-directed input, revalidated on the posting thread.
pub(crate) struct ApplicationInput {
    events: Option<
        std::sync::mpsc::SyncSender<(InputEvent, std::sync::mpsc::Sender<anyhow::Result<()>>)>,
    >,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ApplicationInput {
    pub(crate) fn new(
        pid: i32,
        window: u32,
        alive: impl Fn() -> bool + Send + 'static,
        validate: impl Fn() -> anyhow::Result<ScopedBounds> + Send + 'static,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(trusted(), ACCESSIBILITY);
        anyhow::ensure!(
            pid > 0 && window != 0,
            "invalid window-directed input target"
        );
        let (events, inbox) = std::sync::mpsc::sync_channel::<(
            InputEvent,
            std::sync::mpsc::Sender<anyhow::Result<()>>,
        )>(8);
        let (ready, started) = std::sync::mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("nebula-app-input".into())
            .spawn(move || {
                let mut desktop = match Desktop::new() {
                    Ok(mut desktop) => {
                        desktop.target_pid = Some(pid);
                        desktop.target_window = Some(window);
                        desktop.target_alive = Some(Box::new(alive));
                        let _ = ready.send(Ok(()));
                        desktop
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                while let Ok((event, reply)) = inbox.recv() {
                    let result = validate().and_then(|bounds| {
                        desktop.bounds = bounds;
                        desktop.apply(&event)
                    });
                    if result.is_err() {
                        desktop.release_all();
                    }
                    let _ = reply.send(result);
                }
            })?;
        started
            .recv()
            .map_err(|_| anyhow::anyhow!("scoped input startup failed"))??;
        Ok(Self {
            events: Some(events),
            worker: Some(worker),
        })
    }

    pub(crate) fn inject(&self, event: &InputEvent) -> anyhow::Result<()> {
        let (reply, result) = std::sync::mpsc::channel();
        self.events
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("scoped input stopped"))?
            .send((*event, reply))
            .map_err(|_| anyhow::anyhow!("scoped input stopped"))?;
        result
            .recv_timeout(Duration::from_secs(2))
            .map_err(|_| anyhow::anyhow!("scoped input validation timed out"))?
    }
}

impl Drop for ApplicationInput {
    fn drop(&mut self) {
        drop(self.events.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
/// The CoreGraphics state, owned by the injection thread and never leaving it.
struct Desktop {
    source: CGEventSource,
    target_pid: Option<i32>,
    target_window: Option<u32>,
    target_alive: Option<Box<dyn Fn() -> bool + Send>>,
    /// The desktop area input is mapped onto, in global display space.
    bounds: (f64, f64, f64, f64),
    /// Where the pointer was last put, so a button event with no preceding
    /// move still lands somewhere sensible.
    last: CGPoint,
    /// Which buttons this session believes are down, so they can be released
    /// if it ends mid-drag.
    held: Vec<MouseButton>,
    keys: HeldKeys,
    clicks: Clicks,
    clock: Instant,
    wheel: [f64; 2],
}

#[derive(Default)]
struct HeldKeys(Vec<u16>);

impl HeldKeys {
    fn update(&mut self, code: u16, down: bool) -> bool {
        let repeated = self.0.contains(&code);
        if down && !repeated {
            self.0.push(code);
        } else if !down {
            self.0.retain(|held| *held != code);
        }
        down && repeated
    }

    fn modifiers(&self, code: u16, mut modifiers: Modifiers) -> Modifiers {
        // Winit can deliver the physical transition before ModifiersChanged.
        for (left, right, flag) in [
            (0x38, 0x3c, Modifiers::SHIFT),
            (0x3b, 0x3e, Modifiers::CONTROL),
            (0x3a, 0x3d, Modifiers::ALT),
            (0x37, 0x36, Modifiers::META),
        ] {
            if code == left || code == right {
                modifiers.0 &= !flag.0;
                if self.0.contains(&left) || self.0.contains(&right) {
                    modifiers = modifiers.union(flag);
                }
            }
        }
        modifiers
    }

    fn release_all(&mut self) -> Vec<u16> {
        std::mem::take(&mut self.0)
    }
}

struct Click {
    button: MouseButton,
    point: CGPoint,
    time: Duration,
    count: i64,
    released: bool,
}

struct Clicks {
    interval: Duration,
    last: Option<Click>,
    pressed: [i64; 5],
}

impl Clicks {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            pressed: [0; 5],
        }
    }

    fn moved(&mut self, point: CGPoint) {
        if self.last.as_ref().is_some_and(|last| {
            (point.x - last.point.x).powi(2) + (point.y - last.point.y).powi(2) > 16.0
        }) {
            self.last = None;
        }
    }

    fn update(&mut self, button: MouseButton, down: bool, point: CGPoint, now: Duration) -> i64 {
        let Some(index) = button_number(button).map(|number| number as usize) else {
            return 0;
        };
        self.moved(point);
        if !down {
            if let Some(last) = self.last.as_mut().filter(|last| last.button == button) {
                last.released = true;
            }
            return std::mem::take(&mut self.pressed[index]).max(1);
        }
        let count = self
            .last
            .as_ref()
            .filter(|last| {
                last.button == button
                    && last.released
                    && now
                        .checked_sub(last.time)
                        .is_some_and(|elapsed| elapsed <= self.interval)
            })
            .map_or(1, |last| last.count.saturating_add(1));
        self.last = Some(Click {
            button,
            point,
            time: now,
            count,
            released: false,
        });
        self.pressed[index] = count;
        count
    }
}

impl Desktop {
    fn new() -> anyhow::Result<Self> {
        // `HIDSystemState` makes injected events indistinguishable from real
        // ones to applications, which is what makes modifier-aware apps
        // behave. A private state would be visible as synthetic.
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| anyhow::anyhow!("could not create a CoreGraphics event source"))?;

        let frame = CGDisplay::main().bounds();
        Ok(Self {
            source,
            target_pid: None,
            target_window: None,
            target_alive: None,
            bounds: (
                frame.origin.x,
                frame.origin.y,
                frame.size.width,
                frame.size.height,
            ),
            last: CGPoint::new(
                frame.origin.x + frame.size.width / 2.0,
                frame.origin.y + frame.size.height / 2.0,
            ),
            held: Vec::new(),
            keys: HeldKeys::default(),
            clicks: Clicks::new(double_click_interval()),
            clock: Instant::now(),
            wheel: [0.0; 2],
        })
    }

    /// Turn a normalised position into a point on the target display.
    ///
    /// Clamped rather than rejected: a client whose window is slightly larger
    /// than the stream produces values just outside the range, and a pointer
    /// that sticks to the edge is what a user expects.
    fn point(&self, x: f32, y: f32) -> CGPoint {
        let (ox, oy, w, h) = self.bounds;
        CGPoint::new(
            ox + f64::from(x.clamp(0.0, 1.0)) * w,
            oy + f64::from(y.clamp(0.0, 1.0)) * h,
        )
    }

    fn release_buttons(&mut self, modifiers: Modifiers) {
        let held = std::mem::take(&mut self.held);
        for button in held {
            let Some(cg_button) = cg_button(button) else {
                continue;
            };
            let up = mouse_kind(button, false);
            if let Ok(event) =
                CGEvent::new_mouse_event(self.source.clone(), up, self.last, cg_button)
            {
                set_button_number(&event, button);
                apply_modifiers(&event, modifiers);
                event.set_integer_value_field(
                    EventField::MOUSE_EVENT_CLICK_STATE,
                    self.clicks
                        .update(button, false, self.last, self.clock.elapsed()),
                );
                self.post(&event);
            }
        }
        self.clicks.last = None;
        self.clicks.pressed = [0; 5];
    }

    /// Release physical keys as well as buttons when the transport disappears.
    fn release_all(&mut self) {
        self.release_buttons(Modifiers::NONE);
        for code in self.keys.release_all().into_iter().rev() {
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), code, false) {
                apply_modifiers(&event, Modifiers::NONE);
                self.post(&event);
            }
        }
    }

    fn post(&self, event: &CGEvent) -> bool {
        if self.target_alive.as_ref().is_some_and(|alive| !alive()) {
            return false;
        }
        if let Some(pid) = self.target_pid {
            if matches!(
                event.get_type(),
                CGEventType::MouseMoved
                    | CGEventType::LeftMouseDown
                    | CGEventType::LeftMouseUp
                    | CGEventType::RightMouseDown
                    | CGEventType::RightMouseUp
                    | CGEventType::OtherMouseDown
                    | CGEventType::OtherMouseUp
                    | CGEventType::LeftMouseDragged
                    | CGEventType::RightMouseDragged
                    | CGEventType::OtherMouseDragged
            ) {
                let Some(window) = self.target_window else {
                    return false;
                };
                return post_window_mouse(event, pid, window, self.bounds);
            }
            event.post_to_pid(pid);
        } else {
            event.post(CGEventTapLocation::HID);
        }
        true
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        // A session that ends mid-drag must not leave a button stuck down;
        // the next person to sit at this machine would find a desktop that
        // behaves as though possessed.
        self.release_all();
    }
}

impl Desktop {
    fn apply(&mut self, event: &InputEvent) -> anyhow::Result<()> {
        tracing::trace!(
            target: "nebula_agent::input_trace",
            kind = ?event.kind,
            x = event.x,
            y = event.y,
            hid = event.key.0,
            button = ?event.button,
            modifiers = event.modifiers.0,
            "applying input"
        );
        match event.kind {
            InputKind::MouseMove | InputKind::MouseDrag => {
                let point = self.point(event.x, event.y);
                self.last = point;
                self.clicks.moved(point);
                // Also tolerate older clients that label a held-button move MouseMove.
                let dragging = self.held.first().copied().unwrap_or(MouseButton::None);
                let (kind, button) = match cg_button(dragging) {
                    Some(CGMouseButton::Left) => {
                        (CGEventType::LeftMouseDragged, CGMouseButton::Left)
                    }
                    Some(CGMouseButton::Right) => {
                        (CGEventType::RightMouseDragged, CGMouseButton::Right)
                    }
                    Some(CGMouseButton::Center) => {
                        (CGEventType::OtherMouseDragged, CGMouseButton::Center)
                    }
                    _ => (CGEventType::MouseMoved, CGMouseButton::Left),
                };
                let cg = CGEvent::new_mouse_event(self.source.clone(), kind, point, button)
                    .map_err(|()| anyhow::anyhow!("could not build a mouse event"))?;
                set_button_number(&cg, dragging);
                apply_modifiers(&cg, event.modifiers);
                tracing::trace!(
                    target: "nebula_agent::input_trace",
                    x = point.x,
                    y = point.y,
                    cg_kind = kind as u32,
                    button = ?dragging,
                    bounds = ?self.bounds,
                    "posting native pointer movement (display points)"
                );
                anyhow::ensure!(self.post(&cg), "scoped pointer conversion unavailable");
            }

            InputKind::MouseDown | InputKind::MouseUp => {
                let Some(button) = cg_button(event.button) else {
                    return Ok(());
                };
                let point = self.point(event.x, event.y);
                self.last = point;
                let down = event.kind == InputKind::MouseDown;
                let kind = mouse_kind(event.button, down);
                let cg = CGEvent::new_mouse_event(self.source.clone(), kind, point, button)
                    .map_err(|()| anyhow::anyhow!("could not build a mouse event"))?;
                set_button_number(&cg, event.button);
                apply_modifiers(&cg, event.modifiers);

                // Click state is what turns two clicks into a double click.
                // Without it no application will ever see one.
                cg.set_integer_value_field(
                    EventField::MOUSE_EVENT_CLICK_STATE,
                    self.clicks
                        .update(event.button, down, point, self.clock.elapsed()),
                );
                tracing::trace!(
                    target: "nebula_agent::input_trace",
                    x = point.x,
                    y = point.y,
                    cg_kind = kind as u32,
                    button = ?event.button,
                    "posting native pointer button (display points)"
                );
                anyhow::ensure!(self.post(&cg), "scoped pointer conversion unavailable");

                if down {
                    if !self.held.contains(&event.button) {
                        self.held.push(event.button);
                    }
                } else {
                    self.held.retain(|button| *button != event.button);
                }
            }

            InputKind::Wheel => {
                // Line units, not pixels: the wire carries lines, and macOS
                // applies its own acceleration and direction preferences to
                // line-based scrolls exactly as it would for a real wheel.
                let horizontal = wheel_lines(&mut self.wheel[0], event.scroll_x);
                let vertical = wheel_lines(&mut self.wheel[1], event.scroll_y);
                if (vertical != 0 || horizontal != 0)
                    && self.target_alive.as_ref().is_none_or(|alive| alive())
                {
                    let location = self.target_pid.map(|_| self.point(event.x, event.y));
                    post_scroll(
                        vertical,
                        horizontal,
                        event.modifiers,
                        self.target_pid,
                        location,
                        self.target_window.map(|window| (window, self.bounds)),
                    );
                }
            }

            InputKind::KeyDown | InputKind::KeyUp => {
                let Some(code) = virtual_key(event.key) else {
                    tracing::trace!(hid = event.key.0, "no macOS key for this HID usage");
                    return Ok(());
                };
                let down = event.kind == InputKind::KeyDown;
                let cg = CGEvent::new_keyboard_event(self.source.clone(), code, down)
                    .map_err(|()| anyhow::anyhow!("could not build a key event"))?;
                let repeat = self.keys.update(code, down);
                let modifiers = self.keys.modifiers(code, event.modifiers);
                apply_modifiers(&cg, modifiers);
                cg.set_integer_value_field(
                    EventField::KEYBOARD_EVENT_AUTOREPEAT,
                    i64::from(down && (repeat || event.modifiers.contains(Modifiers::REPEAT))),
                );
                tracing::trace!(
                    target: "nebula_agent::input_trace",
                    hid = event.key.0,
                    virtual_key = code,
                    down,
                    repeat,
                    flags = cg_flags(modifiers).bits(),
                    "posting native physical key"
                );
                self.post(&cg);
            }

            InputKind::PointerLeave => self.release_buttons(event.modifiers),
        }
        Ok(())
    }
}

fn wheel_lines(remainder: &mut f64, delta: f32) -> i32 {
    if !delta.is_finite() {
        return 0;
    }
    *remainder += f64::from(delta);
    let lines = remainder
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX))
        .trunc() as i32;
    *remainder -= f64::from(lines);
    lines
}

fn double_click_interval() -> Duration {
    // This IOKit query reads the user's setting without requiring an AppKit
    // application or invoking UI on the injection worker.
    let seconds = unsafe {
        let handle = NXOpenEventStatus();
        if handle == 0 {
            return Duration::from_millis(500);
        }
        let seconds = NXClickTime(handle);
        NXCloseEventStatus(handle);
        seconds
    };
    if seconds.is_finite() && (0.0..=10.0).contains(&seconds) && seconds > 0.0 {
        Duration::from_secs_f64(seconds)
    } else {
        Duration::from_millis(500)
    }
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn NXOpenEventStatus() -> u32;
    fn NXClickTime(handle: u32) -> f64;
    fn NXCloseEventStatus(handle: u32);
}

/// Post a scroll event.
///
/// `core-graphics` does not wrap `CGEventCreateScrollWheelEvent`, so it is
/// declared here. Line units, not pixels: the wire carries lines, and macOS
/// then applies its own acceleration and natural-direction preference exactly
/// as it would for a real wheel, which is what makes scrolling feel local.
fn post_scroll(
    vertical: i32,
    horizontal: i32,
    modifiers: Modifiers,
    pid: Option<i32>,
    location: Option<CGPoint>,
    window: Option<(u32, ScopedBounds)>,
) {
    // Units: 0 is pixels, 1 is lines.
    const LINE: u32 = 1;
    // The HID tap, the same place physical devices deliver to.
    const TAP_HID: u32 = 0;

    unsafe {
        // A null source means "no particular device", which is what a
        // synthesised scroll should look like; the wheel deltas carry all the
        // meaning here.
        let event = CGEventCreateScrollWheelEvent(std::ptr::null(), LINE, 2, vertical, horizontal);
        if event.is_null() {
            return;
        }
        CGEventSetFlags(event, cg_flags(modifiers).bits());
        if let Some(location) = location {
            CGEventSetLocation(event, location);
        }
        if let Some(pid) = pid {
            if let Some(((window, bounds), point)) = window.zip(location) {
                if let Some(local) = window_local_point(point, bounds) {
                    let spec = WindowPointer {
                        window,
                        local,
                        kind: CGEventType::MouseMoved,
                        flags: cg_flags(modifiers).bits(),
                        click_count: 0,
                        button: 0,
                    };
                    with_window_event(&spec, |bound| {
                        CGEventSetType(bound, CGEventType::ScrollWheel as u32);
                        for field in [
                            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
                            EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
                            EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
                            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
                            EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
                            EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS,
                        ] {
                            CGEventSetIntegerValueField(
                                bound,
                                field,
                                CGEventGetIntegerValueField(event, field),
                            );
                        }
                        CGEventPostToPid(pid, bound);
                    });
                }
            }
        } else {
            CGEventPost(TAP_HID, event);
        }
        CFRelease(event);
    }
}

struct WindowPointer {
    window: u32,
    local: CGPoint,
    kind: CGEventType,
    flags: u64,
    click_count: i64,
    button: i64,
}

fn with_window_event(spec: &WindowPointer, post: impl FnOnce(*mut std::ffi::c_void)) -> bool {
    if spec.window == 0 || !spec.local.x.is_finite() || !spec.local.y.is_finite() {
        return false;
    }
    objc2::rc::autoreleasepool(|_| {
        let pressure = if matches!(
            spec.kind,
            CGEventType::LeftMouseDown
                | CGEventType::RightMouseDown
                | CGEventType::OtherMouseDown
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged
        ) {
            1.0
        } else {
            0.0
        };
        let make = |point| {
            NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType(spec.kind as usize), point, NSEventModifierFlags(spec.flags as usize),
            NSProcessInfo::processInfo().systemUptime(), spec.window as isize, None,
            0, spec.click_count as isize, pressure,
        )
        };
        // A foreign-window CG bridge flips in the sender's screen space. Measure
        // that public conversion instead of assuming a display pixel/point scale.
        // Never mutate CGEvent.location: that invalidates its window-local point.
        let Some(reference) = make(NSPoint { x: 0.0, y: 0.0 }) else {
            return false;
        };
        let Some(reference_event) = reference.CGEvent() else {
            return false;
        };
        let origin =
            unsafe { CGEventGetLocation(std::ptr::from_ref(&*reference_event).cast_mut().cast()) };
        if !origin.x.is_finite() || !origin.y.is_finite() {
            return false;
        }
        let Some(native) = make(NSPoint {
            x: spec.local.x - origin.x,
            y: origin.y - spec.local.y,
        }) else {
            return false;
        };
        let Some(event) = native.CGEvent() else {
            return false;
        };
        let raw = std::ptr::from_ref(&*event).cast_mut().cast();
        let serialized = unsafe { CGEventGetLocation(raw) };
        if !serialized.x.is_finite()
            || !serialized.y.is_finite()
            || (serialized.x - spec.local.x).abs() > 0.01
            || (serialized.y - spec.local.y).abs() > 0.01
        {
            return false;
        }
        // Button number is a public field; AppKit's factory otherwise defaults
        // extra-button events to the middle button.
        unsafe {
            CGEventSetIntegerValueField(raw, EventField::MOUSE_EVENT_BUTTON_NUMBER, spec.button);
        }
        post(raw);
        true
    })
}

fn post_window_mouse(event: &CGEvent, pid: i32, window: u32, bounds: ScopedBounds) -> bool {
    let Some(local) = window_local_point(event.location(), bounds) else {
        return false;
    };
    with_window_event(
        &WindowPointer {
            window,
            local,
            kind: event.get_type(),
            flags: event.get_flags().bits(),
            click_count: event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE),
            button: event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER),
        },
        |bound| unsafe { CGEventPostToPid(pid, bound) },
    )
}

fn window_local_point(global: CGPoint, bounds: ScopedBounds) -> Option<CGPoint> {
    let point = CGPoint::new(global.x - bounds.0, global.y - bounds.1);
    ([global.x, global.y, bounds.0, bounds.1, bounds.2, bounds.3]
        .iter()
        .all(|value| value.is_finite())
        && bounds.2 > 0.0
        && bounds.3 > 0.0
        && (0.0..=bounds.2).contains(&point.x)
        && (0.0..=bounds.3).contains(&point.y))
    .then_some(point)
}

/// What to do about a machine that has not been granted Accessibility.
const ACCESSIBILITY: &str = "cannot inject input. Grant Accessibility to this binary \
     in System Settings → Privacy & Security → Accessibility. The permission is bound \
     to the binary, so if it is already listed after a rebuild, remove it and add it again.";

/// Whether this process may post events to the window server.
pub fn trusted() -> bool {
    // SAFETY: a plain query with no arguments and no ownership transfer.
    unsafe { AXIsProcessTrusted() }
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateScrollWheelEvent(
        source: *const std::ffi::c_void,
        units: u32,
        wheel_count: u32,
        ...
    ) -> *mut std::ffi::c_void;
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CGEventPostToPid(pid: i32, event: *mut std::ffi::c_void);
    fn CGEventSetFlags(event: *mut std::ffi::c_void, flags: u64);
    fn CGEventSetLocation(event: *mut std::ffi::c_void, point: CGPoint);
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPoint;
    fn CGEventSetIntegerValueField(event: *mut std::ffi::c_void, field: u32, value: i64);
    fn CGEventGetIntegerValueField(event: *mut std::ffi::c_void, field: u32) -> i64;
    fn CGEventSetType(event: *mut std::ffi::c_void, kind: u32);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const std::ffi::c_void);
}

/// Set the modifier flags a synthesised event is seen with.
///
/// Modifiers travel with every event rather than as separate key presses, so
/// a release lost on the way cannot leave this machine stuck in, say,
/// permanent Command.
fn apply_modifiers(event: &CGEvent, modifiers: Modifiers) {
    event.set_flags(cg_flags(modifiers));
}

/// Translate wire modifiers into CoreGraphics event flags.
fn cg_flags(modifiers: Modifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::CGEventFlagNull;
    if modifiers.contains(Modifiers::SHIFT) {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    if modifiers.contains(Modifiers::ALT) {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if modifiers.contains(Modifiers::META) {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    if modifiers.contains(Modifiers::CAPS_LOCK) {
        flags |= CGEventFlags::CGEventFlagAlphaShift;
    }
    if modifiers.contains(Modifiers::FN) {
        flags |= CGEventFlags::CGEventFlagSecondaryFn;
    }
    flags
}

fn cg_button(button: MouseButton) -> Option<CGMouseButton> {
    match button {
        MouseButton::Left => Some(CGMouseButton::Left),
        MouseButton::Right => Some(CGMouseButton::Right),
        MouseButton::Middle | MouseButton::Back | MouseButton::Forward => {
            Some(CGMouseButton::Center)
        }
        MouseButton::None => None,
    }
}

fn button_number(button: MouseButton) -> Option<i64> {
    match button {
        MouseButton::Left => Some(0),
        MouseButton::Right => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Back => Some(3),
        MouseButton::Forward => Some(4),
        MouseButton::None => None,
    }
}

fn set_button_number(event: &CGEvent, button: MouseButton) {
    if let Some(number) = button_number(button) {
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
    }
}

fn mouse_kind(button: MouseButton, down: bool) -> CGEventType {
    match (button, down) {
        (MouseButton::Left, true) => CGEventType::LeftMouseDown,
        (MouseButton::Left, false) => CGEventType::LeftMouseUp,
        (MouseButton::Right, true) => CGEventType::RightMouseDown,
        (MouseButton::Right, false) => CGEventType::RightMouseUp,
        (_, true) => CGEventType::OtherMouseDown,
        (_, false) => CGEventType::OtherMouseUp,
    }
}

/// Map a USB HID keyboard usage to a macOS virtual key code.
///
/// The wire carries HID usages precisely so neither end has to know the
/// other's numbering; this table is where that promise is paid for on macOS.
/// The `kVK_*` values are positional, so the mapping holds whatever layout
/// either side is using.
fn virtual_key(key: KeyCode) -> Option<u16> {
    let vk: u16 = match key.0 {
        // Letters, HID a..z.
        0x04 => 0x00,
        0x05 => 0x0B,
        0x06 => 0x08,
        0x07 => 0x02,
        0x08 => 0x0E,
        0x09 => 0x03,
        0x0A => 0x05,
        0x0B => 0x04,
        0x0C => 0x22,
        0x0D => 0x26,
        0x0E => 0x28,
        0x0F => 0x25,
        0x10 => 0x2E,
        0x11 => 0x2D,
        0x12 => 0x1F,
        0x13 => 0x23,
        0x14 => 0x0C,
        0x15 => 0x0F,
        0x16 => 0x01,
        0x17 => 0x11,
        0x18 => 0x20,
        0x19 => 0x09,
        0x1A => 0x0D,
        0x1B => 0x07,
        0x1C => 0x10,
        0x1D => 0x06,

        // Digits, HID 1..9 then 0.
        0x1E => 0x12,
        0x1F => 0x13,
        0x20 => 0x14,
        0x21 => 0x15,
        0x22 => 0x17,
        0x23 => 0x16,
        0x24 => 0x1A,
        0x25 => 0x1C,
        0x26 => 0x19,
        0x27 => 0x1D,

        0x28 => 0x24, // Return
        0x29 => 0x35, // Escape
        0x2A => 0x33, // Backspace
        0x2B => 0x30, // Tab
        0x2C => 0x31, // Space
        0x2D => 0x1B, // -
        0x2E => 0x18, // =
        0x2F => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // backslash
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        0x39 => 0x39, // Caps Lock

        // Function keys F1..F12.
        0x3A => 0x7A,
        0x3B => 0x78,
        0x3C => 0x63,
        0x3D => 0x76,
        0x3E => 0x60,
        0x3F => 0x61,
        0x40 => 0x62,
        0x41 => 0x64,
        0x42 => 0x65,
        0x43 => 0x6D,
        0x44 => 0x67,
        0x45 => 0x6F,
        0x68 => 0x69, // F13
        0x69 => 0x6B, // F14
        0x6A => 0x71, // F15
        0x6B => 0x6A, // F16
        0x6C => 0x40, // F17
        0x6D => 0x4F, // F18
        0x6E => 0x50, // F19
        0x6F => 0x5A, // F20

        0x49 => 0x72, // Insert / Help
        0x4A => 0x73, // Home
        0x4B => 0x74, // Page Up
        0x4C => 0x75, // Forward Delete
        0x4D => 0x77, // End
        0x4E => 0x79, // Page Down
        0x4F => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up

        // Keypad.
        0x53 => 0x47, // Num Lock / Clear
        0x54 => 0x4B, // divide
        0x55 => 0x43, // multiply
        0x56 => 0x4E, // minus
        0x57 => 0x45, // plus
        0x58 => 0x4C, // Enter
        0x59 => 0x53,
        0x5A => 0x54,
        0x5B => 0x55,
        0x5C => 0x56,
        0x5D => 0x57,
        0x5E => 0x58,
        0x5F => 0x59,
        0x60 => 0x5B,
        0x61 => 0x5C,
        0x62 => 0x52,
        0x63 => 0x41, // decimal

        // Modifiers, when sent as keys in their own right.
        0xE0 => 0x3B,
        0xE1 => 0x38,
        0xE2 => 0x3A,
        0xE3 => 0x37,
        0xE4 => 0x3E,
        0xE5 => 0x3C,
        0xE6 => 0x3D,
        0xE7 => 0x36,

        _ => return None,
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_pointer_coordinates_use_owned_window_top_left_not_display_origin() {
        let point =
            window_local_point(CGPoint::new(294.0, 334.0), (180.0, 58.0, 600.0, 412.0)).unwrap();
        assert_eq!((point.x, point.y), (114.0, 276.0));
        assert!(
            window_local_point(CGPoint::new(179.0, 334.0), (180.0, 58.0, 600.0, 412.0)).is_none()
        );
        assert!(
            window_local_point(CGPoint::new(f64::NAN, 334.0), (180.0, 58.0, 600.0, 412.0))
                .is_none()
        );
    }

    #[test]
    fn scoped_pointer_binding_rejects_missing_native_target() {
        assert!(!with_window_event(
            &WindowPointer {
                window: 0,
                local: CGPoint::new(1.0, 1.0),
                kind: CGEventType::MouseMoved,
                flags: 0,
                click_count: 0,
                button: 0,
            },
            |_| panic!("missing target must never post")
        ));
    }

    #[test]
    fn click_counts_match_down_and_up_and_expire() {
        let mut clicks = Clicks::new(Duration::from_millis(500));
        let point = CGPoint::new(100.0, 200.0);
        for (time, count) in [(0, 1), (200, 2), (400, 3), (1000, 1)] {
            assert_eq!(
                clicks.update(MouseButton::Left, true, point, Duration::from_millis(time)),
                count
            );
            assert_eq!(
                clicks.update(
                    MouseButton::Left,
                    false,
                    point,
                    Duration::from_millis(time + 50)
                ),
                count
            );
        }
    }

    #[test]
    fn clicks_require_same_button_nearby_completed_press_and_monotonic_time() {
        let mut clicks = Clicks::new(Duration::from_millis(500));
        let at = CGPoint::new(10.0, 10.0);
        let now = Duration::from_millis(100);
        assert_eq!(clicks.update(MouseButton::Left, true, at, now), 1);
        assert_eq!(clicks.update(MouseButton::Left, true, at, now), 1);
        assert_eq!(clicks.update(MouseButton::Left, false, at, now), 1);
        assert_eq!(clicks.update(MouseButton::Right, true, at, now), 1);
        assert_eq!(clicks.update(MouseButton::Right, false, at, now), 1);
        assert_eq!(clicks.update(MouseButton::Left, true, at, now), 1);
        assert_eq!(clicks.update(MouseButton::Left, false, at, now), 1);
        assert_eq!(
            clicks.update(MouseButton::Left, true, at, Duration::ZERO),
            1
        );
        clicks.update(MouseButton::Left, false, at, Duration::ZERO);
        clicks.moved(CGPoint::new(20.0, 10.0));
        clicks.moved(at);
        assert_eq!(clicks.update(MouseButton::Left, true, at, now), 1);
    }

    #[test]
    fn repeat_and_disconnect_tracking_do_not_duplicate_key_releases() {
        let mut keys = HeldKeys::default();
        assert!(!keys.update(0x37, true));
        assert!(!keys.update(0x00, true));
        assert!(keys.update(0x00, true));
        assert_eq!(keys.release_all(), vec![0x37, 0x00]);
        assert!(keys.release_all().is_empty());
        assert!(!keys.update(0x00, true));
        assert!(!keys.update(0x00, false));
        assert!(keys.release_all().is_empty());
    }

    #[test]
    fn physical_modifier_transitions_override_stale_snapshots() {
        let mut keys = HeldKeys::default();
        keys.update(0x37, true);
        assert_eq!(keys.modifiers(0x37, Modifiers::NONE), Modifiers::META);
        keys.update(0x36, true);
        keys.update(0x37, false);
        assert_eq!(keys.modifiers(0x37, Modifiers::NONE), Modifiers::META);
        keys.update(0x36, false);
        assert_eq!(keys.modifiers(0x36, Modifiers::META), Modifiers::NONE);
    }

    #[test]
    fn fractional_trackpad_deltas_accumulate_instead_of_disappearing() {
        let mut remainder = 0.0;
        assert_eq!(wheel_lines(&mut remainder, 0.25), 0);
        assert_eq!(wheel_lines(&mut remainder, 0.25), 0);
        assert_eq!(wheel_lines(&mut remainder, 0.5), 1);
        assert_eq!(wheel_lines(&mut remainder, -0.5), 0);
        assert_eq!(wheel_lines(&mut remainder, -0.5), -1);
        assert_eq!(wheel_lines(&mut remainder, f32::NAN), 0);
        assert_eq!(remainder, 0.0);
    }

    #[test]
    fn extra_buttons_keep_distinct_native_numbers_and_click_counts() {
        let mut clicks = Clicks::new(Duration::from_millis(500));
        let at = CGPoint::new(1.0, 1.0);
        for (button, number) in [
            (MouseButton::Middle, 2),
            (MouseButton::Back, 3),
            (MouseButton::Forward, 4),
        ] {
            assert_eq!(button_number(button), Some(number));
            assert!(matches!(
                mouse_kind(button, true),
                CGEventType::OtherMouseDown
            ));
            assert_eq!(clicks.update(button, true, at, Duration::ZERO), 1);
            assert_eq!(clicks.update(button, false, at, Duration::ZERO), 1);
        }
    }

    #[test]
    fn the_key_table_never_maps_two_usages_to_one_key() {
        // A collision would make one usage silently type the wrong character,
        // which is the sort of bug that only appears for the one person with
        // that keyboard.
        let mut seen = std::collections::HashMap::new();
        for hid in 0u32..=0xFF {
            if let Some(vk) = virtual_key(KeyCode(hid)) {
                if let Some(previous) = seen.insert(vk, hid) {
                    panic!("HID {hid:#x} and {previous:#x} both map to virtual key {vk:#x}");
                }
            }
        }
        assert!(
            seen.len() >= 90,
            "the table should cover a full keyboard, found {}",
            seen.len()
        );
    }

    #[test]
    fn the_keys_are_where_a_mac_puts_them() {
        // Spot-checked against the kVK_ANSI_* constants: an off-by-one would
        // still pass a collision test.
        assert_eq!(virtual_key(KeyCode::A), Some(0x00));
        assert_eq!(virtual_key(KeyCode::Z), Some(0x06));
        assert_eq!(virtual_key(KeyCode::SPACE), Some(0x31));
        assert_eq!(virtual_key(KeyCode::ENTER), Some(0x24));
        assert_eq!(virtual_key(KeyCode::ESCAPE), Some(0x35));
        assert_eq!(virtual_key(KeyCode::ARROW_LEFT), Some(0x7B));
    }

    #[test]
    fn an_unknown_usage_is_dropped_rather_than_guessed() {
        assert_eq!(virtual_key(KeyCode::NONE), None);
        assert_eq!(virtual_key(KeyCode(0xFFFF)), None);
    }
}
