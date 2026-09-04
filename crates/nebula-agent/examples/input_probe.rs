//! Prove that input injection reaches the real desktop.
//!
//! Run with `cargo run -p nebula-agent --example input_probe`. It moves the
//! pointer to the middle of the primary display, reads back where the window
//! server thinks the pointer is, and puts it back where it found it.
//!
//! This needs the Accessibility permission, which macOS grants per binary:
//! a rebuild invalidates it, so remove and re-add the binary in
//! System Settings → Privacy & Security → Accessibility if it starts failing.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this probe is macOS only");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use ndp_proto::{InputEvent, Modifiers};
    use nebula_agent::media::InputInjector;
    use nebula_agent::platform::macos::input::MacInput;

    // Ask macOS to put up its own dialog if the permission is missing. This
    // is what registers the binary in System Settings → Accessibility, so
    // there is a row to switch on: the list will not offer a binary it has
    // never been asked about.
    prompt_for_accessibility();

    let before = pointer();
    println!("pointer starts at {before:?}");

    let mut input = MacInput::new()?;

    // The middle of the display, in the normalised coordinates a client
    // sends: the agent is what turns them into pixels.
    input.inject(&InputEvent::mouse_move(0.5, 0.5, Modifiers::NONE))?;

    // Injection is asynchronous by design — it crosses to the injection
    // thread and then to the window server.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after = pointer();
    println!("pointer moved to {after:?}");

    // Put it back, so running this does not leave the machine's pointer
    // parked in the middle of the screen.
    let (width, height) = display_size();
    input.inject(&InputEvent::mouse_move(
        (before.0 / width) as f32,
        (before.1 / height) as f32,
        Modifiers::NONE,
    ))?;
    std::thread::sleep(std::time::Duration::from_millis(200));

    let moved = (after.0 - before.0).abs() > 1.0 || (after.1 - before.1).abs() > 1.0;
    let centred = (after.0 - width / 2.0).abs() < width * 0.02
        && (after.1 - height / 2.0).abs() < height * 0.02;
    if moved && centred {
        println!("input injection reaches the desktop");
        Ok(())
    } else {
        anyhow::bail!(
            "the pointer ended at {after:?}, expected the middle of a \
             {width}x{height} display"
        )
    }
}

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types)]
mod cg {
    pub type CGEventRef = *mut std::ffi::c_void;
    pub type CGEventSourceRef = *mut std::ffi::c_void;
    pub type CGDirectDisplayID = u32;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
        pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
        pub fn CGMainDisplayID() -> CGDirectDisplayID;
        pub fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
        pub fn CGDisplayPixelsHigh(display: CGDirectDisplayID) -> usize;
    }
}

#[cfg(target_os = "macos")]
fn pointer() -> (f64, f64) {
    // SAFETY: a null source is documented as valid and means "the current
    // state"; the event is a Core Foundation object we immediately read and
    // then leak, which costs one allocation for the life of this probe.
    unsafe {
        let event = cg::CGEventCreate(std::ptr::null_mut());
        let point = cg::CGEventGetLocation(event);
        (point.x, point.y)
    }
}

#[cfg(target_os = "macos")]
fn display_size() -> (f64, f64) {
    // SAFETY: plain queries against the main display, which always exists on
    // a machine with a screen.
    unsafe {
        let display = cg::CGMainDisplayID();
        (
            cg::CGDisplayPixelsWide(display) as f64,
            cg::CGDisplayPixelsHigh(display) as f64,
        )
    }
}

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types, non_upper_case_globals)]
mod ax {
    pub type CFTypeRef = *const std::ffi::c_void;
    pub type CFStringRef = *const std::ffi::c_void;
    pub type CFDictionaryRef = *const std::ffi::c_void;
    pub type CFAllocatorRef = *const std::ffi::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        pub static kAXTrustedCheckOptionPrompt: CFStringRef;
        pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFBooleanTrue: CFTypeRef;
        pub static kCFTypeDictionaryKeyCallBacks: std::ffi::c_void;
        pub static kCFTypeDictionaryValueCallBacks: std::ffi::c_void;
        pub fn CFDictionaryCreate(
            allocator: CFAllocatorRef,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            count: isize,
            key_callbacks: *const std::ffi::c_void,
            value_callbacks: *const std::ffi::c_void,
        ) -> CFDictionaryRef;
        pub fn CFRelease(cf: CFTypeRef);
    }
}

/// Ask for Accessibility, showing the system dialog when it is missing.
#[cfg(target_os = "macos")]
fn prompt_for_accessibility() {
    // SAFETY: a one-entry dictionary built from Core Foundation's own
    // constants, handed straight back to Core Foundation and released.
    unsafe {
        let keys = [ax::kAXTrustedCheckOptionPrompt as ax::CFTypeRef];
        let values = [ax::kCFBooleanTrue];
        let options = ax::CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::addr_of!(ax::kCFTypeDictionaryKeyCallBacks).cast(),
            std::ptr::addr_of!(ax::kCFTypeDictionaryValueCallBacks).cast(),
        );
        let trusted = ax::AXIsProcessTrustedWithOptions(options);
        ax::CFRelease(options);
        if !trusted {
            println!(
                "macOS should now be showing a dialog. Switch this binary on \
                 in System Settings, then run the probe again."
            );
        }
    }
}
