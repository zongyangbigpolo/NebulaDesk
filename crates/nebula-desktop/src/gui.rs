use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use serde_json::Value;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager,
};

use crate::{
    error::{DesktopError, Result},
    model::{Request, StateExt},
    sessions::ChildCommand,
    Desktop,
};

#[derive(Default)]
struct Exiting {
    confirmed: AtomicBool,
    prompting: AtomicBool,
    stop_prompting: AtomicBool,
}

#[tauri::command]
async fn desktop_request(
    host: tauri::State<'_, Arc<Desktop>>,
    exiting: tauri::State<'_, Exiting>,
    request: Request,
) -> Result<Value> {
    if exiting.prompting.load(Ordering::SeqCst) {
        return Err(DesktopError::new(
            "closing",
            "The application is preparing to exit.",
        ));
    }
    if let Request::OpenPermissionSettings { permission } = request {
        return open_permission_settings(permission)
            .await
            .map(|_| Value::Null);
    }
    if let Request::SendFiles { session_id } = request {
        let session = host
            .sessions
            .list()
            .await
            .into_iter()
            .find(|s| s.session_id == session_id && s.state.active())
            .ok_or_else(|| {
                DesktopError::new("session_closed", "This session is no longer active.")
            })?;
        let files = rfd::AsyncFileDialog::new()
            .set_title("Send files to remote desktop")
            .pick_files()
            .await;
        if let Some(files) = files {
            let paths = files
                .iter()
                .map(|file| {
                    file.path().to_str().map(str::to_owned).ok_or_else(|| {
                        DesktopError::new(
                            "invalid_path",
                            "The selected file path cannot be represented for this native client.",
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            host.sessions
                .command(session.session_id, ChildCommand::SendFiles { paths })
                .await?;
        }
        return Ok(Value::Null);
    }
    host.request(request).await
}

async fn open_permission_settings(permission: crate::model::Permission) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use crate::model::Permission;
        let target = match permission {
            Permission::Screen | Permission::Audio => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
            Permission::Input => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
        };
        let mut command = tokio::process::Command::new("/usr/bin/open");
        command
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), command.status())
            .await
            .map_err(|_| {
                DesktopError::new("settings_unavailable", "System Settings did not respond.")
            })?
            .map_err(|_| {
                DesktopError::new(
                    "settings_unavailable",
                    "System Settings could not be opened.",
                )
            })?;
        if !status.success() {
            return Err(DesktopError::new(
                "settings_unavailable",
                "System Settings rejected the privacy panel request.",
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = permission;
        Err(DesktopError::new(
            "unsupported",
            "Open your operating system privacy settings to manage capture and input permissions.",
        ))
    }
}

fn sidecar(name: &str) -> std::io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let suffix = std::env::consts::EXE_SUFFIX;
    let filename = format!("{name}{suffix}");
    let bundled = executable
        .parent()
        .ok_or_else(|| std::io::Error::other("missing executable directory"))?
        .join(&filename);
    if bundled.is_file() {
        return Ok(bundled);
    }
    #[cfg(debug_assertions)]
    {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        for profile in ["debug", "release"] {
            let path = root.join("target").join(profile).join(&filename);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "required bundled native sidecar is missing",
    ))
}

fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Err(error) = window
            .show()
            .and_then(|_| window.unminimize())
            .and_then(|_| window.set_focus())
        {
            eprintln!("The management window could not be shown: {error}");
        }
    }
}

fn request_exit(app: &tauri::AppHandle) {
    let exiting = app.state::<Exiting>();
    if exiting.prompting.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let host = app.state::<Arc<Desktop>>().inner().clone();
        let active = host
            .sessions
            .list()
            .await
            .iter()
            .filter(|s| s.state.active())
            .count();
        if active > 0 {
            let result = rfd::AsyncMessageDialog::new()
                .set_title("Quit NebulaDesk?")
                .set_description(format!("Quit and disconnect {active} active native session(s)? Your separately enabled local host will remain running."))
                .set_buttons(rfd::MessageButtons::OkCancel)
                .set_level(rfd::MessageLevel::Warning)
                .show().await;
            if result != rfd::MessageDialogResult::Ok {
                app.state::<Exiting>()
                    .prompting
                    .store(false, Ordering::SeqCst);
                return;
            }
        }
        host.shutdown().await;
        app.state::<Exiting>()
            .confirmed
            .store(true, Ordering::SeqCst);
        app.exit(0);
    });
}

fn stop_local_sharing(app: &tauri::AppHandle) {
    if app
        .state::<Exiting>()
        .stop_prompting
        .swap(true, Ordering::SeqCst)
    {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let confirmed = rfd::AsyncMessageDialog::new()
            .set_title("Stop local sharing?")
            .set_description("Stop the local Agent and end all incoming sessions to this computer? Your outgoing native sessions will remain open. No manager login is required.")
            .set_buttons(rfd::MessageButtons::OkCancel)
            .set_level(rfd::MessageLevel::Warning)
            .show().await;
        if confirmed == rfd::MessageDialogResult::Ok {
            let host = app.state::<Arc<Desktop>>().inner().clone();
            if let Err(error) = host.local_agent.set_enabled(false).await {
                rfd::AsyncMessageDialog::new()
                    .set_title("Local sharing could not be stopped")
                    .set_description(error.message)
                    .set_level(rfd::MessageLevel::Error)
                    .set_buttons(rfd::MessageButtons::Ok)
                    .show()
                    .await;
            }
        }
        app.state::<Exiting>()
            .stop_prompting
            .store(false, Ordering::SeqCst);
    });
}

pub fn run() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let app = tauri::Builder::default()
        .manage(Exiting::default())
        .invoke_handler(tauri::generate_handler![desktop_request])
        .setup(|app| {
            let path = match std::env::var_os("NEBULA_DESKTOP_AGENT_STATE") {
                Some(path) => PathBuf::from(path),
                None => app
                    .path()
                    .app_data_dir()?
                    .join("managed-agent")
                    .join("identity.json"),
            };
            app.manage(Arc::new(Desktop::new(
                sidecar("nebula-client")?,
                sidecar("nebula-desktop-agent")?,
                path,
            )));
            let open = MenuItem::with_id(app, "open", "Open NebulaDesk", true, None::<&str>)?;
            let stop = MenuItem::with_id(
                app,
                "stop-sharing",
                "Stop local sharing",
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "Quit NebulaDesk", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &stop, &quit])?;
            let pixels: Vec<u8> = (0..16 * 16).flat_map(|_| [41, 118, 255, 255]).collect();
            TrayIconBuilder::new()
                .tooltip("NebulaDesk")
                .menu(&menu)
                .icon(tauri::image::Image::new_owned(pixels, 16, 16))
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show(app),
                    "stop-sharing" => stop_local_sharing(app),
                    "quit" => request_exit(app),
                    _ => {}
                })
                .build(app)?;
            let window =
                tauri::WebviewWindowBuilder::from_config(app, &app.config().app.windows[0])?
                    .on_navigation(|url| {
                        matches!(url.scheme(), "tauri" | "asset")
                            || matches!(url.host_str(), Some("tauri.localhost"))
                            || (cfg!(debug_assertions)
                                && url.host_str() == Some("localhost")
                                && url.port() == Some(1420))
                    })
                    .build()?;
            let handle = window.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if let Err(error) = handle.hide() {
                        eprintln!("The management window could not be hidden: {error}");
                    }
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())?;
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { ref api, .. } = event {
            if !app.state::<Exiting>().confirmed.load(Ordering::SeqCst) {
                api.prevent_exit();
                request_exit(app);
            }
        }
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show(app);
        }
    });
    Ok(())
}
