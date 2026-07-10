//! Desktop (Tauri) entry point — compiled only under `--features desktop`.
//!
//! Wraps the headless Skald backend in a Tauri event loop. The backend runs on
//! Tauri's shared tokio runtime (no dual runtime). A system-tray icon provides
//! `Open` (show+focus the main window) and `Quit` (graceful shutdown).
//!
//! ## Window policy
//! The main window starts hidden. The traffic-light red / window X button
//! *hides* it instead of closing — the app keeps running in the tray. Only the
//! tray's `Quit` menu item (or Cmd+Q / system termination) actually shuts the
//! backend down and exits.
//!
//! ## Restart safety
//! The `tauri::RunEvent::ExitRequested` handler is re-entrant-guarded by an
//! `AtomicBool`: the first trigger prevents the exit, runs the async backend
//! shutdown, then calls `app.exit(0)` (which would otherwise loop).
//!
//! See `docs/desktop.md` for the architecture overview and build instructions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tracing::{error, info, warn};

use crate::{config::Config, run_backend, shutdown_backend, Backend};

/// Slot for the backend handle, kept in Tauri's managed state.
///
/// `None` until the async `run_backend()` completes; `Some(Backend)` afterwards.
/// The exit handler takes ownership when the user quits, so we need an
/// `Option` rather than a plain `Backend`.
type BackendSlot = Mutex<Option<Backend>>;

/// Process-wide handle to the Tauri app, populated once in the setup hook.
///
/// Used by code paths that don't naturally receive an `AppHandle` (notably the
/// `restart` tool, which is constructed deep inside the tool registry but needs
/// to trigger `AppHandle::restart()` in desktop mode).
static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Take a clone of the Tauri `AppHandle`, if the desktop runtime is up.
/// Always `None` in headless mode (or before the setup hook has run).
pub fn app_handle() -> Option<tauri::AppHandle> {
    APP_HANDLE.get().cloned()
}

/// Desktop entry point. Builds the Tauri app, spawns the backend on its
/// shared tokio runtime, wires the system-tray menu, and runs the event loop.
pub fn run() -> anyhow::Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "starting skald (desktop mode)");

    // Re-entrancy guard: the first `ExitRequested` triggers async shutdown and
    // then calls `app.exit(0)`, which would itself re-emit `ExitRequested`. The
    // flag short-circuits the second trigger so we actually leave the process.
    let exiting = Arc::new(AtomicBool::new(false));

    tauri::Builder::default()
        // Pre-register the backend slot so it exists before the setup hook
        // (the setup hook spawns the backend async; state must already be there).
        .manage::<BackendSlot>(Mutex::new(None))
        .setup(|app| {
            // Stash the app handle for code paths without a natural handle
            // reference (notably the `restart` tool).
            let _ = APP_HANDLE.set(app.handle().clone());

            build_tray(app)?;

            // Resolve the backend port from config so the webview URL is always
            // in sync with where Axum will actually bind. We load the config
            // sync here just to read the port; the backend task re-loads it
            // (cheap — single YAML parse).
            // In desktop mode this also performs the cwd relocation (no-op in
            // dev, real relocation inside an `.app` bundle).
            let port = match std::panic::catch_unwind(|| {
                crate::config::bootstrap_data_dir()
                    .and_then(|_| Config::load())
                    .map(|c| c.server.port)
            }) {
                Ok(Ok(port)) => port,
                Ok(Err(e)) => {
                    error!(error = %e, "failed to load config for window URL");
                    app.handle().exit(1);
                    return Ok(());
                }
                Err(_) => {
                    error!("config load panicked");
                    app.handle().exit(1);
                    return Ok(());
                }
            };
            let url = format!("http://127.0.0.1:{port}");
            info!(%url, "creating main window");
            let parsed_url = tauri::Url::parse(&url)
                .map_err(tauri::Error::InvalidUrl)?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(parsed_url))
                .title("Skald")
                .inner_size(1200.0, 800.0)
                .min_inner_size(800.0, 600.0)
                .visible(false)
                .build()?;

            let app_handle = app.handle().clone();
            // Spawn the backend on Tauri's shared tokio runtime.
            tauri::async_runtime::spawn(async move {
                match run_backend().await {
                    Ok(backend) => {
                        info!("backend ready — desktop mode");
                        let slot = app_handle.state::<BackendSlot>();
                        *slot.lock().unwrap() = Some(backend);
                        // Reveal the main window now that the backend is serving.
                        if let Some(window) = app_handle.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "backend startup failed");
                        app_handle.exit(1);
                    }
                }
            });
            Ok(())
        })
        // Close button (traffic-light red / X) → hide instead of close.
        // The window stays alive in the tray; only "Quit" terminates.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())?
        .run({
            let exiting = exiting.clone();
            move |app_handle, event| {
                if let RunEvent::ExitRequested { api, .. } = event {
                    // Second trigger (from our own app.exit(0)) — let it proceed.
                    if exiting.swap(true, Ordering::SeqCst) {
                        return;
                    }
                api.prevent_exit();
                let app_handle = app_handle.clone();
                tauri::async_runtime::spawn(async move {
                    // Scope the MutexGuard so it is dropped before any `.await`:
                    // std::sync::MutexGuard is !Send, so holding it across an
                    // await point would make the whole future !Send (Tauri's
                    // runtime requires Send futures).
                    let backend = {
                        let slot = app_handle.state::<BackendSlot>();
                        slot.lock().unwrap().take()
                    };
                    if let Some(backend) = backend {
                        info!("graceful shutdown — desktop mode");
                        shutdown_backend(backend).await;
                        info!("shutdown complete — desktop mode");
                    } else {
                        warn!("exit requested before backend was ready");
                    }
                    // Actually leave the process now.
                    app_handle.exit(0);
                });
                }
            }
        });

    Ok(())
}

/// Build the system-tray icon, its menu (Open / Quit), and the event handlers.
fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItemBuilder::with_id("open", "Open").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&open, &quit]).build()?;

    // Tray icon: reuse the app's bundled window icon for now. On macOS the
    // system auto-recolors template images for the menubar theme; we set
    // `icon_as_template(true)` accordingly. A dedicated monochrome tray PNG
    // (loaded via the right Tauri image API for this version) can replace this
    // later — see the icon sources under `icons/`.
    let icon = app.default_window_icon().cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

    TrayIconBuilder::with_id("main")
        .tooltip("Skald")
        .icon(icon)
        .icon_as_template(true)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.unminimize();
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => {
                // Trigger the graceful path via ExitRequested. The handler in
                // `run()` will drain the backend and then call `exit(0)`.
                app.exit(0);
            }
            _ => (),
        })
        .on_tray_icon_event(|tray, event| {
            // Single left-click toggles the main window (show+focus or hide).
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    if window.is_visible().unwrap_or(false) {
                        let _ = window.hide();
                    } else {
                        let _ = window.unminimize();
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
        })
        .build(app)?;
    Ok(())
}
