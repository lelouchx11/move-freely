# Move Freely

Share keyboard, mouse, and clipboard seamlessly across **macOS** (Apple Silicon & Intel), **Windows 10/11**, and **Linux** (X11 & Wayland) over your local Wi-Fi or Ethernet network — no extra hardware or cloud servers required.

## Highlights

- **Bidirectional Peer-to-Peer Control**: Every device can act as controller or controlled machine and switch roles dynamically via the UI, System Tray/Menu Bar, or global shortcuts.
- **Interactive 2D Screen Arrangement**: macOS Display Settings–style drag-and-drop canvas supporting horizontal, vertical, multi-monitor, mixed-DPI, edge resistance, corner deadzones, and per-edge transition toggles.
- **Universal Encrypted Clipboard**: Syncs plain text, multilingual Unicode, rich HTML/RTF, lossless PNG images, and copied files/folders over a dedicated bulk TLS channel with SHA-256 verification and ephemeral staging cleanup.
- **LAN Security & Discovery**: Automatic mDNS discovery (`_move-freely._tcp.local.`) + manual IP fallback, mutual TLS (ECDSA P-256), channel-bound 6-digit PIN pairing with explicit user consent, trusted peer revocation, and LAN-only IP enforcement.
- **Low Latency & Reliability**: Priority queuing keeps input events ahead of bulk clipboard transfers, tracks held keys/buttons to prevent stuck modifiers on disconnect, and reconnects automatically across sleep/wake/IP changes.

## Quick Start

### Desktop Application (UI + Menu Bar / System Tray)

```sh
# 1. Install frontend dependencies and build the UI bundle
npm --prefix ui install
npm --prefix ui run build

# 2. Run the desktop application in development mode
./ui/node_modules/.bin/tauri dev --config crates/desktop/tauri.conf.json

# Or build native installers (.dmg/.app, .msi/.exe, .AppImage/.deb)
./ui/node_modules/.bin/tauri build --config crates/desktop/tauri.conf.json
```

### Headless CLI & Daemon (`mf`)

```sh
# Build and run all unit + integration tests
cargo test --workspace

# Start a peer daemon with an active pairing PIN
cargo run -p mf -- daemon --pin auto

# Pair from another computer
cargo run -p mf -- pair 192.168.1.42:24800 --pin <6-digit-pin>

# Inspect OS permissions and session capabilities
cargo run -p mf -- permissions

# Verify local input injection
cargo run -p mf -- selftest
```

### Legacy Demo / Echo Pipe Verification

```sh
# Terminal A
cargo run -p mf -- serve --config config/layout.example.toml --demo

# Terminal B
cargo run -p mf -- connect 127.0.0.1:24800 --name winbox --token change-me --width 1920 --height 1080 --echo
```

## Default Shortcuts

| Action | Default Shortcut |
| --- | --- |
| Cycle Active Screen | `F12` |
| Claim Controller (This Device) | `Ctrl+Alt+Shift+C` |
| Emergency Release Control | `Ctrl+Alt+Shift+Escape` |
| Pause / Resume Sharing | `Ctrl+Alt+Shift+P` |

All shortcuts are customizable in **Settings -> Keyboard Shortcuts**.

## Documentation

- [Architecture & Ownership Model](docs/ARCHITECTURE.md)
- [Wire Protocol, Security & Layout Math](docs/PROTOCOL.md)
- [Platform Implementation, OS Permissions & Troubleshooting](docs/PLATFORMS_AND_PERMISSIONS.md)
