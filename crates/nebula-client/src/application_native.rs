//! Native ownership relationships; called only from the winit main thread.
use winit::window::{Window, WindowAttributes};

pub fn attributes(
    attributes: WindowAttributes,
    parent: Option<&Window>,
) -> anyhow::Result<WindowAttributes> {
    #[cfg(target_os = "windows")]
    if let Some(parent) = parent {
        use winit::platform::windows::WindowAttributesExtWindows;
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let RawWindowHandle::Win32(handle) = parent.window_handle()?.as_raw() {
            return Ok(attributes.with_owner_window(handle.hwnd.get()));
        }
        anyhow::bail!("native owner handle unavailable");
    }
    let _ = parent;
    Ok(attributes)
}

pub fn set_blocked(window: &Window, blocked: bool) {
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowExtWindows;
        window.set_enable(!blocked);
    }
    let _ = (window, blocked);
}

pub fn attach(child: &Window, parent: &Window) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    unsafe {
        macos::relate(parent, child, true)?;
    }
    #[cfg(target_os = "windows")]
    windows::owner(child, Some(parent))?;
    let _ = (child, parent);
    Ok(())
}

pub fn detach(child: &Window, parent: &Window) {
    #[cfg(target_os = "macos")]
    unsafe {
        let _ = macos::relate(parent, child, false);
    }
    #[cfg(target_os = "windows")]
    {
        let _ = windows::owner(child, None);
    }
    let _ = (child, parent);
}

#[cfg(target_os = "windows")]
mod windows {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    #[link(name = "user32")]
    unsafe extern "system" {
        #[cfg_attr(target_pointer_width = "32", link_name = "SetWindowLongW")]
        fn SetWindowLongPtrW(window: isize, index: i32, value: isize) -> isize;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetLastError(error: u32);
        fn GetLastError() -> u32;
    }

    fn handle(window: &Window) -> anyhow::Result<isize> {
        let RawWindowHandle::Win32(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("native owner handle unavailable");
        };
        Ok(handle.hwnd.get())
    }

    pub fn owner(child: &Window, parent: Option<&Window>) -> anyhow::Result<()> {
        let child = handle(child)?;
        let parent = parent.map(handle).transpose()?.unwrap_or(0);
        // GWLP_HWNDPARENT changes the owner of a top-level window, not WS_CHILD.
        unsafe {
            SetLastError(0);
            if SetWindowLongPtrW(child, -8, parent) == 0 && GetLastError() != 0 {
                anyhow::bail!("could not update native window owner");
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn sel_registerName(name: *const c_char) -> *const c_void;
        fn objc_msgSend();
    }

    unsafe fn native(window: &Window) -> anyhow::Result<*mut c_void> {
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            anyhow::bail!("AppKit window handle unavailable");
        };
        let send: unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let window = send(
            handle.ns_view.as_ptr(),
            sel_registerName(c"window".as_ptr()),
        );
        anyhow::ensure!(!window.is_null(), "AppKit window unavailable");
        Ok(window)
    }

    /// Live Arc<Window>s keep both objects valid across these synchronous messages.
    pub unsafe fn relate(parent: &Window, child: &Window, attach: bool) -> anyhow::Result<()> {
        let parent = native(parent)?;
        let child = native(child)?;
        if attach {
            let send: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void, isize) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            send(
                parent,
                sel_registerName(c"addChildWindow:ordered:".as_ptr()),
                child,
                1,
            );
        } else {
            let send: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            send(
                parent,
                sel_registerName(c"removeChildWindow:".as_ptr()),
                child,
            );
        }
        Ok(())
    }
}
