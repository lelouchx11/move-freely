# Move Freely Architecture

## Overview

Move Freely is a peer-to-peer software KVM enabling seamless bidirectional keyboard, mouse, and universal clipboard sharing across macOS (Apple Silicon & Intel), Windows 10/11, and Linux (X11 & supported Wayland) over a local Wi-Fi or Ethernet network without external servers or internet access.

## Workspace Layout

- `crates/protocol` (`mf-protocol`): OS-agnostic wire protocol (`PROTOCOL_VERSION = 2`), length-prefixed framing (`0x01` JSON control frames + `0x02` binary bulk transfer chunks), normalized `KeyCode` / `Button` / `ModifierFlags` definitions, and 2D multi-monitor DPI-normalized geometry & edge-resistance math (`ClusterTopology`).
- `crates/core` (`mf-core`): Core runtime engine (`MoveFreelyEngine`), persistent X.509/ECDSA P-256 device identity & mTLS verification (`security`), mDNS LAN discovery (`discovery`), universal clipboard & ephemeral file staging (`clipboard`), native OS input & display backends (`input`), launch-on-startup registration (`autostart`), and TOML configuration (`config`).
- `crates/app` (`mf`): Headless CLI and daemon (`mf daemon`, `mf pair`, `mf selftest`, `mf permissions`, plus backward-compatible `mf serve` and `mf connect`).
- `crates/desktop` (`move-freely`): Tauri v2 desktop application with dynamic System Tray / Menu Bar controls and close-to-tray background operation.
- `ui/`: React + TypeScript + Vite desktop interface (Dashboard, interactive macOS-style Screen Arrangement canvas, Device Management & PIN Pairing, Settings).

## Dynamic Peer-to-Peer Topology & Input Ownership

Every Move Freely instance runs the exact same peer node:

1. **Dual-Role Capability**: Any node can act simultaneously as a TLS listener and outbound dialer. When two paired peers dial each other concurrently, a deterministic tie-breaker (`local_device_id < remote_device_id`) preserves a single canonical control connection.
2. **Input Ownership Epoch**: Active controller state is ordered globally by `(epoch: u64, controller_id: DeviceId)`. Higher `(epoch, controller_id)` deterministically resolves competing controller claims, prevents split-brain cursor fighting, and forces the previous controller to release any held keys and ungrab local peripherals immediately.
3. **Simultaneous Local Peripherals & Feedback Loop Prevention**:
   - Controlled peers never lock their own physical keyboard/mouse.
   - All remotely injected events carry a platform-specific source marker (`MF_INJECTED_MARKER` in `EVENT_SOURCE_USER_DATA` on macOS, `dwExtraInfo` on Windows, XTest slave-device exclusion on Linux X11), ensuring local capture never re-routes injected events.
   - When a controlled peer's local physical mouse moves, it sends rate-limited `LocalCursorSync` updates back to the active controller so virtual pointer coordinates stay aligned without jumping.
4. **Priority Channel Separation**:
   - **Control TLS Stream** (`TCP_NODELAY`): Carries real-time input, `Enter`/`Leave`, ownership updates, and heartbeats with a 2-lane priority queue (`high_tx` preempts `normal_tx`).
   - **Data TLS Stream**: Dedicated companion TLS connection for chunked PNG images and clipboard file bundles, preventing head-of-line blocking on keyboard/mouse latency.
5. **Safety Guarantees**:
   - `HeldInputTracker` records every pressed mouse button and key and releases them in safe order (buttons -> regular keys -> modifiers) upon `Leave`, peer disconnect, controller switch, or the Emergency Release shortcut (`Ctrl+Alt+Shift+Escape`).

## Universal Clipboard & Ephemeral File Transfer

- Supports plain text, Unicode/multilingual text, HTML, RTF, lossless PNG images, and file/folder bundles.
- **Loop & Duplicate Prevention**: Every snapshot is SHA-256 hashed. `ClipboardSyncCoordinator` tracks both `last_local_hash` and `last_injected_hash` so writing a remote clipboard snapshot into the local OS clipboard never echoes back to peers.
- **Conflict Resolution**: Updates carry `(lamport_seq, origin_device_id)` for deterministic ordering.
- **Privacy & Ephemeral Storage**: Text, rich text, and images stay strictly in memory/OS clipboard. Copied files are streamed over TLS, path-validated against traversal (`validate_rel_path`), verified per-file with SHA-256, staged inside a `0700` temporary folder, and deleted automatically when superseded or on application shutdown.
