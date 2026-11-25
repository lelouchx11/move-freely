//! Cross-platform native input capture, injection, held-input safety tracking,
//! OS permissions diagnostics, and monitor enumeration.

pub mod headless;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use mf_protocol::{Button, KeyCode, ModifierFlags, Monitor, OsKind};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::config::ModifierMappingMode;

/// Unique marker stamped onto all injected OS events so local capture always
/// distinguishes synthetic remote input from local physical peripherals.
pub const MF_INJECTED_MARKER: i64 = 0x4D46_4B56_4D32; // "MFKVM2"

#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Local physical mouse movement while not grabbing (reports OS coords + raw delta).
    LocalPointerMove { x: i32, y: i32, dx: i32, dy: i32 },
    /// Raw hardware pointer delta while grabbing for remote routing.
    RemotePointerDelta { dx: i32, dy: i32 },
    MouseButton { button: Button, pressed: bool },
    Scroll { dx: f64, dy: f64, continuous: bool },
    Key {
        key: KeyCode,
        pressed: bool,
        text: Option<String>,
        modifiers: ModifierFlags,
    },
    /// Display configuration changed (monitor plugged/unplugged/rearranged).
    MonitorsChanged,
    /// The OS stopped isolating physical input during a remote handoff.
    CaptureLost,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionStatus {
    pub accessibility_granted: bool,
    pub input_monitoring_granted: bool,
    pub can_capture: bool,
    pub can_inject: bool,
    pub session_type: String,
    pub summary: String,
    pub action_hint: Option<String>,
}

/// Controls an active OS input capture backend.
pub trait CaptureController: Send + Sync {
    /// Toggle input grabbing (swallowing local events + pinning cursor).
    /// When releasing grab (`on = false`), optionally warp the local cursor to `(x, y)`.
    fn set_grabbing(&self, on: bool, warp_to: Option<(i32, i32)>);
    fn is_grabbing(&self) -> bool;
    fn stop(&self);
}

pub struct CaptureSession {
    pub events: mpsc::UnboundedReceiver<InputEvent>,
    pub controller: Arc<dyn CaptureController>,
}

/// Injects normalized input events into the local OS.
pub trait PlatformInjector: Send {
    fn inject_pointer_abs(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()>;
    fn inject_pointer_rel(&mut self, dx: i32, dy: i32) -> Result<()>;
    fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()>;
    fn inject_scroll(&mut self, dx: f64, dy: f64, continuous: bool) -> Result<()>;
    fn inject_key(
        &mut self,
        key: KeyCode,
        pressed: bool,
        text: Option<&str>,
        modifiers: ModifierFlags,
    ) -> Result<()>;
    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()>;
    /// Release every currently pressed key and mouse button cleanly.
    fn release_all(&mut self) -> Result<()>;
}

/// Tracks held keys and mouse buttons so disconnecting, switching controllers,
/// or triggering the emergency shortcut never leaves stuck modifiers or buttons.
#[derive(Debug, Default)]
pub struct HeldInputTracker {
    keys: HashSet<KeyCode>,
    buttons: HashSet<Button>,
}

impl HeldInputTracker {
    pub fn record_key(&mut self, key: KeyCode, pressed: bool) {
        if pressed {
            self.keys.insert(key);
        } else {
            self.keys.remove(&key);
        }
    }

    pub fn record_button(&mut self, button: Button, pressed: bool) {
        if pressed {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }

    /// Drain all held inputs in safe release order: mouse buttons, regular keys,
    /// then modifiers last.
    pub fn drain_release_order(&mut self) -> (Vec<Button>, Vec<KeyCode>) {
        let buttons: Vec<Button> = self.buttons.drain().collect();
        let mut regular = Vec::new();
        let mut modifiers = Vec::new();
        for k in self.keys.drain() {
            if k.is_modifier() {
                modifiers.push(k);
            } else {
                regular.push(k);
            }
        }
        regular.extend(modifiers);
        (buttons, regular)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }
}

/// Tracks modifier state on capture side for shortcut matching and event tagging.
#[derive(Debug, Default, Clone, Copy)]
pub struct ModifierStateTracker {
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lmeta: bool,
    rmeta: bool,
    caps_lock: bool,
}

impl ModifierStateTracker {
    pub fn update(&mut self, key: KeyCode, pressed: bool) {
        match key {
            KeyCode::ShiftLeft => self.lshift = pressed,
            KeyCode::ShiftRight => self.rshift = pressed,
            KeyCode::ControlLeft => self.lctrl = pressed,
            KeyCode::ControlRight => self.rctrl = pressed,
            KeyCode::AltLeft => self.lalt = pressed,
            KeyCode::AltRight => self.ralt = pressed,
            KeyCode::MetaLeft => self.lmeta = pressed,
            KeyCode::MetaRight => self.rmeta = pressed,
            KeyCode::CapsLock => {
                if pressed {
                    self.caps_lock = !self.caps_lock;
                }
            }
            _ => {}
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn flags(&self) -> ModifierFlags {
        ModifierFlags {
            shift: self.lshift || self.rshift,
            ctrl: self.lctrl || self.rctrl,
            alt: self.lalt,
            meta: self.lmeta || self.rmeta,
            caps_lock: self.caps_lock,
            alt_gr: self.ralt,
        }
    }
}

/// Apply cross-OS modifier mapping when forwarding from `source_os` to `target_os`.
pub fn remap_key_for_target(
    key: KeyCode,
    mods: ModifierFlags,
    source_os: OsKind,
    target_os: OsKind,
    mode: ModifierMappingMode,
) -> (KeyCode, ModifierFlags) {
    let swap = mode == ModifierMappingMode::SwapCmdCtrlCrossOs
        && ((source_os == OsKind::MacOs) != (target_os == OsKind::MacOs));
    if !swap {
        return (key, mods);
    }
    let mapped_key = match key {
        KeyCode::MetaLeft => KeyCode::ControlLeft,
        KeyCode::MetaRight => KeyCode::ControlRight,
        KeyCode::ControlLeft => KeyCode::MetaLeft,
        KeyCode::ControlRight => KeyCode::MetaRight,
        other => other,
    };
    let mapped_mods = ModifierFlags {
        ctrl: mods.meta,
        meta: mods.ctrl,
        ..mods
    };
    (mapped_key, mapped_mods)
}

/// Initialize OS-level process awareness (e.g., Windows Per-Monitor DPI v2).
pub fn init_platform_process() {
    #[cfg(target_os = "windows")]
    windows::init_dpi_awareness();
}

/// Query native permission status without prompting unless `prompt` is true.
pub fn check_permissions(prompt: bool) -> PermissionStatus {
    #[cfg(target_os = "macos")]
    {
        macos::check_permissions(prompt)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = prompt;
        windows::check_permissions()
    }
    #[cfg(target_os = "linux")]
    {
        let _ = prompt;
        linux::check_permissions()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = prompt;
        PermissionStatus {
            accessibility_granted: true,
            input_monitoring_granted: true,
            can_capture: false,
            can_inject: false,
            session_type: "unsupported".into(),
            summary: "Unsupported platform".into(),
            action_hint: None,
        }
    }
}

/// Open OS privacy/permission settings page when available (macOS Accessibility/Input Monitoring).
pub fn open_permission_settings(kind: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::open_permission_settings(kind)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = kind;
        Ok(())
    }
}

/// Enumerate all active monitors connected to this machine.
pub fn enumerate_monitors() -> Vec<Monitor> {
    #[cfg(target_os = "macos")]
    {
        macos::enumerate_monitors()
    }
    #[cfg(target_os = "windows")]
    {
        windows::enumerate_monitors()
    }
    #[cfg(target_os = "linux")]
    {
        linux::enumerate_monitors()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        vec![default_fallback_monitor()]
    }
}

pub fn default_fallback_monitor() -> Monitor {
    Monitor {
        id: "primary".into(),
        name: "Primary Display".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        physical_width: 1920,
        physical_height: 1080,
        scale_factor: 1.0,
        is_primary: true,
    }
}

/// Start native global input capture for the current OS.
pub fn start_native_capture() -> Result<CaptureSession> {
    #[cfg(target_os = "macos")]
    {
        macos::start_capture()
    }
    #[cfg(target_os = "windows")]
    {
        windows::start_capture()
    }
    #[cfg(target_os = "linux")]
    {
        linux::start_capture()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        anyhow::bail!("native input capture is not supported on this OS")
    }
}

/// Create the native input injector for the current OS.
pub fn new_native_injector() -> Result<Box<dyn PlatformInjector>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacInjector::new()?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WinInjector::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        linux::new_injector()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        anyhow::bail!("native input injection is not supported on this OS")
    }
}

/// Run a short local input self-test (pointer square + typing "hi") to verify OS permissions.
pub fn selftest() -> Result<()> {
    let perms = check_permissions(true);
    tracing::info!(?perms, "OS permission status");
    if !perms.can_inject {
        anyhow::bail!(
            "cannot inject input on this OS: {} ({})",
            perms.summary,
            perms.action_hint.unwrap_or_default()
        );
    }
    let mut inj = new_native_injector()?;
    let step = 60;
    for (dx, dy) in [(step, 0), (0, step), (-step, 0), (0, -step)] {
        inj.inject_pointer_rel(dx, dy)?;
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    for (k, ch) in [(KeyCode::KeyH, "h"), (KeyCode::KeyI, "i")] {
        inj.inject_key(k, true, Some(ch), ModifierFlags::default())?;
        inj.inject_key(k, false, None, ModifierFlags::default())?;
    }
    inj.release_all()?;
    Ok(())
}

/// Basic AtomicBool-backed controller helper for backends.
#[derive(Default)]
pub struct FlagGrabState {
    pub grabbing: AtomicBool,
    pub stop: AtomicBool,
}

impl FlagGrabState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn is_grabbing(&self) -> bool {
        self.grabbing.load(Ordering::SeqCst)
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_tracker_releases_regular_before_modifiers() {
        let mut tracker = HeldInputTracker::default();
        tracker.record_key(KeyCode::ControlLeft, true);
        tracker.record_key(KeyCode::ShiftLeft, true);
        tracker.record_key(KeyCode::KeyC, true);
        tracker.record_button(Button::Left, true);

        let (buttons, keys) = tracker.drain_release_order();
        assert_eq!(buttons, vec![Button::Left]);
        assert_eq!(keys[0], KeyCode::KeyC);
        assert!(keys[1..].contains(&KeyCode::ControlLeft));
        assert!(keys[1..].contains(&KeyCode::ShiftLeft));
        assert!(tracker.is_empty());
    }

    #[test]
    fn modifier_remap_swaps_cmd_ctrl_between_macos_and_windows() {
        let mods = ModifierFlags {
            meta: true,
            shift: true,
            ..Default::default()
        };
        let (k, m) = remap_key_for_target(
            KeyCode::MetaLeft,
            mods,
            OsKind::MacOs,
            OsKind::Windows,
            ModifierMappingMode::SwapCmdCtrlCrossOs,
        );
        assert_eq!(k, KeyCode::ControlLeft);
        assert!(m.ctrl && !m.meta && m.shift);
    }
}
