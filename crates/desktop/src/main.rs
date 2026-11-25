//! Move Freely desktop application (Tauri v2 + native System Tray / Menu Bar).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use mf_core::{
    init_platform_process, AppConfig, ClusterSnapshot, EngineEvent, EngineHandle, EngineMode,
    MoveFreelyEngine, PairingStatus,
};
use mf_protocol::{BoundaryMode, ClusterTopology, DeviceId, DevicePlacement, Rect};
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};

struct DesktopState {
    engine: EngineHandle,
}

#[derive(Clone, Serialize)]
struct DesktopSnapshot<T> {
    #[serde(flatten)]
    cluster: T,
    monitor_layouts: BTreeMap<String, Vec<Rect>>,
}

fn monitor_layouts(snapshot: &ClusterSnapshot) -> BTreeMap<String, Vec<Rect>> {
    snapshot
        .topology
        .devices
        .iter()
        .map(|device| {
            let displays = snapshot
                .topology
                .global_monitors_for(&device.device_id)
                .into_iter()
                .map(|monitor| Rect {
                    x: monitor.canvas_rect.x - device.placement.x,
                    y: monitor.canvas_rect.y - device.placement.y,
                    ..monitor.canvas_rect
                })
                .collect();
            (device.device_id.to_string(), displays)
        })
        .collect()
}

#[tauri::command]
async fn get_snapshot(
    state: State<'_, DesktopState>,
) -> Result<DesktopSnapshot<ClusterSnapshot>, String> {
    let cluster = state.engine.snapshot().await.map_err(|e| e.to_string())?;
    Ok(DesktopSnapshot {
        monitor_layouts: monitor_layouts(&cluster),
        cluster,
    })
}

#[tauri::command]
async fn open_pairing_window(
    state: State<'_, DesktopState>,
    custom_pin: Option<String>,
) -> Result<PairingStatus, String> {
    state
        .engine
        .open_pairing_window(custom_pin)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn close_pairing_window(state: State<'_, DesktopState>) {
    state.engine.close_pairing_window();
}

#[tauri::command]
fn respond_pair_request(state: State<'_, DesktopState>, request_id: u64, accept: bool) {
    state.engine.respond_pair_request(request_id, accept);
}

#[tauri::command]
async fn pair_with_addr(
    state: State<'_, DesktopState>,
    addr: String,
    expected_id: Option<String>,
    pin: String,
) -> Result<(), String> {
    let parsed: SocketAddr = addr
        .trim()
        .parse()
        .map_err(|e| format!("Invalid address '{addr}': {e}"))?;
    state
        .engine
        .pair_with_addr(parsed, expected_id.map(DeviceId::from), pin)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn connect_addr(
    state: State<'_, DesktopState>,
    peer_id: String,
    addr: String,
) -> Result<(), String> {
    let parsed: SocketAddr = addr
        .trim()
        .parse()
        .map_err(|e| format!("Invalid address '{addr}': {e}"))?;
    state
        .engine
        .connect_addr(DeviceId::from(peer_id), parsed)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn disconnect_peer(state: State<'_, DesktopState>, device_id: String) {
    state.engine.disconnect_peer(DeviceId::from(device_id));
}

#[tauri::command]
fn rename_peer(state: State<'_, DesktopState>, device_id: String, alias: Option<String>) {
    state.engine.rename_peer(DeviceId::from(device_id), alias);
}

#[tauri::command]
fn set_peer_enabled(state: State<'_, DesktopState>, device_id: String, enabled: bool) {
    state
        .engine
        .set_peer_enabled(DeviceId::from(device_id), enabled);
}

#[tauri::command]
fn revoke_peer(state: State<'_, DesktopState>, device_id: String) {
    state.engine.revoke_peer(DeviceId::from(device_id));
}

#[tauri::command]
fn set_controller(state: State<'_, DesktopState>, device_id: String) {
    state.engine.set_controller(DeviceId::from(device_id));
}

#[tauri::command]
fn focus_device(state: State<'_, DesktopState>, device_id: String) {
    state.engine.focus_device(DeviceId::from(device_id));
}

#[tauri::command]
fn emergency_release(state: State<'_, DesktopState>) {
    state.engine.emergency_release();
}

#[tauri::command]
fn set_sharing_enabled(state: State<'_, DesktopState>, enabled: bool) {
    state.engine.set_sharing_enabled(enabled);
}

#[tauri::command]
fn update_placements(
    state: State<'_, DesktopState>,
    placements: Vec<DevicePlacement>,
    boundary_mode: BoundaryMode,
) {
    state.engine.update_placements(placements, boundary_mode);
}

#[tauri::command]
fn snap_placement(
    topology: ClusterTopology,
    moving_id: String,
    x: i32,
    y: i32,
    threshold: i32,
) -> (i32, i32) {
    topology.snap_placement(&DeviceId::from(moving_id), x, y, threshold)
}

#[tauri::command]
fn update_config(state: State<'_, DesktopState>, config: AppConfig) {
    state.engine.update_config(config);
}

#[tauri::command]
fn clear_clipboard_cache(state: State<'_, DesktopState>) {
    state.engine.clear_clipboard_cache();
}

#[tauri::command]
fn refresh_monitors(state: State<'_, DesktopState>) {
    state.engine.refresh_monitors();
}

#[tauri::command]
fn open_permission_settings(state: State<'_, DesktopState>, kind: String) {
    state.engine.open_permission_settings(kind);
}

fn device_name_for(snap: &ClusterSnapshot, id: &DeviceId) -> String {
    if &snap.local.device_id == id {
        snap.local.name.clone()
    } else {
        snap.peers
            .iter()
            .find(|p| &p.device_id == id)
            .map(|p| p.display_name.clone())
            .unwrap_or_else(|| id.short().to_string())
    }
}

fn build_tray_menu(
    app: &AppHandle,
    snap: Option<&ClusterSnapshot>,
) -> tauri::Result<Menu<tauri::Wry>> {
    let mut builder = MenuBuilder::new(app);

    if let Some(s) = snap {
        let ctrl_name = device_name_for(s, &s.active_controller);
        let focus_name = device_name_for(s, &s.focused_device);
        let status_label = if s.sharing_enabled {
            format!("Controller: {ctrl_name} • Input → {focus_name}")
        } else {
            "Sharing Paused".to_string()
        };
        let status_item = MenuItemBuilder::with_id("status", status_label)
            .enabled(false)
            .build(app)?;
        builder = builder.item(&status_item).separator();

        let toggle_label = if s.sharing_enabled {
            "Pause Sharing"
        } else {
            "Resume Sharing"
        };
        let toggle = MenuItemBuilder::with_id("toggle-sharing", toggle_label).build(app)?;
        let emergency = MenuItemBuilder::with_id("emergency-release", "Emergency Release Control")
            .build(app)?;
        builder = builder.item(&toggle).item(&emergency).separator();

        // Switch Active Controller submenu
        let mut ctrl_sub = SubmenuBuilder::new(app, "Switch Active Controller");
        let local_ctrl = CheckMenuItemBuilder::with_id(
            format!("ctrl:{}", s.local.device_id),
            format!("{} (This Computer)", s.local.name),
        )
        .checked(s.active_controller == s.local.device_id)
        .build(app)?;
        ctrl_sub = ctrl_sub.item(&local_ctrl);

        for p in s.peers.iter().filter(|p| p.connected && p.enabled) {
            let item =
                CheckMenuItemBuilder::with_id(format!("ctrl:{}", p.device_id), &p.display_name)
                    .checked(s.active_controller == p.device_id)
                    .build(app)?;
            ctrl_sub = ctrl_sub.item(&item);
        }
        builder = builder.item(&ctrl_sub.build()?);

        // Connected Devices submenu
        let mut dev_sub = SubmenuBuilder::new(app, "Connected Devices");
        let connected: Vec<_> = s.peers.iter().filter(|p| p.connected).collect();
        if connected.is_empty() {
            let empty = MenuItemBuilder::with_id("none", "No connected devices")
                .enabled(false)
                .build(app)?;
            dev_sub = dev_sub.item(&empty);
        } else {
            for p in connected {
                let mut p_sub =
                    SubmenuBuilder::new(app, format!("{} ({} ms)", p.display_name, p.latency_ms));
                let focus_item =
                    MenuItemBuilder::with_id(format!("focus:{}", p.device_id), "Send Input Here")
                        .build(app)?;
                let disc_item =
                    MenuItemBuilder::with_id(format!("disc:{}", p.device_id), "Disconnect")
                        .build(app)?;
                p_sub = p_sub.item(&focus_item).item(&disc_item);
                dev_sub = dev_sub.item(&p_sub.build()?);
            }
        }
        builder = builder.item(&dev_sub.build()?).separator();
    }

    let open = MenuItemBuilder::with_id("open-window", "Open Move Freely Settings").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit Move Freely").build(app)?;
    builder.item(&open).item(&quit).build()
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn main() {
    init_platform_process();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mf=info,mf_core=info,mf_desktop=info".into()),
        )
        .init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let engine_runtime = rt.handle().clone();

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            open_pairing_window,
            close_pairing_window,
            respond_pair_request,
            pair_with_addr,
            connect_addr,
            disconnect_peer,
            rename_peer,
            set_peer_enabled,
            revoke_peer,
            set_controller,
            focus_device,
            emergency_release,
            set_sharing_enabled,
            update_placements,
            snap_placement,
            update_config,
            clear_clipboard_cache,
            refresh_monitors,
            open_permission_settings,
        ])
        .on_window_event(|window, event| {
            // Keep background sharing & tray active when closing the window.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            // On Windows, Tauri's event loop registers the process-wide Raw
            // Input mouse receiver. Start our engine after that registration.
            let (engine, _) =
                engine_runtime.block_on(MoveFreelyEngine::start(None, None, EngineMode::Native))?;
            let mut rx = engine.subscribe();
            let initial_snap = engine_runtime.block_on(engine.snapshot()).ok();
            let engine_for_events = engine.clone();
            app.manage(DesktopState { engine });

            let handle = app.handle().clone();
            let tray_menu = build_tray_menu(&handle, initial_snap.as_ref())?;
            let tray_icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

            let tray: TrayIcon = TrayIconBuilder::with_id("main-tray")
                .icon(tray_icon)
                .tooltip("Move Freely")
                .menu(&tray_menu)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .on_menu_event(move |app, ev| {
                    let id = ev.id().as_ref();
                    if id == "open-window" {
                        show_main_window(app);
                        let _ = app.emit("mf://open-settings", ());
                    } else if id == "quit" {
                        engine_for_events.shutdown();
                        app.exit(0);
                    } else if id == "emergency-release" {
                        engine_for_events.emergency_release();
                    } else if id == "toggle-sharing" {
                        let eng = engine_for_events.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Ok(s) = eng.snapshot().await {
                                eng.set_sharing_enabled(!s.sharing_enabled);
                            }
                        });
                    } else if let Some(dev_id) = id.strip_prefix("ctrl:") {
                        engine_for_events.set_controller(DeviceId::from(dev_id));
                    } else if let Some(dev_id) = id.strip_prefix("focus:") {
                        engine_for_events.focus_device(DeviceId::from(dev_id));
                    } else if let Some(dev_id) = id.strip_prefix("disc:") {
                        engine_for_events.disconnect_peer(DeviceId::from(dev_id));
                    }
                })
                .build(app)?;

            let tray_arc = Arc::new(tray);
            let app_handle = app.handle().clone();

            tauri::async_runtime::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    match &ev {
                        EngineEvent::Snapshot(snap) => {
                            let _ = app_handle.emit(
                                "mf://snapshot",
                                DesktopSnapshot {
                                    cluster: snap,
                                    monitor_layouts: monitor_layouts(snap),
                                },
                            );
                            if let Ok(menu) = build_tray_menu(&app_handle, Some(snap)) {
                                let _ = tray_arc.set_menu(Some(menu));
                            }
                        }
                        EngineEvent::IncomingPair(req) => {
                            let _ = app_handle.emit("mf://incoming-pair", req);
                            show_main_window(&app_handle);
                        }
                        EngineEvent::Notification { level, message } => {
                            let _ = app_handle.emit(
                                "mf://notification",
                                serde_json::json!({ "level": level, "message": message }),
                            );
                        }
                    }
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error running Move Freely desktop app");
}
