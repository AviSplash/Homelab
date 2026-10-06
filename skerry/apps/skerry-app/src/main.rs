//! Skerry desktop app: the engine plus a settings window and a tray icon.

#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use serde::Serialize;
use skerry_core::clipboard::NullClipboard;
use skerry_core::config::Paths;
use skerry_core::engine::{
    self, Backends, EngineEvent, EngineHandle, EngineOptions, FocusView, PairTarget, SettingsUpdate, Snapshot,
};
use skerry_core::geometry::Edge;
use skerry_core::input::{BackendStatus, NullCapture, NullEmulation};
use skerry_core::keys::OsKind;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

struct AppState {
    engine: EngineHandle,
    backend: String,
    config_path: String,
}

#[derive(Serialize)]
struct Info {
    backend: String,
    config_path: String,
}

#[tauri::command]
fn get_state(state: State<'_, AppState>) -> Snapshot {
    state.engine.snapshot()
}

#[tauri::command]
fn get_info(state: State<'_, AppState>) -> Info {
    Info { backend: state.backend.clone(), config_path: state.config_path.clone() }
}

#[tauri::command]
async fn pair(state: State<'_, AppState>, target: PairTarget) -> Result<u64, String> {
    state.engine.pair(target).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn submit_code(state: State<'_, AppState>, session: u64, code: String) {
    state.engine.submit_code(session, &code);
}

#[tauri::command]
fn cancel_pairing(state: State<'_, AppState>, session: u64) {
    state.engine.cancel_pairing(session);
}

#[tauri::command]
fn set_layout(state: State<'_, AppState>, edge: Edge, peer: Option<String>) {
    state.engine.set_layout(edge, peer);
}

#[tauri::command]
fn forget(state: State<'_, AppState>, id: String) {
    state.engine.forget(&id);
}

#[tauri::command]
fn update_settings(state: State<'_, AppState>, settings: SettingsUpdate) {
    state.engine.update_settings(settings);
}

#[tauri::command]
fn set_speed(state: State<'_, AppState>, id: String, speed: f64) {
    state.engine.set_speed(&id, speed);
}

#[tauri::command]
fn add_manual_peer(state: State<'_, AppState>, addr: String) {
    state.engine.add_manual_peer(&addr);
}

#[tauri::command]
fn remove_manual_peer(state: State<'_, AppState>, addr: String) {
    state.engine.remove_manual_peer(&addr);
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    let al = app.autolaunch();
    if enabled { al.enable() } else { al.disable() }.map_err(|e| e.to_string())
}

/// Open the OS page where the user grants input permissions.
#[tauri::command]
fn open_permission_settings() {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .spawn();
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Backends that do nothing, used when the platform could not be set up so
/// the window can still explain what went wrong.
fn fallback_backends(reason: String) -> Backends {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    Backends {
        os: OsKind::current(),
        capture: Box::new(NullCapture { displays: vec![], status: BackendStatus::Error(reason.clone()) }),
        capture_events: rx,
        emulation: Box::new(NullEmulation { status: BackendStatus::Error(reason) }),
        screen: Box::new(Vec::new),
        clipboard: Box::new(NullClipboard),
    }
}

fn tray_text(s: &Snapshot) -> (String, &'static str) {
    let name = |id: &str| s.peers.iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_default();
    let tip = match &s.focus {
        _ if !s.settings.enabled => "Skerry: paused".to_string(),
        FocusView::Local => {
            let n = s.peers.iter().filter(|p| p.online).count();
            format!("Skerry: {n} computer{} connected", if n == 1 { "" } else { "s" })
        }
        FocusView::Controlling(p) => format!("Skerry: controlling {}", name(p)),
        FocusView::ControlledBy(p) => format!("Skerry: controlled by {}", name(p)),
    };
    (tip, if s.settings.enabled { "Pause sharing" } else { "Resume sharing" })
}

fn main() {
    skerry_platform::init_process();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tao=warn,wry=warn".into()),
        )
        .init();

    let minimized = std::env::args().any(|a| a == "--minimized");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--minimized"])))
        .setup(move |app| {
            let paths = Paths::default_location()?;
            let config_path = paths.config.display().to_string();
            let (engine, backend) = tauri::async_runtime::block_on(async move {
                let (backends, backend) = match skerry_platform::backends().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("input backends unavailable: {e:#}");
                        (fallback_backends(format!("{e:#}")), "unavailable".to_string())
                    }
                };
                let engine = engine::start(EngineOptions::new(paths), backends).await?;
                anyhow::Ok((engine, backend))
            })?;
            tracing::info!("Skerry started ({backend})");

            // Tray icon.
            let open = MenuItem::with_id(app, "open", "Open Skerry", true, None::<&str>)?;
            let toggle = MenuItem::with_id(app, "toggle", "Pause sharing", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Skerry", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &toggle, &sep, &quit])?;
            let mut tray = TrayIconBuilder::with_id("skerry")
                .menu(&menu)
                .tooltip("Skerry")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "open" => show_main(app),
                    "toggle" => {
                        let st = app.state::<AppState>();
                        let enabled = st.engine.snapshot().settings.enabled;
                        st.engine.update_settings(SettingsUpdate { enabled: Some(!enabled), ..Default::default() });
                    }
                    "quit" => {
                        let engine = app.state::<AppState>().engine.clone();
                        tauri::async_runtime::block_on(engine.shutdown());
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left, button_state: MouseButtonState::Up, ..
                    } = ev
                    {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            // Forward engine events to the window and keep the tray current.
            let handle = app.handle().clone();
            let mut events = engine.subscribe();
            let toggle_item: MenuItem<Wry> = toggle.clone();
            tauri::async_runtime::spawn(async move {
                let mut announced = std::collections::HashSet::new();
                loop {
                    match events.recv().await {
                        Ok(ev) => {
                            if let EngineEvent::State(s) = &ev {
                                let (tip, label) = tray_text(s);
                                if let Some(t) = handle.tray_by_id("skerry") {
                                    let _ = t.set_tooltip(Some(&tip));
                                }
                                let _ = toggle_item.set_text(label);
                                // Someone wants to pair: bring the window up so the code is visible.
                                for p in &s.pairings {
                                    if p.stage == "show_code" && announced.insert(p.session) {
                                        show_main(&handle);
                                    }
                                }
                            }
                            let _ = handle.emit("engine", &ev);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });

            app.manage(AppState { engine, backend, config_path });
            if !minimized {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps Skerry running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            get_info,
            pair,
            submit_code,
            cancel_pairing,
            set_layout,
            forget,
            update_settings,
            set_speed,
            add_manual_peer,
            remove_manual_peer,
            get_autostart,
            set_autostart,
            open_permission_settings,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Skerry");

    app.run(|app, event| {
        #[cfg(target_os = "macos")]
        if let RunEvent::Reopen { .. } = event {
            show_main(app);
        }
        if let RunEvent::ExitRequested { api, code, .. } = &event {
            // Only quit from the tray menu; closing the last window must not.
            if code.is_none() {
                api.prevent_exit();
            }
        }
        let _ = app;
    });
}
