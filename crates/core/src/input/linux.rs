//! Native Linux input backends:
//! - X11: pure-Rust `x11rb` (XRandR display enumeration, XInput2 capture/grab, XTest injection)
//! - Wayland: XDG Desktop Portal (`ashpd` RemoteDesktop + `reis` libei) with explicit permission diagnostics

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use mf_protocol::{Button, KeyCode, ModifierFlags, Monitor};
use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _, DeviceUse};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, GrabMode};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event as XEvent;
use x11rb::rust_connection::RustConnection;

use super::{
    CaptureController, CaptureSession, HeldInputTracker, InputEvent, ModifierStateTracker,
    PermissionStatus, PlatformInjector,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSessionKind {
    X11,
    Wayland,
    Headless,
}

pub fn detect_session() -> LinuxSessionKind {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|s| s.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false);
    let x11 = std::env::var_os("DISPLAY").is_some();
    if wayland {
        LinuxSessionKind::Wayland
    } else if x11 {
        LinuxSessionKind::X11
    } else {
        LinuxSessionKind::Headless
    }
}

pub fn check_permissions() -> PermissionStatus {
    match detect_session() {
        LinuxSessionKind::X11 => {
            let can_connect = x11rb::connect(None).is_ok();
            PermissionStatus {
                accessibility_granted: can_connect,
                input_monitoring_granted: can_connect,
                can_capture: can_connect,
                can_inject: can_connect,
                session_type: "x11".into(),
                summary: if can_connect {
                    "X11 display connection ready (XInput2 + XTest)".into()
                } else {
                    "Failed connecting to X11 DISPLAY".into()
                },
                action_hint: if can_connect {
                    None
                } else {
                    Some("Ensure DISPLAY is set and X11 access is authorized (xhost / XAUTHORITY)".into())
                },
            }
        }
        LinuxSessionKind::Wayland => PermissionStatus {
            accessibility_granted: true,
            input_monitoring_granted: false,
            can_capture: false,
            can_inject: true,
            session_type: "wayland".into(),
            summary: "Wayland session detected: remote control injection uses XDG RemoteDesktop/libei portal; global pointer capture on Wayland requires an X11 session or compositor InputCapture portal support".into(),
            action_hint: Some(
                "This Wayland device can receive control cleanly via XDG RemoteDesktop portal. To act as controller, switch to an X11 session or use a compositor supporting InputCapture portal.".into(),
            ),
        },
        LinuxSessionKind::Headless => PermissionStatus {
            accessibility_granted: false,
            input_monitoring_granted: false,
            can_capture: false,
            can_inject: false,
            session_type: "headless".into(),
            summary: "No DISPLAY or WAYLAND_DISPLAY environment detected".into(),
            action_hint: Some("Start Move Freely inside an X11 or Wayland desktop session".into()),
        },
    }
}

pub fn enumerate_monitors() -> Vec<Monitor> {
    if let Ok((conn, screen_idx)) = x11rb::connect(None) {
        let root = conn.setup().roots[screen_idx].root;
        let scale = detect_linux_scale_factor();
        if let Ok(cookie) = conn.randr_get_monitors(root, true) {
            if let Ok(reply) = cookie.reply() {
                let mut out = Vec::new();
                for (i, m) in reply.monitors.into_iter().enumerate() {
                    let w = (m.width as u32).max(1);
                    let h = (m.height as u32).max(1);
                    let name = conn
                        .get_atom_name(m.name)
                        .ok()
                        .and_then(|c| c.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.name).to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| format!("Monitor {}", i + 1));
                    out.push(Monitor {
                        id: format!("xrandr-{i}-{name}"),
                        name,
                        x: m.x as i32,
                        y: m.y as i32,
                        width: w,
                        height: h,
                        physical_width: w,
                        physical_height: h,
                        scale_factor: scale,
                        is_primary: m.primary || i == 0,
                    });
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
        let s = &conn.setup().roots[screen_idx];
        return vec![Monitor {
            id: "x11-root".into(),
            name: "X11 Display".into(),
            x: 0,
            y: 0,
            width: s.width_in_pixels as u32,
            height: s.height_in_pixels as u32,
            physical_width: s.width_in_pixels as u32,
            physical_height: s.height_in_pixels as u32,
            scale_factor: scale,
            is_primary: true,
        }];
    }
    vec![super::default_fallback_monitor()]
}

fn detect_linux_scale_factor() -> f64 {
    if let Ok(val) = std::env::var("GDK_SCALE") {
        if let Ok(f) = val.parse::<f64>() {
            if f >= 0.5 {
                return f;
            }
        }
    }
    if let Ok(val) = std::env::var("QT_SCALE_FACTOR") {
        if let Ok(f) = val.parse::<f64>() {
            if f >= 0.5 {
                return f;
            }
        }
    }
    1.0
}

// ---------------------------------------------------------------------------
// X11 Capture + Grab via XInput2
// ---------------------------------------------------------------------------

struct X11CaptureController {
    grabbing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    cmd_tx: std::sync::mpsc::Sender<CaptureCmd>,
}

enum CaptureCmd {
    SetGrab(bool, Option<(i32, i32)>),
    Stop,
}

impl CaptureController for X11CaptureController {
    fn set_grabbing(&self, on: bool, warp_to: Option<(i32, i32)>) {
        self.grabbing.store(on, Ordering::SeqCst);
        let _ = self.cmd_tx.send(CaptureCmd::SetGrab(on, warp_to));
    }

    fn is_grabbing(&self) -> bool {
        self.grabbing.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(CaptureCmd::Stop);
    }
}

impl Drop for X11CaptureController {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start_capture() -> Result<CaptureSession> {
    match detect_session() {
        LinuxSessionKind::Wayland => {
            anyhow::bail!(
                "Global input capture on Wayland is restricted by the compositor unless InputCapture portal is available; run an X11 session to act as controller (Wayland injection as a controlled device is supported)."
            );
        }
        LinuxSessionKind::Headless => {
            anyhow::bail!("No X11 DISPLAY available for Linux input capture");
        }
        LinuxSessionKind::X11 => start_x11_capture(),
    }
}

fn start_x11_capture() -> Result<CaptureSession> {
    let (conn, screen_idx) = x11rb::connect(None).context("connecting to X11")?;
    let root = conn.setup().roots[screen_idx].root;
    let screen_w = conn.setup().roots[screen_idx].width_in_pixels as i32;
    let screen_h = conn.setup().roots[screen_idx].height_in_pixels as i32;

    // Query XInput2 support
    conn.xinput_xi_query_version(2, 2)?.reply()?;

    // Identify XTest slave device IDs so injected events never loop back.
    let mut xtest_devices = std::collections::HashSet::new();
    if let Ok(devs) = conn.xinput_xi_query_device(xinput::Device::ALL)?.reply() {
        for d in devs.infos {
            let name = String::from_utf8_lossy(&d.name);
            if name.contains("XTEST") {
                xtest_devices.insert(d.deviceid);
            }
        }
    }

    let mask = xinput::XIEventMask::RAW_MOTION
        | xinput::XIEventMask::RAW_BUTTON_PRESS
        | xinput::XIEventMask::RAW_BUTTON_RELEASE
        | xinput::XIEventMask::RAW_KEY_PRESS
        | xinput::XIEventMask::RAW_KEY_RELEASE;
    conn.xinput_xi_select_events(
        root,
        &[xinput::EventMask {
            deviceid: xinput::Device::ALL_MASTER.into(),
            mask: vec![mask],
        }],
    )?;
    conn.flush()?;

    let (tx, rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<CaptureCmd>();
    let grabbing = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let grab_flag = grabbing.clone();
    let stop_flag = stop.clone();

    std::thread::Builder::new()
        .name("mf-x11-capture".into())
        .spawn(move || {
            let mut mods = ModifierStateTracker::default();
            let mut last_warp = Instant::now();
            let center_x = screen_w / 2;
            let center_y = screen_h / 2;
            let mut active_grab = false;

            while !stop_flag.load(Ordering::SeqCst) {
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        CaptureCmd::SetGrab(on, warp_to) => {
                            if on && !active_grab {
                                let pmask = EventMask::BUTTON_PRESS
                                    | EventMask::BUTTON_RELEASE
                                    | EventMask::POINTER_MOTION;
                                let _ = conn.grab_pointer(
                                    false,
                                    root,
                                    pmask,
                                    GrabMode::ASYNC,
                                    GrabMode::ASYNC,
                                    root,
                                    x11rb::NONE,
                                    x11rb::CURRENT_TIME,
                                );
                                let _ = conn.grab_keyboard(
                                    false,
                                    root,
                                    x11rb::CURRENT_TIME,
                                    GrabMode::ASYNC,
                                    GrabMode::ASYNC,
                                );
                                let _ = conn.warp_pointer(
                                    x11rb::NONE,
                                    root,
                                    0,
                                    0,
                                    0,
                                    0,
                                    center_x as i16,
                                    center_y as i16,
                                );
                                let _ = conn.flush();
                                active_grab = true;
                            } else if !on && active_grab {
                                let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
                                let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
                                if let Some((wx, wy)) = warp_to {
                                    let _ = conn.warp_pointer(
                                        x11rb::NONE,
                                        root,
                                        0,
                                        0,
                                        0,
                                        0,
                                        wx as i16,
                                        wy as i16,
                                    );
                                }
                                let _ = conn.flush();
                                active_grab = false;
                            }
                        }
                        CaptureCmd::Stop => {
                            if active_grab {
                                let _ = conn.ungrab_pointer(x11rb::CURRENT_TIME);
                                let _ = conn.ungrab_keyboard(x11rb::CURRENT_TIME);
                                let _ = conn.flush();
                            }
                            return;
                        }
                    }
                }

                // Poll X11 socket non-blockingly with brief sleep when idle
                while let Ok(Some(ev)) = conn.poll_for_event() {
                    let is_grabbing = grab_flag.load(Ordering::SeqCst);
                    match ev {
                        XEvent::XinputRawMotion(rm) => {
                            if xtest_devices.contains(&rm.sourceid) {
                                continue;
                            }
                            let (dx, dy) = extract_raw_xy(&rm.valuator_mask, &rm.axisvalues_raw);
                            if dx == 0 && dy == 0 {
                                continue;
                            }
                            if is_grabbing {
                                let _ = tx.send(InputEvent::RemotePointerDelta { dx, dy });
                                if last_warp.elapsed().as_millis() > 25 {
                                    let _ = conn.warp_pointer(
                                        x11rb::NONE,
                                        root,
                                        0,
                                        0,
                                        0,
                                        0,
                                        center_x as i16,
                                        center_y as i16,
                                    );
                                    let _ = conn.flush();
                                    last_warp = Instant::now();
                                }
                            } else if let Ok(Ok(qp)) = conn.query_pointer(root).map(|c| c.reply()) {
                                let _ = tx.send(InputEvent::LocalPointerMove {
                                    x: qp.root_x as i32,
                                    y: qp.root_y as i32,
                                    dx,
                                    dy,
                                });
                            }
                        }
                        XEvent::XinputRawButtonPress(rb) => {
                            if xtest_devices.contains(&rb.sourceid) || !is_grabbing {
                                continue;
                            }
                            handle_x11_button(rb.detail, true, &tx);
                        }
                        XEvent::XinputRawButtonRelease(rb) => {
                            if xtest_devices.contains(&rb.sourceid) || !is_grabbing {
                                continue;
                            }
                            handle_x11_button(rb.detail, false, &tx);
                        }
                        XEvent::XinputRawKeyPress(rk) => {
                            if xtest_devices.contains(&rk.sourceid) {
                                continue;
                            }
                            let key = evdev_keycode_to_protocol(rk.detail.saturating_sub(8));
                            mods.update(key, true);
                            let _ = tx.send(InputEvent::Key {
                                key,
                                pressed: true,
                                text: None,
                                modifiers: mods.flags(),
                            });
                        }
                        XEvent::XinputRawKeyRelease(rk) => {
                            if xtest_devices.contains(&rk.sourceid) {
                                continue;
                            }
                            let key = evdev_keycode_to_protocol(rk.detail.saturating_sub(8));
                            mods.update(key, false);
                            let _ = tx.send(InputEvent::Key {
                                key,
                                pressed: false,
                                text: None,
                                modifiers: mods.flags(),
                            });
                        }
                        _ => {}
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        })?;

    Ok(CaptureSession {
        events: rx,
        controller: Arc::new(X11CaptureController {
            grabbing,
            stop,
            cmd_tx,
        }),
    })
}

fn handle_x11_button(detail: u32, pressed: bool, tx: &mpsc::UnboundedSender<InputEvent>) {
    match detail {
        1 => {
            let _ = tx.send(InputEvent::MouseButton {
                button: Button::Left,
                pressed,
            });
        }
        2 => {
            let _ = tx.send(InputEvent::MouseButton {
                button: Button::Middle,
                pressed,
            });
        }
        3 => {
            let _ = tx.send(InputEvent::MouseButton {
                button: Button::Right,
                pressed,
            });
        }
        4 if pressed => {
            let _ = tx.send(InputEvent::Scroll {
                dx: 0.0,
                dy: 1.0,
                continuous: false,
            });
        }
        5 if pressed => {
            let _ = tx.send(InputEvent::Scroll {
                dx: 0.0,
                dy: -1.0,
                continuous: false,
            });
        }
        6 if pressed => {
            let _ = tx.send(InputEvent::Scroll {
                dx: -1.0,
                dy: 0.0,
                continuous: false,
            });
        }
        7 if pressed => {
            let _ = tx.send(InputEvent::Scroll {
                dx: 1.0,
                dy: 0.0,
                continuous: false,
            });
        }
        8 => {
            let _ = tx.send(InputEvent::MouseButton {
                button: Button::Back,
                pressed,
            });
        }
        9 => {
            let _ = tx.send(InputEvent::MouseButton {
                button: Button::Forward,
                pressed,
            });
        }
        _ => {}
    }
}

fn extract_raw_xy(mask: &[u32], values: &[xinput::Fp3232]) -> (i32, i32) {
    let mut dx = 0i32;
    let mut dy = 0i32;
    let mut idx = 0usize;
    let bits = mask.first().copied().unwrap_or(0);
    if (bits & 1) != 0 {
        if let Some(v) = values.get(idx) {
            dx = v.integral;
        }
        idx += 1;
    }
    if (bits & 2) != 0 {
        if let Some(v) = values.get(idx) {
            dy = v.integral;
        }
    }
    (dx, dy)
}

// ---------------------------------------------------------------------------
// Linux Injectors: X11 (XTest) & Wayland (ashpd RemoteDesktop + reis/libei)
// ---------------------------------------------------------------------------

pub fn new_injector() -> Result<Box<dyn PlatformInjector>> {
    match detect_session() {
        LinuxSessionKind::Wayland => {
            match WaylandEiInjector::new() {
                Ok(inj) => Ok(Box::new(inj)),
                Err(e) => {
                    // Fallback to XWayland if DISPLAY exists, surfacing clear context.
                    if std::env::var_os("DISPLAY").is_some() {
                        tracing::warn!(error = %e, "Wayland libei portal unavailable; falling back to X11/XWayland XTest");
                        Ok(Box::new(X11Injector::new()?))
                    } else {
                        Err(e)
                    }
                }
            }
        }
        LinuxSessionKind::X11 => Ok(Box::new(X11Injector::new()?)),
        LinuxSessionKind::Headless => anyhow::bail!("No X11 or Wayland session available"),
    }
}

pub struct X11Injector {
    conn: RustConnection,
    root: xproto::Window,
    pointer_id: u8,
    keyboard_id: u8,
    held: HeldInputTracker,
}

impl X11Injector {
    pub fn new() -> Result<Self> {
        let (conn, screen_idx) = x11rb::connect(None).context("connecting to X11 DISPLAY")?;
        let root = conn.setup().roots[screen_idx].root;
        let devs = conn.xinput_list_input_devices()?.reply()?;
        let pointer_id = devs
            .devices
            .iter()
            .find(|d| d.device_use == DeviceUse::IS_X_POINTER)
            .map(|d| d.device_id)
            .unwrap_or(0);
        let keyboard_id = devs
            .devices
            .iter()
            .find(|d| d.device_use == DeviceUse::IS_X_KEYBOARD)
            .map(|d| d.device_id)
            .unwrap_or(0);
        Ok(Self {
            conn,
            root,
            pointer_id,
            keyboard_id,
            held: HeldInputTracker::default(),
        })
    }
}

impl Drop for X11Injector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

impl PlatformInjector for X11Injector {
    fn inject_pointer_abs(&mut self, x: i32, y: i32, _dx: i32, _dy: i32) -> Result<()> {
        self.conn.warp_pointer(
            x11rb::NONE,
            self.root,
            0,
            0,
            0,
            0,
            x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn inject_pointer_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.conn.xtest_fake_input(
            xproto::MOTION_NOTIFY_EVENT,
            1,
            x11rb::CURRENT_TIME,
            x11rb::NONE,
            dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            self.pointer_id,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.held.record_button(button, pressed);
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
            Button::Back => 8,
            Button::Forward => 9,
            Button::Other(n) => n,
        };
        let ev_type = if pressed {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.conn.xtest_fake_input(
            ev_type,
            detail,
            x11rb::CURRENT_TIME,
            self.root,
            0,
            0,
            self.pointer_id,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64, _continuous: bool) -> Result<()> {
        let v_steps = dy.round() as i32;
        if v_steps != 0 {
            let btn = if v_steps > 0 { 4 } else { 5 };
            for _ in 0..v_steps.abs().min(20) {
                self.conn.xtest_fake_input(
                    xproto::BUTTON_PRESS_EVENT,
                    btn,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    self.pointer_id,
                )?;
                self.conn.xtest_fake_input(
                    xproto::BUTTON_RELEASE_EVENT,
                    btn,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    self.pointer_id,
                )?;
            }
        }
        let h_steps = dx.round() as i32;
        if h_steps != 0 {
            let btn = if h_steps > 0 { 7 } else { 6 };
            for _ in 0..h_steps.abs().min(20) {
                self.conn.xtest_fake_input(
                    xproto::BUTTON_PRESS_EVENT,
                    btn,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    self.pointer_id,
                )?;
                self.conn.xtest_fake_input(
                    xproto::BUTTON_RELEASE_EVENT,
                    btn,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    self.pointer_id,
                )?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn inject_key(
        &mut self,
        key: KeyCode,
        pressed: bool,
        _text: Option<&str>,
        _modifiers: ModifierFlags,
    ) -> Result<()> {
        self.held.record_key(key, pressed);
        let ev_type = if pressed {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        if let Some(evdev_code) = protocol_to_evdev_keycode(key) {
            let x11_code = (evdev_code + 8) as u8;
            self.conn.xtest_fake_input(
                ev_type,
                x11_code,
                x11rb::CURRENT_TIME,
                self.root,
                0,
                0,
                self.keyboard_id,
            )?;
            self.conn.flush()?;
        }
        Ok(())
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        self.inject_pointer_abs(x, y, 0, 0)
    }

    fn release_all(&mut self) -> Result<()> {
        let (buttons, keys) = self.held.drain_release_order();
        for b in buttons {
            let _ = self.inject_button(b, false);
        }
        for k in keys {
            let _ = self.inject_key(k, false, None, ModifierFlags::default());
        }
        Ok(())
    }
}

// Wayland injector using XDG Desktop Portal RemoteDesktop + libei (`reis`)
pub struct WaylandEiInjector {
    context: reis::ei::Context,
    connection: reis::ei::Connection,
    pointer: Option<(reis::ei::Device, reis::ei::Pointer)>,
    pointer_abs: Option<(reis::ei::Device, reis::ei::PointerAbsolute)>,
    button: Option<(reis::ei::Device, reis::ei::Button)>,
    scroll: Option<(reis::ei::Device, reis::ei::Scroll)>,
    keyboard: Option<(reis::ei::Device, reis::ei::Keyboard)>,
    sequence: u32,
    held: HeldInputTracker,
}

unsafe impl Send for WaylandEiInjector {}

impl WaylandEiInjector {
    pub fn new() -> Result<Self> {
        let rt = tokio::runtime::Runtime::new()?;
        let context = rt.block_on(async {
            if let Some(ctx) = reis::ei::Context::connect_to_env()? {
                return Ok::<_, anyhow::Error>(ctx);
            }
            use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
            use ashpd::desktop::PersistMode;
            let rd = RemoteDesktop::new().await?;
            let session = rd.create_session().await?;
            rd.select_devices(
                &session,
                DeviceType::Keyboard | DeviceType::Pointer,
                None,
                PersistMode::Application,
            )
            .await?;
            rd.start(&session, None).await?;
            let fd = rd.connect_to_eis(&session).await?;
            let stream = UnixStream::from(fd);
            stream.set_nonblocking(true)?;
            Ok(reis::ei::Context::new(stream)?)
        })?;

        let resp = reis::handshake::ei_handshake_blocking(
            &context,
            "move-freely",
            reis::ei::handshake::ContextType::Sender,
        )
        .map_err(|e| anyhow!("libei handshake failed: {e:?}"))?;
        context.flush()?;

        let mut inj = Self {
            context,
            connection: resp.connection,
            pointer: None,
            pointer_abs: None,
            button: None,
            scroll: None,
            keyboard: None,
            sequence: resp.serial.wrapping_add(1),
            held: HeldInputTracker::default(),
        };
        inj.pump_events()?;
        Ok(inj)
    }

    fn pump_events(&mut self) -> Result<()> {
        let mut seats: HashMap<reis::ei::Seat, u64> = HashMap::new();
        let mut devices: HashMap<reis::ei::Device, HashMap<String, reis::Object>> = HashMap::new();
        let mut started = false;

        for _ in 0..10 {
            let _ = self.context.read();
            while let Some(res) = self.context.pending_event() {
                if let reis::PendingRequestResult::Request(ev) = res {
                    match ev {
                        reis::ei::Event::Connection(_, reis::ei::connection::Event::Seat { seat }) => {
                            seats.insert(seat, 0);
                        }
                        reis::ei::Event::Seat(seat, req) => match req {
                            reis::ei::seat::Event::Capability { mask, .. } => {
                                *seats.entry(seat).or_insert(0) |= mask;
                            }
                            reis::ei::seat::Event::Done => {
                                if let Some(mask) = seats.get(&seat) {
                                    seat.bind(*mask);
                                }
                            }
                            reis::ei::seat::Event::Device { device } => {
                                devices.insert(device, HashMap::new());
                            }
                            _ => {}
                        },
                        reis::ei::Event::Device(device, req) => {
                            let map = devices.entry(device.clone()).or_default();
                            match req {
                                reis::ei::device::Event::Interface { object } => {
                                    map.insert(object.interface().to_string(), object);
                                }
                                reis::ei::device::Event::Resumed { serial } => {
                                    device.start_emulating(self.sequence, serial);
                                    self.sequence = self.sequence.wrapping_add(1);
                                    started = true;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
            }
            let _ = self.context.flush();
            if started {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }

        for (dev, map) in devices {
            if let Some(p) = map.get("ei_pointer").and_then(|o| o.clone().downcast()) {
                self.pointer = Some((dev.clone(), p));
            }
            if let Some(pa) = map
                .get("ei_pointer_absolute")
                .and_then(|o| o.clone().downcast())
            {
                self.pointer_abs = Some((dev.clone(), pa));
            }
            if let Some(b) = map.get("ei_button").and_then(|o| o.clone().downcast()) {
                self.button = Some((dev.clone(), b));
            }
            if let Some(s) = map.get("ei_scroll").and_then(|o| o.clone().downcast()) {
                self.scroll = Some((dev.clone(), s));
            }
            if let Some(k) = map.get("ei_keyboard").and_then(|o| o.clone().downcast()) {
                self.keyboard = Some((dev.clone(), k));
            }
        }
        Ok(())
    }

    fn flush_dev(&mut self, dev: &reis::ei::Device) -> Result<()> {
        dev.frame(self.sequence, 0);
        self.sequence = self.sequence.wrapping_add(1);
        self.context.flush()?;
        Ok(())
    }
}

impl Drop for WaylandEiInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
        self.connection.disconnect();
        let _ = self.context.flush();
    }
}

impl PlatformInjector for WaylandEiInjector {
    fn inject_pointer_abs(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        if let Some((dev, pa)) = self.pointer_abs.clone() {
            pa.motion_absolute(x.max(0) as f32, y.max(0) as f32);
            return self.flush_dev(&dev);
        }
        self.inject_pointer_rel(dx, dy)
    }

    fn inject_pointer_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        if let Some((dev, p)) = self.pointer.clone() {
            p.motion_relative(dx as f32, dy as f32);
            return self.flush_dev(&dev);
        }
        Ok(())
    }

    fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.held.record_button(button, pressed);
        if let Some((dev, b)) = self.button.clone() {
            let code = match button {
                Button::Left => 0x110,
                Button::Right => 0x111,
                Button::Middle => 0x112,
                Button::Back => 0x116,
                Button::Forward => 0x115,
                Button::Other(n) => 0x110 + n as u32,
            };
            let st = if pressed {
                reis::ei::button::ButtonState::Press
            } else {
                reis::ei::button::ButtonState::Released
            };
            b.button(code, st);
            return self.flush_dev(&dev);
        }
        Ok(())
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64, _continuous: bool) -> Result<()> {
        if let Some((dev, s)) = self.scroll.clone() {
            s.scroll(dx as f32, (-dy) as f32);
            return self.flush_dev(&dev);
        }
        Ok(())
    }

    fn inject_key(
        &mut self,
        key: KeyCode,
        pressed: bool,
        _text: Option<&str>,
        _modifiers: ModifierFlags,
    ) -> Result<()> {
        self.held.record_key(key, pressed);
        if let Some((dev, kb)) = self.keyboard.clone() {
            if let Some(code) = protocol_to_evdev_keycode(key) {
                let st = if pressed {
                    reis::ei::keyboard::KeyState::Press
                } else {
                    reis::ei::keyboard::KeyState::Released
                };
                kb.key(code, st);
                return self.flush_dev(&dev);
            }
        }
        Ok(())
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        self.inject_pointer_abs(x, y, 0, 0)
    }

    fn release_all(&mut self) -> Result<()> {
        let (buttons, keys) = self.held.drain_release_order();
        for b in buttons {
            let _ = self.inject_button(b, false);
        }
        for k in keys {
            let _ = self.inject_key(k, false, None, ModifierFlags::default());
        }
        Ok(())
    }
}

fn evdev_keycode_to_protocol(code: u32) -> KeyCode {
    match code {
        1 => KeyCode::Escape,
        2 => KeyCode::Digit1,
        3 => KeyCode::Digit2,
        4 => KeyCode::Digit3,
        5 => KeyCode::Digit4,
        6 => KeyCode::Digit5,
        7 => KeyCode::Digit6,
        8 => KeyCode::Digit7,
        9 => KeyCode::Digit8,
        10 => KeyCode::Digit9,
        11 => KeyCode::Digit0,
        12 => KeyCode::Minus,
        13 => KeyCode::Equal,
        14 => KeyCode::Backspace,
        15 => KeyCode::Tab,
        16 => KeyCode::KeyQ,
        17 => KeyCode::KeyW,
        18 => KeyCode::KeyE,
        19 => KeyCode::KeyR,
        20 => KeyCode::KeyT,
        21 => KeyCode::KeyY,
        22 => KeyCode::KeyU,
        23 => KeyCode::KeyI,
        24 => KeyCode::KeyO,
        25 => KeyCode::KeyP,
        26 => KeyCode::BracketLeft,
        27 => KeyCode::BracketRight,
        28 => KeyCode::Enter,
        29 => KeyCode::ControlLeft,
        30 => KeyCode::KeyA,
        31 => KeyCode::KeyS,
        32 => KeyCode::KeyD,
        33 => KeyCode::KeyF,
        34 => KeyCode::KeyG,
        35 => KeyCode::KeyH,
        36 => KeyCode::KeyJ,
        37 => KeyCode::KeyK,
        38 => KeyCode::KeyL,
        39 => KeyCode::Semicolon,
        40 => KeyCode::Quote,
        41 => KeyCode::Backquote,
        42 => KeyCode::ShiftLeft,
        43 => KeyCode::Backslash,
        44 => KeyCode::KeyZ,
        45 => KeyCode::KeyX,
        46 => KeyCode::KeyC,
        47 => KeyCode::KeyV,
        48 => KeyCode::KeyB,
        49 => KeyCode::KeyN,
        50 => KeyCode::KeyM,
        51 => KeyCode::Comma,
        52 => KeyCode::Period,
        53 => KeyCode::Slash,
        54 => KeyCode::ShiftRight,
        55 => KeyCode::NumpadMultiply,
        56 => KeyCode::AltLeft,
        57 => KeyCode::Space,
        58 => KeyCode::CapsLock,
        59 => KeyCode::F1,
        60 => KeyCode::F2,
        61 => KeyCode::F3,
        62 => KeyCode::F4,
        63 => KeyCode::F5,
        64 => KeyCode::F6,
        65 => KeyCode::F7,
        66 => KeyCode::F8,
        67 => KeyCode::F9,
        68 => KeyCode::F10,
        69 => KeyCode::NumLock,
        70 => KeyCode::ScrollLock,
        71 => KeyCode::Numpad7,
        72 => KeyCode::Numpad8,
        73 => KeyCode::Numpad9,
        74 => KeyCode::NumpadSubtract,
        75 => KeyCode::Numpad4,
        76 => KeyCode::Numpad5,
        77 => KeyCode::Numpad6,
        78 => KeyCode::NumpadAdd,
        79 => KeyCode::Numpad1,
        80 => KeyCode::Numpad2,
        81 => KeyCode::Numpad3,
        82 => KeyCode::Numpad0,
        83 => KeyCode::NumpadDecimal,
        86 => KeyCode::IntlBackslash,
        87 => KeyCode::F11,
        88 => KeyCode::F12,
        96 => KeyCode::NumpadEnter,
        97 => KeyCode::ControlRight,
        98 => KeyCode::NumpadDivide,
        99 => KeyCode::PrintScreen,
        100 => KeyCode::AltRight,
        102 => KeyCode::Home,
        103 => KeyCode::ArrowUp,
        104 => KeyCode::PageUp,
        105 => KeyCode::ArrowLeft,
        106 => KeyCode::ArrowRight,
        107 => KeyCode::End,
        108 => KeyCode::ArrowDown,
        109 => KeyCode::PageDown,
        110 => KeyCode::Insert,
        111 => KeyCode::Delete,
        113 => KeyCode::AudioVolumeMute,
        114 => KeyCode::AudioVolumeDown,
        115 => KeyCode::AudioVolumeUp,
        119 => KeyCode::Pause,
        125 => KeyCode::MetaLeft,
        126 => KeyCode::MetaRight,
        127 => KeyCode::ContextMenu,
        163 => KeyCode::MediaTrackNext,
        164 => KeyCode::MediaPlayPause,
        165 => KeyCode::MediaTrackPrevious,
        166 => KeyCode::MediaStop,
        183 => KeyCode::F13,
        184 => KeyCode::F14,
        185 => KeyCode::F15,
        186 => KeyCode::F16,
        187 => KeyCode::F17,
        188 => KeyCode::F18,
        189 => KeyCode::F19,
        190 => KeyCode::F20,
        191 => KeyCode::F21,
        192 => KeyCode::F22,
        193 => KeyCode::F23,
        194 => KeyCode::F24,
        other => KeyCode::Unknown(other),
    }
}

fn protocol_to_evdev_keycode(key: KeyCode) -> Option<u32> {
    Some(match key {
        KeyCode::Escape => 1,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Minus => 12,
        KeyCode::Equal => 13,
        KeyCode::Backspace => 14,
        KeyCode::Tab => 15,
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::BracketLeft => 26,
        KeyCode::BracketRight => 27,
        KeyCode::Enter => 28,
        KeyCode::ControlLeft => 29,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::Semicolon => 39,
        KeyCode::Quote => 40,
        KeyCode::Backquote => 41,
        KeyCode::ShiftLeft => 42,
        KeyCode::Backslash => 43,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Comma => 51,
        KeyCode::Period => 52,
        KeyCode::Slash => 53,
        KeyCode::ShiftRight => 54,
        KeyCode::NumpadMultiply => 55,
        KeyCode::AltLeft => 56,
        KeyCode::Space => 57,
        KeyCode::CapsLock => 58,
        KeyCode::F1 => 59,
        KeyCode::F2 => 60,
        KeyCode::F3 => 61,
        KeyCode::F4 => 62,
        KeyCode::F5 => 63,
        KeyCode::F6 => 64,
        KeyCode::F7 => 65,
        KeyCode::F8 => 66,
        KeyCode::F9 => 67,
        KeyCode::F10 => 68,
        KeyCode::NumLock => 69,
        KeyCode::ScrollLock => 70,
        KeyCode::Numpad7 => 71,
        KeyCode::Numpad8 => 72,
        KeyCode::Numpad9 => 73,
        KeyCode::NumpadSubtract => 74,
        KeyCode::Numpad4 => 75,
        KeyCode::Numpad5 => 76,
        KeyCode::Numpad6 => 77,
        KeyCode::NumpadAdd => 78,
        KeyCode::Numpad1 => 79,
        KeyCode::Numpad2 => 80,
        KeyCode::Numpad3 => 81,
        KeyCode::Numpad0 => 82,
        KeyCode::NumpadDecimal => 83,
        KeyCode::IntlBackslash => 86,
        KeyCode::F11 => 87,
        KeyCode::F12 => 88,
        KeyCode::NumpadEnter => 96,
        KeyCode::ControlRight => 97,
        KeyCode::NumpadDivide => 98,
        KeyCode::PrintScreen => 99,
        KeyCode::AltRight => 100,
        KeyCode::Home => 102,
        KeyCode::ArrowUp => 103,
        KeyCode::PageUp => 104,
        KeyCode::ArrowLeft => 105,
        KeyCode::ArrowRight => 106,
        KeyCode::End => 107,
        KeyCode::ArrowDown => 108,
        KeyCode::PageDown => 109,
        KeyCode::Insert => 110,
        KeyCode::Delete => 111,
        KeyCode::AudioVolumeMute => 113,
        KeyCode::AudioVolumeDown => 114,
        KeyCode::AudioVolumeUp => 115,
        KeyCode::Pause => 119,
        KeyCode::MetaLeft => 125,
        KeyCode::MetaRight => 126,
        KeyCode::ContextMenu => 127,
        KeyCode::MediaTrackNext => 163,
        KeyCode::MediaPlayPause => 164,
        KeyCode::MediaTrackPrevious => 165,
        KeyCode::MediaStop => 166,
        KeyCode::F13 => 183,
        KeyCode::F14 => 184,
        KeyCode::F15 => 185,
        KeyCode::F16 => 186,
        KeyCode::F17 => 187,
        KeyCode::F18 => 188,
        KeyCode::F19 => 189,
        KeyCode::F20 => 190,
        KeyCode::F21 => 191,
        KeyCode::F22 => 192,
        KeyCode::F23 => 193,
        KeyCode::F24 => 194,
        KeyCode::Unknown(c) => c,
        _ => return None,
    })
}
