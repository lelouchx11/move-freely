//! Native Windows input capture (`WH_KEYBOARD_LL` + `WH_MOUSE_LL` + Raw Input),
//! `SendInput` virtual-desktop injection, Per-Monitor DPI v2 awareness, and
//! multi-monitor display enumeration.

use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Result};
use mf_protocol::{Button, KeyCode, ModifierFlags, Monitor};
use tokio::sync::mpsc;
use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, GetKeyboardState, MapVirtualKeyExW, SendInput, ToUnicodeEx, INPUT,
    INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC_EX, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, VIRTUAL_KEY, VK_ADD, VK_BACK, VK_CAPITAL, VK_DECIMAL, VK_DELETE, VK_DIVIDE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F13, VK_F14, VK_F15, VK_F16,
    VK_F17, VK_F18, VK_F19, VK_F2, VK_F20, VK_F21, VK_F22, VK_F23, VK_F24, VK_F3, VK_F4, VK_F5,
    VK_F6, VK_F7, VK_F8, VK_F9, VK_HOME, VK_INSERT, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT,
    VK_LWIN, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
    VK_MULTIPLY, VK_NEXT, VK_NUMLOCK, VK_NUMPAD0, VK_NUMPAD1, VK_NUMPAD2, VK_NUMPAD3, VK_NUMPAD4,
    VK_NUMPAD5, VK_NUMPAD6, VK_NUMPAD7, VK_NUMPAD8, VK_NUMPAD9, VK_OEM_1, VK_OEM_102, VK_OEM_2,
    VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD,
    VK_OEM_PLUS, VK_PAUSE, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT,
    VK_RWIN, VK_SCROLL, VK_SNAPSHOT, VK_SPACE, VK_SUBTRACT, VK_TAB, VK_UP, VK_VOLUME_DOWN,
    VK_VOLUME_MUTE, VK_VOLUME_UP, VK_APPS,
};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_RELATIVE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetForegroundWindow, GetMessageW, GetSystemMetrics, GetWindowThreadProcessId,
    PostThreadMessageW, RegisterClassW, SetCursorPos, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP,
    LLMHF_INJECTED, MSLLHOOKSTRUCT, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WHEEL_DELTA, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_INPUT,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use super::{
    CaptureController, CaptureSession, HeldInputTracker, InputEvent, ModifierStateTracker,
    PermissionStatus, PlatformInjector, MF_INJECTED_MARKER,
};

const INJECTED_EXTRA: usize = MF_INJECTED_MARKER as usize;
const MONITORINFOF_PRIMARY: u32 = 1;

pub fn init_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

pub fn check_permissions() -> PermissionStatus {
    PermissionStatus {
        accessibility_granted: true,
        input_monitoring_granted: true,
        can_capture: true,
        can_inject: true,
        session_type: "windows".into(),
        summary: "Windows native input capture and SendInput ready (note: elevated UAC/Admin windows require elevated privileges due to Windows UIPI)".into(),
        action_hint: None,
    }
}

pub fn enumerate_monitors() -> Vec<Monitor> {
    init_dpi_awareness();
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let ptr = &mut list as *mut Vec<Monitor> as isize;
        let _ = EnumDisplayMonitors(HDC::default(), None, Some(monitor_enum_cb), LPARAM(ptr));
    }
    if list.is_empty() {
        list.push(super::default_fallback_monitor());
    }
    list
}

unsafe extern "system" fn monitor_enum_cb(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<Monitor>);
    let mut info: MONITORINFOEXW = zeroed();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut _).as_bool() {
        let r = info.monitorInfo.rcMonitor;
        let w = (r.right - r.left).max(1) as u32;
        let h = (r.bottom - r.top).max(1) as u32;
        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let scale = (dpi_x as f64 / 96.0).max(0.5);
        let end = info
            .szDevice
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szDevice.len());
        let dev_name = String::from_utf16_lossy(&info.szDevice[..end]);
        let primary = (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0;
        list.push(Monitor {
            id: dev_name.clone(),
            name: if dev_name.is_empty() {
                format!("Display {}", list.len() + 1)
            } else {
                dev_name
            },
            x: r.left,
            y: r.top,
            width: w,
            height: h,
            physical_width: w,
            physical_height: h,
            scale_factor: scale,
            is_primary: primary,
        });
    }
    BOOL(1)
}

// ---------------------------------------------------------------------------
// Native Capture + Grab (WH_KEYBOARD_LL + WH_MOUSE_LL + Raw Input)
// ---------------------------------------------------------------------------

struct WinHookState {
    tx: mpsc::UnboundedSender<InputEvent>,
    grabbing: AtomicBool,
    mods: Mutex<ModifierStateTracker>,
    last_pos: Mutex<Option<(i32, i32)>>,
    anchor_pos: Mutex<(i32, i32)>,
    thread_id: AtomicU32,
}

static HOOK_STATE: OnceLock<Mutex<Option<Arc<WinHookState>>>> = OnceLock::new();

fn hook_state_cell() -> &'static Mutex<Option<Arc<WinHookState>>> {
    HOOK_STATE.get_or_init(|| Mutex::new(None))
}

struct WinCaptureController {
    state: Arc<WinHookState>,
}

impl CaptureController for WinCaptureController {
    fn set_grabbing(&self, on: bool, warp_to: Option<(i32, i32)>) {
        let prev = self.state.grabbing.swap(on, Ordering::SeqCst);
        if prev == on {
            return;
        }
        unsafe {
            if on {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                *self.state.anchor_pos.lock().unwrap() = (pt.x, pt.y);
            } else if let Some((wx, wy)) = warp_to {
                let _ = SetCursorPos(wx, wy);
                *self.state.last_pos.lock().unwrap() = Some((wx, wy));
            }
        }
    }

    fn is_grabbing(&self) -> bool {
        self.state.grabbing.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.set_grabbing(false, None);
        let tid = self.state.thread_id.load(Ordering::SeqCst);
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }
}

impl Drop for WinCaptureController {
    fn drop(&mut self) {
        self.stop();
    }
}

unsafe extern "system" fn ll_keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let kb = *(lparam.0 as *const KBDLLHOOKSTRUCT);
        if kb.dwExtraInfo == INJECTED_EXTRA {
            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
        }
        let guard = hook_state_cell().lock().unwrap();
        if let Some(state) = guard.as_ref() {
            let msg = wparam.0 as u32;
            let pressed = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) || (kb.flags.0 & LLKHF_UP.0 == 0 && !matches!(msg, WM_KEYUP | WM_SYSKEYUP));
            let ext = (kb.flags.0 & LLKHF_EXTENDED.0) != 0;
            let key = win_vk_to_protocol(kb.vkCode as u16, ext);
            let mods = {
                let mut m = state.mods.lock().unwrap();
                m.update(key, pressed);
                m.flags()
            };
            let text = if pressed && !mods.has_command_modifier() {
                vk_to_unicode(kb.vkCode, kb.scanCode)
            } else {
                None
            };
            let _ = state.tx.send(InputEvent::Key {
                key,
                pressed,
                text,
                modifiers: mods,
            });
            if state.grabbing.load(Ordering::SeqCst) {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

unsafe extern "system" fn ll_mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let ms = *(lparam.0 as *const MSLLHOOKSTRUCT);
        if ms.dwExtraInfo == INJECTED_EXTRA || (ms.flags & LLMHF_INJECTED) != 0 && ms.dwExtraInfo == INJECTED_EXTRA {
            return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
        }
        let guard = hook_state_cell().lock().unwrap();
        if let Some(state) = guard.as_ref() {
            let grabbing = state.grabbing.load(Ordering::SeqCst);
            let msg = wparam.0 as u32;
            match msg {
                WM_MOUSEMOVE => {
                    if !grabbing {
                        let (x, y) = (ms.pt.x, ms.pt.y);
                        let mut last = state.last_pos.lock().unwrap();
                        let (dx, dy) = last.map(|(px, py)| (x - px, y - py)).unwrap_or((0, 0));
                        *last = Some((x, y));
                        if dx != 0 || dy != 0 {
                            let _ = state.tx.send(InputEvent::LocalPointerMove { x, y, dx, dy });
                        }
                    }
                    // When grabbing, WM_INPUT delivers raw unclamped hardware deltas!
                }
                WM_LBUTTONDOWN | WM_LBUTTONUP => {
                    if grabbing {
                        let _ = state.tx.send(InputEvent::MouseButton {
                            button: Button::Left,
                            pressed: msg == WM_LBUTTONDOWN,
                        });
                    }
                }
                WM_RBUTTONDOWN | WM_RBUTTONUP => {
                    if grabbing {
                        let _ = state.tx.send(InputEvent::MouseButton {
                            button: Button::Right,
                            pressed: msg == WM_RBUTTONDOWN,
                        });
                    }
                }
                WM_MBUTTONDOWN | WM_MBUTTONUP => {
                    if grabbing {
                        let _ = state.tx.send(InputEvent::MouseButton {
                            button: Button::Middle,
                            pressed: msg == WM_MBUTTONDOWN,
                        });
                    }
                }
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    if grabbing {
                        let xbtn = ((ms.mouseData >> 16) & 0xFFFF) as u16;
                        let button = match xbtn {
                            1 => Button::Back,
                            2 => Button::Forward,
                            n => Button::Other(n as u8),
                        };
                        let _ = state.tx.send(InputEvent::MouseButton {
                            button,
                            pressed: msg == WM_XBUTTONDOWN,
                        });
                    }
                }
                WM_MOUSEWHEEL => {
                    if grabbing {
                        let raw = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                        let lines = raw as f64 / WHEEL_DELTA as f64;
                        let _ = state.tx.send(InputEvent::Scroll {
                            dx: 0.0,
                            dy: lines,
                            continuous: false,
                        });
                    }
                }
                WM_MOUSEHWHEEL => {
                    if grabbing {
                        let raw = ((ms.mouseData >> 16) & 0xFFFF) as i16;
                        let lines = raw as f64 / WHEEL_DELTA as f64;
                        let _ = state.tx.send(InputEvent::Scroll {
                            dx: lines,
                            dy: 0.0,
                            continuous: false,
                        });
                    }
                }
                _ => {}
            }
            if grabbing {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

unsafe extern "system" fn raw_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT {
        let mut size: u32 = 0;
        let hdr_size = size_of::<RAWINPUTHEADER>() as u32;
        GetRawInputData(HRAWINPUT(lparam.0 as *mut _), RID_INPUT, None, &mut size, hdr_size);
        if size > 0 {
            let mut buf = vec![0u8; size as usize];
            if GetRawInputData(
                HRAWINPUT(lparam.0 as *mut _),
                RID_INPUT,
                Some(buf.as_mut_ptr() as *mut _),
                &mut size,
                hdr_size,
            ) == size
            {
                let raw = &*(buf.as_ptr() as *const RAWINPUT);
                if raw.header.dwType == RIM_TYPEMOUSE.0 {
                    let mouse = raw.data.mouse;
                    if (mouse.usFlags.0 & MOUSE_MOVE_RELATIVE.0) == MOUSE_MOVE_RELATIVE.0 || mouse.usFlags.0 == 0 {
                        let dx = mouse.lLastX;
                        let dy = mouse.lLastY;
                        if dx != 0 || dy != 0 {
                            let guard = hook_state_cell().lock().unwrap();
                            if let Some(state) = guard.as_ref() {
                                if state.grabbing.load(Ordering::SeqCst) {
                                    let _ = state.tx.send(InputEvent::RemotePointerDelta { dx, dy });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub fn start_capture() -> Result<CaptureSession> {
    init_dpi_awareness();
    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(WinHookState {
        tx,
        grabbing: AtomicBool::new(false),
        mods: Mutex::new(ModifierStateTracker::default()),
        last_pos: Mutex::new(None),
        anchor_pos: Mutex::new((0, 0)),
        thread_id: AtomicU32::new(0),
    });

    *hook_state_cell().lock().unwrap() = Some(state.clone());

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let thread_state = state.clone();

    std::thread::Builder::new()
        .name("mf-windows-capture".into())
        .spawn(move || unsafe {
            let tid = windows::Win32::System::Threading::GetCurrentThreadId();
            thread_state.thread_id.store(tid, Ordering::SeqCst);

            let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinst = HINSTANCE(hmod.0);
            let cls_name = w!("MoveFreelyRawInput");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(raw_wnd_proc),
                hInstance: hinst,
                lpszClassName: cls_name,
                ..zeroed()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                Default::default(),
                cls_name,
                w!("MoveFreelyHookWnd"),
                Default::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                hinst,
                None,
            )
            .unwrap_or_default();

            if !hwnd.0.is_null() {
                let rid = RAWINPUTDEVICE {
                    usUsagePage: 0x01,
                    usUsage: 0x02, // Mouse
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                };
                let _ = RegisterRawInputDevices(&[rid], size_of::<RAWINPUTDEVICE>() as u32);
            }

            let kb_hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), hinst, 0) {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("WH_KEYBOARD_LL failed: {e}")));
                    return;
                }
            };
            let ms_hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(ll_mouse_proc), hinst, 0) {
                Ok(h) => h,
                Err(e) => {
                    let _ = UnhookWindowsHookEx(kb_hook);
                    let _ = ready_tx.send(Err(anyhow!("WH_MOUSE_LL failed: {e}")));
                    return;
                }
            };

            let _ = ready_tx.send(Ok(()));

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let _ = UnhookWindowsHookEx(kb_hook);
            let _ = UnhookWindowsHookEx(ms_hook);
            if !hwnd.0.is_null() {
                let _ = DestroyWindow(hwnd);
            }
            *hook_state_cell().lock().unwrap() = None;
        })?;

    ready_rx
        .recv()
        .map_err(|_| anyhow!("Windows capture thread exited prematurely"))??;

    Ok(CaptureSession {
        events: rx,
        controller: Arc::new(WinCaptureController { state }),
    })
}

fn vk_to_unicode(vk: u32, scan: u32) -> Option<String> {
    unsafe {
        let mut kb_state = [0u8; 256];
        if GetKeyboardState(&mut kb_state).is_err() {
            return None;
        }
        let fg = GetForegroundWindow();
        let tid = GetWindowThreadProcessId(fg, None);
        let hkl = GetKeyboardLayout(tid);
        let mut buf = [0u16; 8];
        let count = ToUnicodeEx(vk, scan, &kb_state, &mut buf, 0, hkl);
        if count > 0 {
            let s = String::from_utf16_lossy(&buf[..count as usize]);
            if !s.chars().all(|c| c.is_control()) {
                return Some(s);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Native Windows Injection (`SendInput` + Virtual Desktop Multi-Monitor)
// ---------------------------------------------------------------------------

pub struct WinInjector {
    held: HeldInputTracker,
}

impl WinInjector {
    pub fn new() -> Result<Self> {
        init_dpi_awareness();
        Ok(Self {
            held: HeldInputTracker::default(),
        })
    }

    fn send(inputs: &[INPUT]) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if sent != inputs.len() as u32 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("SendInput failed ({err}); target window may be elevated (UIPI)");
        }
        Ok(())
    }

    fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32, dx: i32, dy: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_EXTRA,
                },
            },
        }
    }

    fn key_input(flags: KEYBD_EVENT_FLAGS, vk: VIRTUAL_KEY, scan: u16) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECTED_EXTRA,
                },
            },
        }
    }
}

impl Drop for WinInjector {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

impl PlatformInjector for WinInjector {
    fn inject_pointer_abs(&mut self, x: i32, y: i32, _dx: i32, _dy: i32) -> Result<()> {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

            let nx = (((x - vx) as i64 * 65535) / (vw - 1).max(1) as i64).clamp(0, 65535) as i32;
            let ny = (((y - vy) as i64 * 65535) / (vh - 1).max(1) as i64).clamp(0, 65535) as i32;

            Self::send(&[Self::mouse_input(
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                0,
                nx,
                ny,
            )])
        }
    }

    fn inject_pointer_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        Self::send(&[Self::mouse_input(MOUSEEVENTF_MOVE, 0, dx, dy)])
    }

    fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()> {
        self.held.record_button(button, pressed);
        let (flag, data) = match (button, pressed) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, 1),
            (Button::Back, false) => (MOUSEEVENTF_XUP, 1),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, 2),
            (Button::Other(_), _) => return Ok(()),
        };
        Self::send(&[Self::mouse_input(flag, data, 0, 0)])
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64, continuous: bool) -> Result<()> {
        let mult = if continuous { 4.0 } else { WHEEL_DELTA as f64 };
        let mut inputs = Vec::new();
        let vy = (dy * mult).round() as i32;
        let hx = (dx * mult).round() as i32;
        if vy != 0 {
            inputs.push(Self::mouse_input(MOUSEEVENTF_WHEEL, vy, 0, 0));
        }
        if hx != 0 {
            inputs.push(Self::mouse_input(MOUSEEVENTF_HWHEEL, hx, 0, 0));
        }
        Self::send(&inputs)
    }

    fn inject_key(
        &mut self,
        key: KeyCode,
        pressed: bool,
        text: Option<&str>,
        modifiers: ModifierFlags,
    ) -> Result<()> {
        self.held.record_key(key, pressed);

        if let KeyCode::Unicode(ch) = key {
            if pressed {
                let mut buf = [0u16; 2];
                let units = ch.encode_utf16(&mut buf);
                let mut inputs = Vec::new();
                for &u in units {
                    inputs.push(Self::key_input(KEYEVENTF_UNICODE, VIRTUAL_KEY(0), u));
                    inputs.push(Self::key_input(
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                        VIRTUAL_KEY(0),
                        u,
                    ));
                }
                Self::send(&inputs)?;
            }
            return Ok(());
        }

        // Inject non-ASCII characters via KEYEVENTF_UNICODE when no command modifier is held.
        if pressed && !modifiers.has_command_modifier() {
            if let Some(txt) = text {
                if !txt.is_empty() && !txt.is_ascii() {
                    let mut inputs = Vec::new();
                    for u in txt.encode_utf16() {
                        inputs.push(Self::key_input(KEYEVENTF_UNICODE, VIRTUAL_KEY(0), u));
                        inputs.push(Self::key_input(
                            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                            VIRTUAL_KEY(0),
                            u,
                        ));
                    }
                    return Self::send(&inputs);
                }
            }
        }

        if let Some((vk, extended)) = protocol_to_win_vk(key) {
            let scan = unsafe {
                let hkl = GetKeyboardLayout(0);
                MapVirtualKeyExW(vk.0 as u32, MAPVK_VK_TO_VSC_EX, hkl) as u16
            };
            let mut flags = KEYBD_EVENT_FLAGS::default();
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !pressed {
                flags |= KEYEVENTF_KEYUP;
            }
            Self::send(&[Self::key_input(flags, vk, scan)])?;
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

fn win_vk_to_protocol(vk: u16, extended: bool) -> KeyCode {
    let v = VIRTUAL_KEY(vk);
    match v {
        k if k == VIRTUAL_KEY(0x41) => KeyCode::KeyA,
        k if k == VIRTUAL_KEY(0x42) => KeyCode::KeyB,
        k if k == VIRTUAL_KEY(0x43) => KeyCode::KeyC,
        k if k == VIRTUAL_KEY(0x44) => KeyCode::KeyD,
        k if k == VIRTUAL_KEY(0x45) => KeyCode::KeyE,
        k if k == VIRTUAL_KEY(0x46) => KeyCode::KeyF,
        k if k == VIRTUAL_KEY(0x47) => KeyCode::KeyG,
        k if k == VIRTUAL_KEY(0x48) => KeyCode::KeyH,
        k if k == VIRTUAL_KEY(0x49) => KeyCode::KeyI,
        k if k == VIRTUAL_KEY(0x4A) => KeyCode::KeyJ,
        k if k == VIRTUAL_KEY(0x4B) => KeyCode::KeyK,
        k if k == VIRTUAL_KEY(0x4C) => KeyCode::KeyL,
        k if k == VIRTUAL_KEY(0x4D) => KeyCode::KeyM,
        k if k == VIRTUAL_KEY(0x4E) => KeyCode::KeyN,
        k if k == VIRTUAL_KEY(0x4F) => KeyCode::KeyO,
        k if k == VIRTUAL_KEY(0x50) => KeyCode::KeyP,
        k if k == VIRTUAL_KEY(0x51) => KeyCode::KeyQ,
        k if k == VIRTUAL_KEY(0x52) => KeyCode::KeyR,
        k if k == VIRTUAL_KEY(0x53) => KeyCode::KeyS,
        k if k == VIRTUAL_KEY(0x54) => KeyCode::KeyT,
        k if k == VIRTUAL_KEY(0x55) => KeyCode::KeyU,
        k if k == VIRTUAL_KEY(0x56) => KeyCode::KeyV,
        k if k == VIRTUAL_KEY(0x57) => KeyCode::KeyW,
        k if k == VIRTUAL_KEY(0x58) => KeyCode::KeyX,
        k if k == VIRTUAL_KEY(0x59) => KeyCode::KeyY,
        k if k == VIRTUAL_KEY(0x5A) => KeyCode::KeyZ,
        k if k == VIRTUAL_KEY(0x30) => KeyCode::Digit0,
        k if k == VIRTUAL_KEY(0x31) => KeyCode::Digit1,
        k if k == VIRTUAL_KEY(0x32) => KeyCode::Digit2,
        k if k == VIRTUAL_KEY(0x33) => KeyCode::Digit3,
        k if k == VIRTUAL_KEY(0x34) => KeyCode::Digit4,
        k if k == VIRTUAL_KEY(0x35) => KeyCode::Digit5,
        k if k == VIRTUAL_KEY(0x36) => KeyCode::Digit6,
        k if k == VIRTUAL_KEY(0x37) => KeyCode::Digit7,
        k if k == VIRTUAL_KEY(0x38) => KeyCode::Digit8,
        k if k == VIRTUAL_KEY(0x39) => KeyCode::Digit9,
        k if k == VK_OEM_MINUS => KeyCode::Minus,
        k if k == VK_OEM_PLUS => KeyCode::Equal,
        k if k == VK_OEM_4 => KeyCode::BracketLeft,
        k if k == VK_OEM_6 => KeyCode::BracketRight,
        k if k == VK_OEM_5 => KeyCode::Backslash,
        k if k == VK_OEM_1 => KeyCode::Semicolon,
        k if k == VK_OEM_7 => KeyCode::Quote,
        k if k == VK_OEM_3 => KeyCode::Backquote,
        k if k == VK_OEM_COMMA => KeyCode::Comma,
        k if k == VK_OEM_PERIOD => KeyCode::Period,
        k if k == VK_OEM_2 => KeyCode::Slash,
        k if k == VK_OEM_102 => KeyCode::IntlBackslash,
        k if k == VK_RETURN && extended => KeyCode::NumpadEnter,
        k if k == VK_RETURN => KeyCode::Enter,
        k if k == VK_ESCAPE => KeyCode::Escape,
        k if k == VK_BACK => KeyCode::Backspace,
        k if k == VK_TAB => KeyCode::Tab,
        k if k == VK_SPACE => KeyCode::Space,
        k if k == VK_CAPITAL => KeyCode::CapsLock,
        k if k == VK_LSHIFT => KeyCode::ShiftLeft,
        k if k == VK_RSHIFT => KeyCode::ShiftRight,
        k if k == VK_LCONTROL => KeyCode::ControlLeft,
        k if k == VK_RCONTROL => KeyCode::ControlRight,
        k if k == VK_LMENU => KeyCode::AltLeft,
        k if k == VK_RMENU => KeyCode::AltRight,
        k if k == VK_LWIN => KeyCode::MetaLeft,
        k if k == VK_RWIN => KeyCode::MetaRight,
        k if k == VK_APPS => KeyCode::ContextMenu,
        k if k == VK_F1 => KeyCode::F1,
        k if k == VK_F2 => KeyCode::F2,
        k if k == VK_F3 => KeyCode::F3,
        k if k == VK_F4 => KeyCode::F4,
        k if k == VK_F5 => KeyCode::F5,
        k if k == VK_F6 => KeyCode::F6,
        k if k == VK_F7 => KeyCode::F7,
        k if k == VK_F8 => KeyCode::F8,
        k if k == VK_F9 => KeyCode::F9,
        k if k == VK_F10 => KeyCode::F10,
        k if k == VK_F11 => KeyCode::F11,
        k if k == VK_F12 => KeyCode::F12,
        k if k == VK_F13 => KeyCode::F13,
        k if k == VK_F14 => KeyCode::F14,
        k if k == VK_F15 => KeyCode::F15,
        k if k == VK_F16 => KeyCode::F16,
        k if k == VK_F17 => KeyCode::F17,
        k if k == VK_F18 => KeyCode::F18,
        k if k == VK_F19 => KeyCode::F19,
        k if k == VK_F20 => KeyCode::F20,
        k if k == VK_F21 => KeyCode::F21,
        k if k == VK_F22 => KeyCode::F22,
        k if k == VK_F23 => KeyCode::F23,
        k if k == VK_F24 => KeyCode::F24,
        k if k == VK_SNAPSHOT => KeyCode::PrintScreen,
        k if k == VK_SCROLL => KeyCode::ScrollLock,
        k if k == VK_PAUSE => KeyCode::Pause,
        k if k == VK_INSERT => KeyCode::Insert,
        k if k == VK_DELETE => KeyCode::Delete,
        k if k == VK_HOME => KeyCode::Home,
        k if k == VK_END => KeyCode::End,
        k if k == VK_PRIOR => KeyCode::PageUp,
        k if k == VK_NEXT => KeyCode::PageDown,
        k if k == VK_UP => KeyCode::ArrowUp,
        k if k == VK_DOWN => KeyCode::ArrowDown,
        k if k == VK_LEFT => KeyCode::ArrowLeft,
        k if k == VK_RIGHT => KeyCode::ArrowRight,
        k if k == VK_NUMLOCK => KeyCode::NumLock,
        k if k == VK_NUMPAD0 => KeyCode::Numpad0,
        k if k == VK_NUMPAD1 => KeyCode::Numpad1,
        k if k == VK_NUMPAD2 => KeyCode::Numpad2,
        k if k == VK_NUMPAD3 => KeyCode::Numpad3,
        k if k == VK_NUMPAD4 => KeyCode::Numpad4,
        k if k == VK_NUMPAD5 => KeyCode::Numpad5,
        k if k == VK_NUMPAD6 => KeyCode::Numpad6,
        k if k == VK_NUMPAD7 => KeyCode::Numpad7,
        k if k == VK_NUMPAD8 => KeyCode::Numpad8,
        k if k == VK_NUMPAD9 => KeyCode::Numpad9,
        k if k == VK_ADD => KeyCode::NumpadAdd,
        k if k == VK_SUBTRACT => KeyCode::NumpadSubtract,
        k if k == VK_MULTIPLY => KeyCode::NumpadMultiply,
        k if k == VK_DIVIDE => KeyCode::NumpadDivide,
        k if k == VK_DECIMAL => KeyCode::NumpadDecimal,
        k if k == VK_VOLUME_MUTE => KeyCode::AudioVolumeMute,
        k if k == VK_VOLUME_DOWN => KeyCode::AudioVolumeDown,
        k if k == VK_VOLUME_UP => KeyCode::AudioVolumeUp,
        k if k == VK_MEDIA_PLAY_PAUSE => KeyCode::MediaPlayPause,
        k if k == VK_MEDIA_STOP => KeyCode::MediaStop,
        k if k == VK_MEDIA_NEXT_TRACK => KeyCode::MediaTrackNext,
        k if k == VK_MEDIA_PREV_TRACK => KeyCode::MediaTrackPrevious,
        _ => KeyCode::Unknown(vk as u32),
    }
}

fn protocol_to_win_vk(key: KeyCode) -> Option<(VIRTUAL_KEY, bool)> {
    Some(match key {
        KeyCode::KeyA => (VIRTUAL_KEY(0x41), false),
        KeyCode::KeyB => (VIRTUAL_KEY(0x42), false),
        KeyCode::KeyC => (VIRTUAL_KEY(0x43), false),
        KeyCode::KeyD => (VIRTUAL_KEY(0x44), false),
        KeyCode::KeyE => (VIRTUAL_KEY(0x45), false),
        KeyCode::KeyF => (VIRTUAL_KEY(0x46), false),
        KeyCode::KeyG => (VIRTUAL_KEY(0x47), false),
        KeyCode::KeyH => (VIRTUAL_KEY(0x48), false),
        KeyCode::KeyI => (VIRTUAL_KEY(0x49), false),
        KeyCode::KeyJ => (VIRTUAL_KEY(0x4A), false),
        KeyCode::KeyK => (VIRTUAL_KEY(0x4B), false),
        KeyCode::KeyL => (VIRTUAL_KEY(0x4C), false),
        KeyCode::KeyM => (VIRTUAL_KEY(0x4D), false),
        KeyCode::KeyN => (VIRTUAL_KEY(0x4E), false),
        KeyCode::KeyO => (VIRTUAL_KEY(0x4F), false),
        KeyCode::KeyP => (VIRTUAL_KEY(0x50), false),
        KeyCode::KeyQ => (VIRTUAL_KEY(0x51), false),
        KeyCode::KeyR => (VIRTUAL_KEY(0x52), false),
        KeyCode::KeyS => (VIRTUAL_KEY(0x53), false),
        KeyCode::KeyT => (VIRTUAL_KEY(0x54), false),
        KeyCode::KeyU => (VIRTUAL_KEY(0x55), false),
        KeyCode::KeyV => (VIRTUAL_KEY(0x56), false),
        KeyCode::KeyW => (VIRTUAL_KEY(0x57), false),
        KeyCode::KeyX => (VIRTUAL_KEY(0x58), false),
        KeyCode::KeyY => (VIRTUAL_KEY(0x59), false),
        KeyCode::KeyZ => (VIRTUAL_KEY(0x5A), false),
        KeyCode::Digit0 => (VIRTUAL_KEY(0x30), false),
        KeyCode::Digit1 => (VIRTUAL_KEY(0x31), false),
        KeyCode::Digit2 => (VIRTUAL_KEY(0x32), false),
        KeyCode::Digit3 => (VIRTUAL_KEY(0x33), false),
        KeyCode::Digit4 => (VIRTUAL_KEY(0x34), false),
        KeyCode::Digit5 => (VIRTUAL_KEY(0x35), false),
        KeyCode::Digit6 => (VIRTUAL_KEY(0x36), false),
        KeyCode::Digit7 => (VIRTUAL_KEY(0x37), false),
        KeyCode::Digit8 => (VIRTUAL_KEY(0x38), false),
        KeyCode::Digit9 => (VIRTUAL_KEY(0x39), false),
        KeyCode::Minus => (VK_OEM_MINUS, false),
        KeyCode::Equal => (VK_OEM_PLUS, false),
        KeyCode::BracketLeft => (VK_OEM_4, false),
        KeyCode::BracketRight => (VK_OEM_6, false),
        KeyCode::Backslash => (VK_OEM_5, false),
        KeyCode::Semicolon => (VK_OEM_1, false),
        KeyCode::Quote => (VK_OEM_7, false),
        KeyCode::Backquote => (VK_OEM_3, false),
        KeyCode::Comma => (VK_OEM_COMMA, false),
        KeyCode::Period => (VK_OEM_PERIOD, false),
        KeyCode::Slash => (VK_OEM_2, false),
        KeyCode::IntlBackslash => (VK_OEM_102, false),
        KeyCode::Enter => (VK_RETURN, false),
        KeyCode::Escape => (VK_ESCAPE, false),
        KeyCode::Backspace => (VK_BACK, false),
        KeyCode::Tab => (VK_TAB, false),
        KeyCode::Space => (VK_SPACE, false),
        KeyCode::CapsLock => (VK_CAPITAL, false),
        KeyCode::ShiftLeft => (VK_LSHIFT, false),
        KeyCode::ShiftRight => (VK_RSHIFT, false),
        KeyCode::ControlLeft => (VK_LCONTROL, false),
        KeyCode::ControlRight => (VK_RCONTROL, true),
        KeyCode::AltLeft => (VK_LMENU, false),
        KeyCode::AltRight => (VK_RMENU, true),
        KeyCode::MetaLeft => (VK_LWIN, false),
        KeyCode::MetaRight => (VK_RWIN, false),
        KeyCode::ContextMenu => (VK_APPS, true),
        KeyCode::F1 => (VK_F1, false),
        KeyCode::F2 => (VK_F2, false),
        KeyCode::F3 => (VK_F3, false),
        KeyCode::F4 => (VK_F4, false),
        KeyCode::F5 => (VK_F5, false),
        KeyCode::F6 => (VK_F6, false),
        KeyCode::F7 => (VK_F7, false),
        KeyCode::F8 => (VK_F8, false),
        KeyCode::F9 => (VK_F9, false),
        KeyCode::F10 => (VK_F10, false),
        KeyCode::F11 => (VK_F11, false),
        KeyCode::F12 => (VK_F12, false),
        KeyCode::F13 => (VK_F13, false),
        KeyCode::F14 => (VK_F14, false),
        KeyCode::F15 => (VK_F15, false),
        KeyCode::F16 => (VK_F16, false),
        KeyCode::F17 => (VK_F17, false),
        KeyCode::F18 => (VK_F18, false),
        KeyCode::F19 => (VK_F19, false),
        KeyCode::F20 => (VK_F20, false),
        KeyCode::F21 => (VK_F21, false),
        KeyCode::F22 => (VK_F22, false),
        KeyCode::F23 => (VK_F23, false),
        KeyCode::F24 => (VK_F24, false),
        KeyCode::PrintScreen => (VK_SNAPSHOT, true),
        KeyCode::ScrollLock => (VK_SCROLL, false),
        KeyCode::Pause => (VK_PAUSE, false),
        KeyCode::Insert => (VK_INSERT, true),
        KeyCode::Delete => (VK_DELETE, true),
        KeyCode::Home => (VK_HOME, true),
        KeyCode::End => (VK_END, true),
        KeyCode::PageUp => (VK_PRIOR, true),
        KeyCode::PageDown => (VK_NEXT, true),
        KeyCode::ArrowUp => (VK_UP, true),
        KeyCode::ArrowDown => (VK_DOWN, true),
        KeyCode::ArrowLeft => (VK_LEFT, true),
        KeyCode::ArrowRight => (VK_RIGHT, true),
        KeyCode::NumLock => (VK_NUMLOCK, true),
        KeyCode::Numpad0 => (VK_NUMPAD0, false),
        KeyCode::Numpad1 => (VK_NUMPAD1, false),
        KeyCode::Numpad2 => (VK_NUMPAD2, false),
        KeyCode::Numpad3 => (VK_NUMPAD3, false),
        KeyCode::Numpad4 => (VK_NUMPAD4, false),
        KeyCode::Numpad5 => (VK_NUMPAD5, false),
        KeyCode::Numpad6 => (VK_NUMPAD6, false),
        KeyCode::Numpad7 => (VK_NUMPAD7, false),
        KeyCode::Numpad8 => (VK_NUMPAD8, false),
        KeyCode::Numpad9 => (VK_NUMPAD9, false),
        KeyCode::NumpadAdd => (VK_ADD, false),
        KeyCode::NumpadSubtract => (VK_SUBTRACT, false),
        KeyCode::NumpadMultiply => (VK_MULTIPLY, false),
        KeyCode::NumpadDivide => (VK_DIVIDE, true),
        KeyCode::NumpadDecimal => (VK_DECIMAL, false),
        KeyCode::NumpadEnter => (VK_RETURN, true),
        KeyCode::NumpadEqual => (VK_OEM_PLUS, false),
        KeyCode::AudioVolumeMute => (VK_VOLUME_MUTE, false),
        KeyCode::AudioVolumeDown => (VK_VOLUME_DOWN, false),
        KeyCode::AudioVolumeUp => (VK_VOLUME_UP, false),
        KeyCode::MediaPlayPause => (VK_MEDIA_PLAY_PAUSE, false),
        KeyCode::MediaStop => (VK_MEDIA_STOP, false),
        KeyCode::MediaTrackNext => (VK_MEDIA_NEXT_TRACK, false),
        KeyCode::MediaTrackPrevious => (VK_MEDIA_PREV_TRACK, false),
        KeyCode::Unknown(c) => (VIRTUAL_KEY(u16::try_from(c).ok()?), false),
        KeyCode::Unicode(_) => return None,
    })
}
