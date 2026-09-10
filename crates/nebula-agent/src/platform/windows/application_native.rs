use std::{
    collections::BTreeMap,
    os::windows::io::AsRawHandle,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use anyhow::{ensure, Context};
use windows::{
    core::{BOOL, PCWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, LPARAM, RECT, WPARAM},
        Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
        Security::{
            GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
            TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
        UI::WindowsAndMessaging::*,
    },
};

use super::application_policy::{integrity_permits, owns_window};

pub(super) struct Instance {
    child: Mutex<Child>,
    pub pid: u32,
    property: Vec<u16>,
    integrity: u32,
    active: AtomicBool,
}

impl Instance {
    pub fn launch(
        executable: &str,
        args: &[String],
        working_dir: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let path = Path::new(executable);
        ensure!(
            path.is_absolute(),
            "published executable must be an absolute trusted path"
        );
        ensure!(
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe")),
            "application launch requires a native .exe, not a shell script"
        );
        ensure!(
            !executable.contains('\0') && args.iter().all(|arg| !arg.contains('\0')),
            "application command contains NUL"
        );
        let path = path
            .canonicalize()
            .context("published executable is unavailable")?;
        ensure!(path.is_file(), "published executable is not a file");
        let integrity = unsafe { process_integrity(GetCurrentProcess())? };
        // Never invoke a shell, concatenate arguments, attach by process name,
        // or reuse/terminate a user's pre-existing application instance.
        let mut command = Command::new(path);
        if let Some(directory) = working_dir {
            ensure!(
                Path::new(directory).is_absolute(),
                "application working directory must be absolute"
            );
            let directory = Path::new(directory)
                .canonicalize()
                .context("application working directory is unavailable")?;
            ensure!(
                directory.is_dir(),
                "application working directory is not a directory"
            );
            command.current_dir(directory);
        }
        let child = command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("could not launch trusted application")?;
        let target_integrity = unsafe { process_integrity(HANDLE(child.as_raw_handle()))? };
        ensure!(
            integrity_permits(integrity, target_integrity),
            "application integrity differs from agent; elevation/UAC bypass is forbidden"
        );
        Ok(Arc::new(Self {
            pid: child.id(),
            child: Mutex::new(child),
            property: format!("NebulaDesk.Application.{}\0", uuid::Uuid::new_v4())
                .encode_utf16()
                .collect(),
            integrity,
            active: AtomicBool::new(true),
        }))
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.active.load(Ordering::Acquire),
            "application instance was revoked"
        );
        let mut child = self
            .child
            .lock()
            .map_err(|_| anyhow::anyhow!("application lock poisoned"))?;
        ensure!(
            child.try_wait()?.is_none(),
            "launched application exited; existing-instance handoff is not owned"
        );
        let target = unsafe { process_integrity(HANDLE(child.as_raw_handle()))? };
        ensure!(
            integrity_permits(self.integrity, target),
            "application changed integrity; scoped access refused"
        );
        Ok(())
    }

    pub fn revoke(&self) {
        self.active.store(false, Ordering::Release);
    }

    pub fn discover(&self) -> anyhow::Result<Vec<isize>> {
        self.validate()?;
        let mut result = Enumeration {
            pid: self.pid,
            windows: Vec::new(),
        };
        unsafe {
            EnumWindows(
                Some(enumerate),
                LPARAM(&mut result as *mut Enumeration as isize),
            )?;
        }
        Ok(result.windows)
    }
}

struct Enumeration {
    pid: u32,
    windows: Vec<isize>,
}

unsafe extern "system" fn enumerate(hwnd: HWND, data: LPARAM) -> BOOL {
    let state = unsafe { &mut *(data.0 as *mut Enumeration) };
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == state.pid && unsafe { IsWindowVisible(hwnd).as_bool() } {
        state.windows.push(hwnd.0 as isize);
    }
    BOOL(1)
}

pub(super) struct Window {
    pub instance: Arc<Instance>,
    raw: isize,
    generation: usize,
}

impl Window {
    pub fn bind(
        instance: Arc<Instance>,
        raw: isize,
        generation: usize,
    ) -> anyhow::Result<Arc<Self>> {
        ensure!(generation != 0, "invalid window generation");
        instance.validate()?;
        let hwnd = HWND(raw as *mut _);
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        ensure!(
            pid == instance.pid,
            "window does not belong to launched application"
        );
        unsafe {
            SetPropW(
                hwnd,
                PCWSTR(instance.property.as_ptr()),
                Some(HANDLE(generation as *mut _)),
            )?;
        }
        let window = Arc::new(Self {
            instance,
            raw,
            generation,
        });
        window.validate()?;
        Ok(window)
    }

    pub fn hwnd(&self) -> HWND {
        HWND(self.raw as *mut _)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.instance.validate()?;
        let mut pid = 0;
        let hwnd = self.hwnd();
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        let generation =
            unsafe { GetPropW(hwnd, PCWSTR(self.instance.property.as_ptr())) }.0 as usize;
        ensure!(
            owns_window(self.instance.pid, pid, true, generation == self.generation),
            "application window closed, changed ownership, or its native handle was recycled"
        );
        Ok(())
    }

    pub fn bounds(&self) -> anyhow::Result<RECT> {
        self.validate()?;
        let mut rect = RECT::default();
        unsafe {
            DwmGetWindowAttribute(
                self.hwnd(),
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rect as *mut RECT as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )?;
        }
        ensure!(
            rect.right > rect.left && rect.bottom > rect.top,
            "application window has no capture geometry"
        );
        Ok(rect)
    }

    pub fn title(&self) -> anyhow::Result<String> {
        self.validate()?;
        let mut text = [0u16; 1024];
        let count = unsafe { GetWindowTextW(self.hwnd(), &mut text) }.max(0) as usize;
        Ok(String::from_utf16_lossy(&text[..count]))
    }

    pub fn owner(&self) -> Option<isize> {
        unsafe {
            GetWindow(self.hwnd(), GW_OWNER)
                .ok()
                .map(|value| value.0 as isize)
                .filter(|value| *value != 0)
        }
    }

    pub fn focus(&self) -> anyhow::Result<()> {
        self.validate()?;
        ensure!(
            unsafe { SetForegroundWindow(self.hwnd()).as_bool() },
            "Windows refused foreground activation; local user focus consent is required"
        );
        Ok(())
    }

    pub fn resize(&self, width: u32, height: u32) -> anyhow::Result<()> {
        self.validate()?;
        ensure!(
            (64..=8192).contains(&width) && (64..=8192).contains(&height),
            "application surface dimensions must be between 64 and 8192"
        );
        unsafe {
            SetWindowPos(
                self.hwnd(),
                None,
                0,
                0,
                width as i32,
                height as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )?;
        }
        Ok(())
    }

    pub fn show(&self, command: SHOW_WINDOW_CMD) -> anyhow::Result<()> {
        self.validate()?;
        unsafe {
            let _ = ShowWindow(self.hwnd(), command);
        }
        Ok(())
    }

    pub fn close(&self) -> anyhow::Result<()> {
        self.validate()?;
        unsafe {
            PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0))?;
        }
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(self.hwnd(), Some(&mut pid));
        }
        let generation =
            unsafe { GetPropW(self.hwnd(), PCWSTR(self.instance.property.as_ptr())) }.0 as usize;
        if pid == self.instance.pid && generation == self.generation {
            unsafe {
                let _ = RemovePropW(self.hwnd(), PCWSTR(self.instance.property.as_ptr()));
            }
        }
    }
}

pub(super) struct Windows {
    pub instance: Arc<Instance>,
    pub known: BTreeMap<isize, (u64, Arc<Window>)>,
    next: u64,
}

impl Windows {
    pub fn new(instance: Arc<Instance>) -> Self {
        Self {
            instance,
            known: BTreeMap::new(),
            next: 1,
        }
    }

    pub fn refresh(&mut self) -> anyhow::Result<()> {
        let discovered = self.instance.discover()?;
        // A hidden/minimized owned window is temporarily unavailable, not a
        // retired identity. Only destruction/replacement revokes its ID.
        self.known
            .retain(|_, (_, window)| window.validate().is_ok());
        for raw in discovered {
            if self.known.contains_key(&raw) {
                continue;
            }
            let id = self.next;
            self.next = self
                .next
                .checked_add(1)
                .context("surface generation exhausted")?;
            let generation = usize::try_from(id).context("native window generation exhausted")?;
            let window = Window::bind(self.instance.clone(), raw, generation)?;
            self.known.insert(raw, (id, window));
        }
        Ok(())
    }
}

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

unsafe fn process_integrity(process: HANDLE) -> anyhow::Result<u32> {
    let mut handle = HANDLE::default();
    unsafe {
        OpenProcessToken(process, TOKEN_QUERY, &mut handle)?;
    }
    let token = Token(handle);
    let mut length = 0;
    unsafe {
        let _ = GetTokenInformation(token.0, TokenIntegrityLevel, None, 0, &mut length);
    }
    ensure!(
        length as usize >= std::mem::size_of::<TOKEN_MANDATORY_LABEL>() && length <= 65536,
        "cannot determine application integrity"
    );
    // usize storage guarantees the alignment required by TOKEN_MANDATORY_LABEL.
    let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )?;
        let label = &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let count = *GetSidSubAuthorityCount(label.Label.Sid);
        ensure!(count > 0, "application integrity SID is empty");
        Ok(*GetSidSubAuthority(label.Label.Sid, u32::from(count - 1)))
    }
}
