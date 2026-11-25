# Platform Implementation, Permissions & Troubleshooting

## macOS (Apple Silicon & Intel)

- **Capture & Grab**: Native `CGEventTap` (`HID` location, active filter) captures mouse moves/drags, left/right/middle/back/forward buttons, continuous & discrete scroll wheel, keys, and `FlagsChanged` modifiers. Automatically re-enables itself on `kCGEventTapDisabledByTimeout`.
- **Cursor Pinning**: When controlling a remote screen, `CGAssociateMouseAndMouseCursorPosition(false)` + `CGDisplayHideCursor` keeps the local cursor hidden while preserving raw hardware `MOUSE_EVENT_DELTA_X/Y`.
- **Required Permissions** (System Settings -> Privacy & Security):
  1. **Accessibility**: Required for `CGEventPost` injection and `CGEventTap` interception.
  2. **Input Monitoring**: Required to observe global keyboard/mouse events.
  - Check status via Settings -> Security & Permissions or `mf permissions`.
  - After granting permissions in macOS System Settings, relaunch Move Freely (or your terminal if using the CLI).

## Windows 10 & Windows 11

- **DPI Awareness**: Calls `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` on startup so multi-monitor virtual desktop coordinates and per-monitor DPI scaling are exact.
- **Display Enumeration**: `EnumDisplayMonitors` + `GetMonitorInfoW` + `GetDpiForMonitor`.
- **Capture & Injection**: `WH_KEYBOARD_LL` + `WH_MOUSE_LL` + Raw Input (`WM_INPUT` with `RIDEV_INPUTSINK`) capture unaccelerated hardware deltas; `SendInput` injects virtual desktop absolute coordinates (`MOUSEEVENTF_VIRTUALDESK`), extended scan codes (`KEYEVENTF_EXTENDEDKEY`), and Unicode UTF-16 events (`KEYEVENTF_UNICODE`).
- **UIPI Restriction**: Windows User Interface Privilege Isolation (UIPI) blocks a standard (non-elevated) process from injecting input into an Administrator/elevated window (such as Task Manager or an Admin Command Prompt). To control elevated Windows apps remotely, launch Move Freely as Administrator or via a signed UIAccess installation.

## Linux (X11 & Wayland)

### X11
- Uses pure-Rust `x11rb` (`XRandR` multi-monitor enumeration, `XInput2` raw events + pointer/keyboard grab only while controlling remotely, `XTest` injection, `arboard` + `xclip` file URI clipboard).
- No `/dev/input` or root privileges are required under X11.

### Wayland
- Wayland security architecture isolates global pointer/keyboard capture from unprivileged clients.
- **As a Controlled Device**: Supported via XDG Desktop Portal `RemoteDesktop` + `libei` (`ashpd` + `reis`), with automatic fallback to XWayland `XTest` when `DISPLAY` is available. The desktop environment will display a system consent prompt on first remote-desktop session initialization.
- **As a Controller**: Global input grabbing across arbitrary windows is restricted by Wayland compositors unless `org.freedesktop.portal.InputCapture` is exposed and granted by the compositor. Move Freely clearly detects and communicates Wayland session restrictions in both the Dashboard UI and `mf permissions` rather than silently failing.
- **Clipboard**: Uses `arboard` with `wayland-data-control` (plus `wl-copy`/`wl-paste` for `text/uri-list` file transfers where supported by the compositor).

## Troubleshooting

- **mDNS doesn't discover a peer across VLANs/subnets**: Use **Manual IP Connection & Pairing** in the Devices tab (`<peer-ip>:24800`) and verify TCP port `24800` is allowed in the OS firewall (macOS Firewall / Windows Defender Firewall / `ufw`).
- **Stuck modifier key or trapped cursor**: Press the **Emergency Release** shortcut (`Ctrl+Alt+Shift+Escape` by default) or click **Emergency Release** in the tray/sidebar.
- **Clipboard file paste does not appear on Linux**: Ensure `xclip` (X11) or `wl-clipboard` (Wayland) is installed (`sudo apt install xclip` or `wl-clipboard`).
