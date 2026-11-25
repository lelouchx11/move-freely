//! Native macOS input capture (`CGEventTap`), input injection (`CGEventPost`),
//! display enumeration (`CGDisplay`), and TCC permission diagnostics.

#![allow(non_upper_case_globals)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use core_foundation::base::TCFType;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef, UniChar};
use core_graphics::display::{CGDisplay, CGPoint};
use core_graphics::event::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode,
    CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use mf_protocol::{Button, KeyCode, ModifierFlags, Monitor};
use tokio::sync::mpsc;

use super::{
    CaptureController, CaptureSession, HeldInputTracker, InputEvent, ModifierStateTracker,
    PermissionStatus, PlatformInjector, MF_INJECTED_MARKER,
};

// ---------------------------------------------------------------------------
// Permissions (Accessibility + Input Monitoring)
// ---------------------------------------------------------------------------

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

pub fn check_permissions(prompt: bool) -> PermissionStatus {
    let accessibility_granted = unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let val = if prompt {
            core_foundation::boolean::CFBoolean::true_value()
        } else {
            core_foundation::boolean::CFBoolean::false_value()
        };
        let dict = CFDictionary::from_CFType_pairs(&[(key, val)]);
        AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef())
    };

    let input_monitoring_granted = unsafe {
        if prompt {
            if !CGPreflightListenEventAccess() {
                CGRequestListenEventAccess()
            } else {
                true
            }
        } else {
            CGPreflightListenEventAccess()
        }
    };

    let can_inject = accessibility_granted;
    let can_capture = accessibility_granted && input_monitoring_granted;

    let (summary, action_hint) = match (accessibility_granted, input_monitoring_granted) {
        (true, true) => (
            "Accessibility and Input Monitoring granted".to_string(),
            None,
        ),
        (false, true) => (
            "Accessibility permission required to control input".to_string(),
            Some("Open System Settings → Privacy & Security → Accessibility and enable Move Freely".to_string()),
        ),
        (true, false) => (
            "Input Monitoring permission required to capture keyboard & mouse".to_string(),
            Some("Open System Settings → Privacy & Security → Input Monitoring and enable Move Freely".to_string()),
        ),
        (false, false) => (
            "Accessibility and Input Monitoring permissions required".to_string(),
            Some("Grant both Accessibility and Input Monitoring in System Settings → Privacy & Security".to_string()),
        ),
    };

    PermissionStatus {
        accessibility_granted,
        input_monitoring_granted,
        can_capture,
        can_inject,
        session_type: "macos".into(),
        summary,
        action_hint,
    }
}

pub fn open_permission_settings(kind: &str) -> Result<()> {
    let url = match kind {
        "input_monitoring" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        }
        _ => "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
    };
    let _ = std::process::Command::new("open").arg(url).spawn();
    Ok(())
}

// ---------------------------------------------------------------------------
// Display Enumeration
// ---------------------------------------------------------------------------

pub fn enumerate_monitors() -> Vec<Monitor> {
    let Ok(ids) = CGDisplay::active_displays() else {
        return vec![super::default_fallback_monitor()];
    };
    let mut monitors = Vec::new();
    for (idx, id) in ids.into_iter().enumerate() {
        let d = CGDisplay::new(id);
        let bounds = d.bounds();
        let (pw, ph) = d
            .display_mode()
            .map(|m| (m.pixel_width() as u32, m.pixel_height() as u32))
            .unwrap_or_else(|| (d.pixels_wide() as u32, d.pixels_high() as u32));
        let lw = bounds.size.width.round().max(1.0) as u32;
        let lh = bounds.size.height.round().max(1.0) as u32;
        let scale = if lw > 0 {
            (pw as f64 / lw as f64).max(1.0)
        } else {
            1.0
        };
        let name = if d.is_builtin() {
            "Built-in Display".to_string()
        } else if d.is_main() {
            format!("Primary Display {}", idx + 1)
        } else {
            format!("Display {} ({})", idx + 1, id)
        };
        monitors.push(Monitor {
            id: format!("cg-{id}"),
            name,
            x: bounds.origin.x.round() as i32,
            y: bounds.origin.y.round() as i32,
            width: lw,
            height: lh,
            physical_width: pw,
            physical_height: ph,
            scale_factor: scale,
            is_primary: d.is_main(),
        });
    }
    if monitors.is_empty() {
        monitors.push(super::default_fallback_monitor());
    }
    monitors
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: *const c_void,
        order: core_foundation::base::CFIndex,
    ) -> *const c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(rl: *mut c_void, source: *const c_void, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRunLoopStop(rl: *mut c_void);
    static kCFRunLoopCommonModes: CFStringRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: RawTapCallback,
        user_info: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: *mut core_graphics::sys::CGEvent, field: CGEventField) -> i64;
    fn CGEventGetDoubleValueField(event: *mut core_graphics::sys::CGEvent, field: CGEventField) -> f64;
    fn CGEventGetLocation(event: *mut core_graphics::sys::CGEvent) -> CGPoint;
    fn CGEventGetFlags(event: *mut core_graphics::sys::CGEvent) -> CGEventFlags;
    fn CGEventKeyboardGetUnicodeString(
        event: *mut core_graphics::sys::CGEvent,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut UniChar,
    );
}

type RawTapCallback = unsafe extern "C" fn(
    proxy: *mut c_void,
    etype: u32,
    event: *mut core_graphics::sys::CGEvent,
    user_info: *mut c_void,
) -> *mut core_graphics::sys::CGEvent;

fn keyboard_event_text(event: *mut core_graphics::sys::CGEvent) -> Option<String> {
    // Event taps run on a worker thread. Read the event's text without calling
    // Text Input Source APIs, which require the main thread.
    let mut units = [0; 64];
    let mut length = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(event, units.len(), &mut length, units.as_mut_ptr());
    }
    let text = String::from_utf16(units.get(..length)?).ok()?;
    text.chars().any(|ch| !ch.is_control()).then_some(text)
}

// ---------------------------------------------------------------------------
// Native Capture + Grab via CGEventTap
// ---------------------------------------------------------------------------

struct TapContext {
    tx: mpsc::UnboundedSender<InputEvent>,
    grabbing: AtomicBool,
    cursor_hidden: AtomicBool,
    tap_port: Mutex<usize>,
    run_loop: Mutex<usize>,
    mods: Mutex<ModifierStateTracker>,
    last_local_pos: Mutex<Option<(i32, i32)>>,
}

struct MacCaptureController {
    ctx: Arc<TapContext>,
}

impl CaptureController for MacCaptureController {
    fn set_grabbing(&self, on: bool, warp_to: Option<(i32, i32)>) {
        if self.is_grabbing() == on {
            return;
        }
        if on {
            self.ctx.grabbing.store(true, Ordering::SeqCst);
            let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(false);
            if !self.ctx.cursor_hidden.swap(true, Ordering::SeqCst) {
                let _ = CGDisplay::main().hide_cursor();
            }
        } else {
            if let Some((wx, wy)) = warp_to {
                let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(wx as f64, wy as f64));
            }
            *self.ctx.last_local_pos.lock().unwrap() = warp_to;
            let _ = CGDisplay::associate_mouse_and_mouse_cursor_position(true);
            if self.ctx.cursor_hidden.swap(false, Ordering::SeqCst) {
                let _ = CGDisplay::main().show_cursor();
            }
            self.ctx.grabbing.store(false, Ordering::SeqCst);
        }
    }

    fn is_grabbing(&self) -> bool {
        self.ctx.grabbing.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.set_grabbing(false, None);
        let rl = *self.ctx.run_loop.lock().unwrap();
        if rl != 0 {
            unsafe {
                CFRunLoopStop(rl as *mut c_void);
            }
        }
    }
}

impl Drop for MacCaptureController {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe extern "C" fn event_tap_cb(
    _proxy: *mut c_void,
    etype: u32,
    raw_event: *mut core_graphics::sys::CGEvent,
    user_info: *mut c_void,
) -> *mut core_graphics::sys::CGEvent {
    let ctx = &*(user_info as *const TapContext);

    // Re-enable tap automatically if macOS disabled it due to timeout or user input.
    if etype == CGEventType::TapDisabledByTimeout as u32
        || etype == CGEventType::TapDisabledByUserInput as u32
    {
        let tap = *ctx.tap_port.lock().unwrap();
        if tap != 0 {
            CGEventTapEnable(tap as *mut c_void, true);
        }
        return raw_event;
    }

    if raw_event.is_null() {
        return raw_event;
    }

    let user_data = CGEventGetIntegerValueField(raw_event, EventField::EVENT_SOURCE_USER_DATA);
    if user_data == MF_INJECTED_MARKER {
        return raw_event;
    }

    let grabbing = ctx.grabbing.load(Ordering::SeqCst);
    let mut swallow = grabbing;

    match etype {
        // Mouse move & dragged
        5 | 6 | 7 | 27 => {
            let raw_dx = CGEventGetIntegerValueField(raw_event, EventField::MOUSE_EVENT_DELTA_X) as i32;
            let raw_dy = CGEventGetIntegerValueField(raw_event, EventField::MOUSE_EVENT_DELTA_Y) as i32;
            if grabbing {
                if raw_dx != 0 || raw_dy != 0 {
                    let _ = ctx.tx.send(InputEvent::RemotePointerDelta {
                        dx: raw_dx,
                        dy: raw_dy,
                    });
                }
            } else {
                let loc = CGEventGetLocation(raw_event);
                let (x, y) = (loc.x.round() as i32, loc.y.round() as i32);
                let mut last = ctx.last_local_pos.lock().unwrap();
                let (dx, dy) = if raw_dx != 0 || raw_dy != 0 {
                    (raw_dx, raw_dy)
                } else if let Some((px, py)) = *last {
                    (x - px, y - py)
                } else {
                    (0, 0)
                };
                *last = Some((x, y));
                if dx != 0 || dy != 0 {
                    let _ = ctx.tx.send(InputEvent::LocalPointerMove { x, y, dx, dy });
                }
            }
        }
        // Left/Right/Other mouse buttons
        1 => {
            if grabbing {
                let _ = ctx.tx.send(InputEvent::MouseButton {
                    button: Button::Left,
                    pressed: true,
                });
            }
        }
        2 => {
            if grabbing {
                let _ = ctx.tx.send(InputEvent::MouseButton {
                    button: Button::Left,
                    pressed: false,
                });
            }
        }
        3 => {
            if grabbing {
                let _ = ctx.tx.send(InputEvent::MouseButton {
                    button: Button::Right,
                    pressed: true,
                });
            }
        }
        4 => {
            if grabbing {
                let _ = ctx.tx.send(InputEvent::MouseButton {
                    button: Button::Right,
                    pressed: false,
                });
            }
        }
        25 | 26 => {
            if grabbing {
                let pressed = etype == 25;
                let btn_num = CGEventGetIntegerValueField(raw_event, EventField::MOUSE_EVENT_BUTTON_NUMBER);
                let button = match btn_num {
                    2 => Button::Middle,
                    3 => Button::Back,
                    4 => Button::Forward,
                    other => Button::Other(other.clamp(0, 255) as u8),
                };
                let _ = ctx
                    .tx
                    .send(InputEvent::MouseButton { button, pressed });
            }
        }
        // Scroll wheel
        22 => {
            if grabbing {
                const IS_CONTINUOUS_FIELD: CGEventField = 88;
                let continuous = CGEventGetIntegerValueField(raw_event, IS_CONTINUOUS_FIELD) != 0;
                let (dx, dy) = if continuous {
                    let py = CGEventGetDoubleValueField(
                        raw_event,
                        EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1,
                    );
                    let px = CGEventGetDoubleValueField(
                        raw_event,
                        EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2,
                    );
                    // macOS axis2 positive = scroll left; normalize to +dx = right
                    (-px, py)
                } else {
                    let ly = CGEventGetIntegerValueField(
                        raw_event,
                        EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1,
                    ) as f64;
                    let lx = CGEventGetIntegerValueField(
                        raw_event,
                        EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2,
                    ) as f64;
                    (-lx, ly)
                };
                if dx.abs() > f64::EPSILON || dy.abs() > f64::EPSILON {
                    let _ = ctx.tx.send(InputEvent::Scroll {
                        dx,
                        dy,
                        continuous,
                    });
                }
            }
        }
        // KeyDown / KeyUp
        10 | 11 => {
            let pressed = etype == 10;
            let code = CGEventGetIntegerValueField(raw_event, EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let key = cg_keycode_to_protocol(code);
            let flags = CGEventGetFlags(raw_event);
            let mods = {
                let mut m = ctx.mods.lock().unwrap();
                m.update(key, pressed);
                let mut f = m.flags();
                // Reconcile against live CGEventFlags so CapsLock/external states stay accurate.
                f.shift |= flags.contains(CGEventFlags::CGEventFlagShift);
                f.ctrl |= flags.contains(CGEventFlags::CGEventFlagControl);
                f.alt |= flags.contains(CGEventFlags::CGEventFlagAlternate);
                f.meta |= flags.contains(CGEventFlags::CGEventFlagCommand);
                f.caps_lock = flags.contains(CGEventFlags::CGEventFlagAlphaShift);
                f
            };
            let text = if grabbing && pressed && !mods.has_command_modifier() {
                keyboard_event_text(raw_event)
            } else {
                None
            };
            // Always forward key events so the engine can match global shortcuts
            // (e.g. F12, ClaimController, EmergencyRelease) even when local!
            let _ = ctx.tx.send(InputEvent::Key {
                key,
                pressed,
                text,
                modifiers: mods,
            });
            swallow = grabbing;
        }
        // FlagsChanged (modifier press/release)
        12 => {
            let code = CGEventGetIntegerValueField(raw_event, EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let key = cg_keycode_to_protocol(code);
            let flags = CGEventGetFlags(raw_event);
            let pressed = is_modifier_pressed_from_flags(code, flags);
            let mods = {
                let mut m = ctx.mods.lock().unwrap();
                m.update(key, pressed);
                m.flags()
            };
            let _ = ctx.tx.send(InputEvent::Key {
                key,
                pressed,
                text: None,
                modifiers: mods,
            });
            swallow = grabbing;
        }
        _ => {
            swallow = false;
        }
    }

    if swallow {
        std::ptr::null_mut()
    } else {
        raw_event
    }
}

pub fn start_capture() -> Result<CaptureSession> {
    let perms = check_permissions(false);
    if !perms.can_capture {
        anyhow::bail!(
            "{} ({})",
            perms.summary,
            perms.action_hint.unwrap_or_default()
        );
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let ctx = Arc::new(TapContext {
        tx,
        grabbing: AtomicBool::new(false),
        cursor_hidden: AtomicBool::new(false),
        tap_port: Mutex::new(0),
        run_loop: Mutex::new(0),
        mods: Mutex::new(ModifierStateTracker::default()),
        last_local_pos: Mutex::new(None),
    });

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let thread_ctx = ctx.clone();

    std::thread::Builder::new()
        .name("mf-macos-capture".into())
        .spawn(move || unsafe {
            let mask: u64 = (1 << 1)
                | (1 << 2)
                | (1 << 3)
                | (1 << 4)
                | (1 << 5)
                | (1 << 6)
                | (1 << 7)
                | (1 << 10)
                | (1 << 11)
                | (1 << 12)
                | (1 << 22)
                | (1 << 25)
                | (1 << 26)
                | (1 << 27);

            let raw_ctx = Arc::into_raw(thread_ctx.clone()) as *mut c_void;
            let tap = CGEventTapCreate(
                CGEventTapLocation::HID,
                0, // kCGHeadInsertEventTap
                0, // kCGEventTapOptionDefault (active filter)
                mask,
                event_tap_cb,
                raw_ctx,
            );
            if tap.is_null() {
                let _ = Arc::from_raw(raw_ctx as *const TapContext);
                let _ = ready_tx.send(Err(anyhow!(
                    "CGEventTapCreate failed — ensure Accessibility and Input Monitoring are granted"
                )));
                return;
            }

            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), tap, 0);
            if source.is_null() {
                CFRelease(tap);
                let _ = Arc::from_raw(raw_ctx as *const TapContext);
                let _ = ready_tx.send(Err(anyhow!("CFMachPortCreateRunLoopSource failed")));
                return;
            }

            let rl = CFRunLoopGetCurrent();
            *thread_ctx.tap_port.lock().unwrap() = tap as usize;
            *thread_ctx.run_loop.lock().unwrap() = rl as usize;

            CFRunLoopAddSource(rl, source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
            let _ = ready_tx.send(Ok(()));

            CFRunLoopRun();

            // Clean teardown on CFRunLoopStop
            CGEventTapEnable(tap, false);
            CFRelease(source);
            CFRelease(tap);
            let _ = Arc::from_raw(raw_ctx as *const TapContext);
        })?;

    ready_rx
        .recv()
        .map_err(|_| anyhow!("macOS capture thread exited prematurely"))??;

    Ok(CaptureSession {
        events: rx,
        controller: Arc::new(MacCaptureController { ctx }),
    })
}

// ---------------------------------------------------------------------------
// Native Injection via CGEventPost
// ---------------------------------------------------------------------------

pub struct MacInjector {
    source: CGEventSource,
    held: HeldInputTracker,
    active_flags: CGEventFlags,
    cursor_pos: (f64, f64),
    pressed_buttons: u8, // bit 0 = left, 1 = right, 2 = middle
    last_clicks: HashMap<u8, (i64, Instant)>,
    double_click_gap: Duration,
}

// CoreGraphics `CGEventSource` and `CGEventPost` are thread-safe.
unsafe impl Send for MacInjector {}

impl MacInjector {
    pub fn new() -> Result<Self> {
        let perms = check_permissions(false);
        if !perms.can_inject {
            anyhow::bail!(
                "{} ({})",
                perms.summary,
                perms.action_hint.unwrap_or_default()
            );
        }
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed creating CGEventSource"))?;
        let loc = CGEvent::new(source.clone())
            .map(|e| e.location())
            .unwrap_or(CGPoint::new(100.0, 100.0));
        Ok(Self {
            source,
            held: HeldInputTracker::default(),
            active_flags: CGEventFlags::CGEventFlagNonCoalesced,
            cursor_pos: (loc.x, loc.y),
            pressed_buttons: 0,
            last_clicks: HashMap::new(),
            double_click_gap: Duration::from_millis(450),
        })
    }

    fn next_click_count(&mut self, btn_idx: u8, pressed: bool) -> i64 {
        let now = Instant::now();
        if pressed {
            let count = match self.last_clicks.get(&btn_idx) {
                Some((prev, at)) if now.duration_since(*at) <= self.double_click_gap => prev + 1,
                _ => 1,
            };
            self.last_clicks.insert(btn_idx, (count, now));
            count
        } else {
            self.last_clicks.get(&btn_idx).map(|(c, _)| *c).unwrap_or(1)
        }
    }

    fn post_event(&self, ev: &CGEvent) {
        ev.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, MF_INJECTED_MARKER);
        ev.post(CGEventTapLocation::HID);
    }

    fn update_modifier_flags(&mut self, key: KeyCode, pressed: bool) {
        let bit = match key {
            KeyCode::ShiftLeft | KeyCode::ShiftRight => CGEventFlags::CGEventFlagShift,
            KeyCode::ControlLeft | KeyCode::ControlRight => CGEventFlags::CGEventFlagControl,
            KeyCode::AltLeft | KeyCode::AltRight => CGEventFlags::CGEventFlagAlternate,
            KeyCode::MetaLeft | KeyCode::MetaRight => CGEventFlags::CGEventFlagCommand,
            KeyCode::CapsLock => {
                if pressed {
                    self.active_flags.toggle(CGEventFlags::CGEventFlagAlphaShift);
                }
                return;
            }
            _ => return,
        };
        if pressed {
            self.active_flags.insert(bit);
        } else {
            self.active_flags.remove(bit);
        }
    }
}

impl Drop for MacInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

impl PlatformInjector for MacInjector {
    fn inject_pointer_abs(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        let (x, y) = (x as f64, y as f64);
        self.cursor_pos = (x, y);
        let (etype, btn) = if (self.pressed_buttons & 1) != 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if (self.pressed_buttons & 2) != 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if (self.pressed_buttons & 4) != 0 {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        let ev = CGEvent::new_mouse_event(self.source.clone(), etype, CGPoint::new(x, y), btn)
            .map_err(|_| anyhow!("CGEvent::new_mouse_event failed"))?;
        ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
        ev.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
        ev.set_flags(self.active_flags);
        self.post_event(&ev);
        Ok(())
    }

    fn inject_pointer_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let nx = (self.cursor_pos.0 + dx as f64).round() as i32;
        let ny = (self.cursor_pos.1 + dy as f64).round() as i32;
        self.inject_pointer_abs(nx, ny, dx, dy)
    }

    fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.held.record_button(button, pressed);
        let (etype, cg_btn, btn_num, mask_bit) = match button {
            Button::Left => (
                if pressed {
                    CGEventType::LeftMouseDown
                } else {
                    CGEventType::LeftMouseUp
                },
                CGMouseButton::Left,
                0i64,
                1u8,
            ),
            Button::Right => (
                if pressed {
                    CGEventType::RightMouseDown
                } else {
                    CGEventType::RightMouseUp
                },
                CGMouseButton::Right,
                1i64,
                2u8,
            ),
            Button::Middle => (
                if pressed {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                },
                CGMouseButton::Center,
                2i64,
                4u8,
            ),
            Button::Back => (
                if pressed {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                },
                CGMouseButton::Center,
                3i64,
                0u8,
            ),
            Button::Forward => (
                if pressed {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                },
                CGMouseButton::Center,
                4i64,
                0u8,
            ),
            Button::Other(n) => (
                if pressed {
                    CGEventType::OtherMouseDown
                } else {
                    CGEventType::OtherMouseUp
                },
                CGMouseButton::Center,
                n as i64,
                0u8,
            ),
        };
        if mask_bit != 0 {
            if pressed {
                self.pressed_buttons |= mask_bit;
            } else {
                self.pressed_buttons &= !mask_bit;
            }
        }
        let click_count = self.next_click_count(btn_num as u8, pressed);
        let pos = CGPoint::new(self.cursor_pos.0, self.cursor_pos.1);
        let ev = CGEvent::new_mouse_event(self.source.clone(), etype, pos, cg_btn)
            .map_err(|_| anyhow!("CGEvent::new_mouse_event button failed"))?;
        ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, btn_num);
        ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count);
        ev.set_flags(self.active_flags);
        self.post_event(&ev);
        Ok(())
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64, continuous: bool) -> Result<()> {
        let (unit, scale) = if continuous {
            (ScrollEventUnit::PIXEL, 1.0)
        } else {
            (ScrollEventUnit::LINE, 1.0)
        };
        let wy = (dy * scale).round() as i32;
        let wx = (-dx * scale).round() as i32;
        if wy == 0 && wx == 0 {
            return Ok(());
        }
        let ev = CGEvent::new_scroll_event(self.source.clone(), unit, 2, wy, wx, 0)
            .map_err(|_| anyhow!("CGEvent::new_scroll_event failed"))?;
        ev.set_flags(self.active_flags);
        self.post_event(&ev);
        Ok(())
    }

    fn inject_key(
        &mut self,
        key: KeyCode,
        pressed: bool,
        text: Option<&str>,
        _modifiers: ModifierFlags,
    ) -> Result<()> {
        self.held.record_key(key, pressed);
        self.update_modifier_flags(key, pressed);

        if let KeyCode::Unicode(ch) = key {
            if pressed {
                let ev = CGEvent::new_keyboard_event(self.source.clone(), 0, true)
                    .map_err(|_| anyhow!("CGEvent::new_keyboard_event unicode failed"))?;
                let mut buf = [0u8; 4];
                ev.set_string(ch.encode_utf8(&mut buf));
                ev.set_flags(CGEventFlags::CGEventFlagNull);
                self.post_event(&ev);
                let up = CGEvent::new_keyboard_event(self.source.clone(), 0, false)
                    .map_err(|_| anyhow!("CGEvent::new_keyboard_event unicode up failed"))?;
                self.post_event(&up);
            }
            return Ok(());
        }

        if let Some(code) = protocol_to_cg_keycode(key) {
            let ev = CGEvent::new_keyboard_event(self.source.clone(), code, pressed)
                .map_err(|_| anyhow!("CGEvent::new_keyboard_event failed"))?;
            ev.set_flags(self.active_flags);
            // If non-ASCII text was supplied with no command modifier held, attach the
            // Unicode payload so international characters cross keyboard layouts cleanly.
            if pressed {
                if let Some(txt) = text {
                    let has_cmd_mod = self.active_flags.intersects(
                        CGEventFlags::CGEventFlagCommand
                            | CGEventFlags::CGEventFlagControl
                            | CGEventFlags::CGEventFlagAlternate,
                    );
                    if !has_cmd_mod && !txt.is_empty() && !txt.is_ascii() {
                        ev.set_string(txt);
                    }
                }
            }
            self.post_event(&ev);
        } else if pressed {
            if let Some(txt) = text {
                let ev = CGEvent::new_keyboard_event(self.source.clone(), 0, true)
                    .map_err(|_| anyhow!("CGEvent::new_keyboard_event fallback failed"))?;
                ev.set_string(txt);
                self.post_event(&ev);
            }
        }
        Ok(())
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        self.cursor_pos = (x as f64, y as f64);
        CGDisplay::warp_mouse_cursor_position(CGPoint::new(x as f64, y as f64))
            .map_err(|e| anyhow!("CGWarpMouseCursorPosition failed ({e})"))?;
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let (buttons, keys) = self.held.drain_release_order();
        for b in buttons {
            let _ = self.inject_button(b, false);
        }
        for k in keys {
            let _ = self.inject_key(k, false, None, ModifierFlags::default());
        }
        self.active_flags = CGEventFlags::CGEventFlagNonCoalesced;
        self.pressed_buttons = 0;
        Ok(())
    }
}

fn is_modifier_pressed_from_flags(code: u16, flags: CGEventFlags) -> bool {
    match code {
        56 | 60 => flags.contains(CGEventFlags::CGEventFlagShift),
        59 | 62 => flags.contains(CGEventFlags::CGEventFlagControl),
        58 | 61 => flags.contains(CGEventFlags::CGEventFlagAlternate),
        55 | 54 => flags.contains(CGEventFlags::CGEventFlagCommand),
        57 => flags.contains(CGEventFlags::CGEventFlagAlphaShift),
        _ => false,
    }
}

pub fn cg_keycode_to_protocol(code: u16) -> KeyCode {
    match code {
        0 => KeyCode::KeyA,
        11 => KeyCode::KeyB,
        8 => KeyCode::KeyC,
        2 => KeyCode::KeyD,
        14 => KeyCode::KeyE,
        3 => KeyCode::KeyF,
        5 => KeyCode::KeyG,
        4 => KeyCode::KeyH,
        34 => KeyCode::KeyI,
        38 => KeyCode::KeyJ,
        40 => KeyCode::KeyK,
        37 => KeyCode::KeyL,
        46 => KeyCode::KeyM,
        45 => KeyCode::KeyN,
        31 => KeyCode::KeyO,
        35 => KeyCode::KeyP,
        12 => KeyCode::KeyQ,
        15 => KeyCode::KeyR,
        1 => KeyCode::KeyS,
        17 => KeyCode::KeyT,
        32 => KeyCode::KeyU,
        9 => KeyCode::KeyV,
        13 => KeyCode::KeyW,
        7 => KeyCode::KeyX,
        16 => KeyCode::KeyY,
        6 => KeyCode::KeyZ,
        29 => KeyCode::Digit0,
        18 => KeyCode::Digit1,
        19 => KeyCode::Digit2,
        20 => KeyCode::Digit3,
        21 => KeyCode::Digit4,
        23 => KeyCode::Digit5,
        22 => KeyCode::Digit6,
        26 => KeyCode::Digit7,
        28 => KeyCode::Digit8,
        25 => KeyCode::Digit9,
        27 => KeyCode::Minus,
        24 => KeyCode::Equal,
        33 => KeyCode::BracketLeft,
        30 => KeyCode::BracketRight,
        42 => KeyCode::Backslash,
        41 => KeyCode::Semicolon,
        39 => KeyCode::Quote,
        50 => KeyCode::Backquote,
        43 => KeyCode::Comma,
        47 => KeyCode::Period,
        44 => KeyCode::Slash,
        10 => KeyCode::IntlBackslash,
        36 => KeyCode::Enter,
        53 => KeyCode::Escape,
        51 => KeyCode::Backspace,
        48 => KeyCode::Tab,
        49 => KeyCode::Space,
        57 => KeyCode::CapsLock,
        56 => KeyCode::ShiftLeft,
        60 => KeyCode::ShiftRight,
        59 => KeyCode::ControlLeft,
        62 => KeyCode::ControlRight,
        58 => KeyCode::AltLeft,
        61 => KeyCode::AltRight,
        55 => KeyCode::MetaLeft,
        54 => KeyCode::MetaRight,
        110 => KeyCode::ContextMenu,
        122 => KeyCode::F1,
        120 => KeyCode::F2,
        99 => KeyCode::F3,
        118 => KeyCode::F4,
        96 => KeyCode::F5,
        97 => KeyCode::F6,
        98 => KeyCode::F7,
        100 => KeyCode::F8,
        101 => KeyCode::F9,
        109 => KeyCode::F10,
        103 => KeyCode::F11,
        111 => KeyCode::F12,
        105 => KeyCode::F13,
        107 => KeyCode::F14,
        113 => KeyCode::F15,
        106 => KeyCode::F16,
        64 => KeyCode::F17,
        79 => KeyCode::F18,
        80 => KeyCode::F19,
        90 => KeyCode::F20,
        114 => KeyCode::Insert,
        117 => KeyCode::Delete,
        115 => KeyCode::Home,
        119 => KeyCode::End,
        116 => KeyCode::PageUp,
        121 => KeyCode::PageDown,
        126 => KeyCode::ArrowUp,
        125 => KeyCode::ArrowDown,
        123 => KeyCode::ArrowLeft,
        124 => KeyCode::ArrowRight,
        71 => KeyCode::NumLock,
        82 => KeyCode::Numpad0,
        83 => KeyCode::Numpad1,
        84 => KeyCode::Numpad2,
        85 => KeyCode::Numpad3,
        86 => KeyCode::Numpad4,
        87 => KeyCode::Numpad5,
        88 => KeyCode::Numpad6,
        89 => KeyCode::Numpad7,
        91 => KeyCode::Numpad8,
        92 => KeyCode::Numpad9,
        69 => KeyCode::NumpadAdd,
        78 => KeyCode::NumpadSubtract,
        67 => KeyCode::NumpadMultiply,
        75 => KeyCode::NumpadDivide,
        65 => KeyCode::NumpadDecimal,
        76 => KeyCode::NumpadEnter,
        81 => KeyCode::NumpadEqual,
        other => KeyCode::Unknown(other as u32),
    }
}

pub fn protocol_to_cg_keycode(key: KeyCode) -> Option<CGKeyCode> {
    Some(match key {
        KeyCode::KeyA => 0,
        KeyCode::KeyB => 11,
        KeyCode::KeyC => 8,
        KeyCode::KeyD => 2,
        KeyCode::KeyE => 14,
        KeyCode::KeyF => 3,
        KeyCode::KeyG => 5,
        KeyCode::KeyH => 4,
        KeyCode::KeyI => 34,
        KeyCode::KeyJ => 38,
        KeyCode::KeyK => 40,
        KeyCode::KeyL => 37,
        KeyCode::KeyM => 46,
        KeyCode::KeyN => 45,
        KeyCode::KeyO => 31,
        KeyCode::KeyP => 35,
        KeyCode::KeyQ => 12,
        KeyCode::KeyR => 15,
        KeyCode::KeyS => 1,
        KeyCode::KeyT => 17,
        KeyCode::KeyU => 32,
        KeyCode::KeyV => 9,
        KeyCode::KeyW => 13,
        KeyCode::KeyX => 7,
        KeyCode::KeyY => 16,
        KeyCode::KeyZ => 6,
        KeyCode::Digit0 => 29,
        KeyCode::Digit1 => 18,
        KeyCode::Digit2 => 19,
        KeyCode::Digit3 => 20,
        KeyCode::Digit4 => 21,
        KeyCode::Digit5 => 23,
        KeyCode::Digit6 => 22,
        KeyCode::Digit7 => 26,
        KeyCode::Digit8 => 28,
        KeyCode::Digit9 => 25,
        KeyCode::Minus => 27,
        KeyCode::Equal => 24,
        KeyCode::BracketLeft => 33,
        KeyCode::BracketRight => 30,
        KeyCode::Backslash => 42,
        KeyCode::Semicolon => 41,
        KeyCode::Quote => 39,
        KeyCode::Backquote => 50,
        KeyCode::Comma => 43,
        KeyCode::Period => 47,
        KeyCode::Slash => 44,
        KeyCode::IntlBackslash => 10,
        KeyCode::Enter => 36,
        KeyCode::Escape => 53,
        KeyCode::Backspace => 51,
        KeyCode::Tab => 48,
        KeyCode::Space => 49,
        KeyCode::CapsLock => 57,
        KeyCode::ShiftLeft => 56,
        KeyCode::ShiftRight => 60,
        KeyCode::ControlLeft => 59,
        KeyCode::ControlRight => 62,
        KeyCode::AltLeft => 58,
        KeyCode::AltRight => 61,
        KeyCode::MetaLeft => 55,
        KeyCode::MetaRight => 54,
        KeyCode::ContextMenu => 110,
        KeyCode::F1 => 122,
        KeyCode::F2 => 120,
        KeyCode::F3 => 99,
        KeyCode::F4 => 118,
        KeyCode::F5 => 96,
        KeyCode::F6 => 97,
        KeyCode::F7 => 98,
        KeyCode::F8 => 100,
        KeyCode::F9 => 101,
        KeyCode::F10 => 109,
        KeyCode::F11 => 103,
        KeyCode::F12 => 111,
        KeyCode::F13 => 105,
        KeyCode::F14 => 107,
        KeyCode::F15 => 113,
        KeyCode::F16 => 106,
        KeyCode::F17 => 64,
        KeyCode::F18 => 79,
        KeyCode::F19 => 80,
        KeyCode::F20 => 90,
        KeyCode::Insert => 114,
        KeyCode::Delete => 117,
        KeyCode::Home => 115,
        KeyCode::End => 119,
        KeyCode::PageUp => 116,
        KeyCode::PageDown => 121,
        KeyCode::ArrowUp => 126,
        KeyCode::ArrowDown => 125,
        KeyCode::ArrowLeft => 123,
        KeyCode::ArrowRight => 124,
        KeyCode::NumLock => 71,
        KeyCode::Numpad0 => 82,
        KeyCode::Numpad1 => 83,
        KeyCode::Numpad2 => 84,
        KeyCode::Numpad3 => 85,
        KeyCode::Numpad4 => 86,
        KeyCode::Numpad5 => 87,
        KeyCode::Numpad6 => 88,
        KeyCode::Numpad7 => 89,
        KeyCode::Numpad8 => 91,
        KeyCode::Numpad9 => 92,
        KeyCode::NumpadAdd => 69,
        KeyCode::NumpadSubtract => 78,
        KeyCode::NumpadMultiply => 67,
        KeyCode::NumpadDivide => 75,
        KeyCode::NumpadDecimal => 65,
        KeyCode::NumpadEnter => 76,
        KeyCode::NumpadEqual => 81,
        KeyCode::PrintScreen
        | KeyCode::ScrollLock
        | KeyCode::Pause
        | KeyCode::F21
        | KeyCode::F22
        | KeyCode::F23
        | KeyCode::F24 => {
            return None
        }
        KeyCode::Unknown(c) => u16::try_from(c).ok()?,
        _ => return None,
    })
}
