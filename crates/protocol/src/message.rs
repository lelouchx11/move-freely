//! Messages exchanged between paired peer devices.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::layout::{BoundaryMode, DevicePlacement, Monitor};

/// Wire protocol version. Peers reject mismatched major versions.
pub const PROTOCOL_VERSION: u32 = 2;

/// Cryptographic or persistent unique identifier for a device (derived from its
/// public key / TLS certificate SHA-256 fingerprint).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DeviceId(pub String);

impl DeviceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn short(&self) -> &str {
        let s = self.0.as_str();
        if s.len() > 8 {
            &s[..8]
        } else {
            s
        }
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for DeviceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DeviceId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl AsRef<str> for DeviceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    MacOs,
    Windows,
    Linux,
    Other,
}

impl OsKind {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "linux")]
        {
            Self::Linux
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchKind {
    Aarch64,
    X86_64,
    Other,
}

impl ArchKind {
    pub fn current() -> Self {
        #[cfg(target_arch = "aarch64")]
        {
            Self::Aarch64
        }
        #[cfg(target_arch = "x86_64")]
        {
            Self::X86_64
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            Self::Other
        }
    }
}

/// Identifies whether a TLS connection carries real-time control/input frames
/// or bulk clipboard image/file transfer frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelRole {
    Control,
    Data,
}

/// Capabilities advertised by a peer based on OS permissions and session type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerCapabilities {
    pub can_capture: bool,
    pub can_inject: bool,
    pub clipboard_text: bool,
    pub clipboard_image: bool,
    pub clipboard_files: bool,
    pub wayland_session: bool,
    pub status_note: Option<String>,
}

impl Default for PeerCapabilities {
    fn default() -> Self {
        Self {
            can_capture: true,
            can_inject: true,
            clipboard_text: true,
            clipboard_image: true,
            clipboard_files: true,
            wayland_session: false,
            status_note: None,
        }
    }
}

/// Metadata for a single file/directory inside a clipboard file-transfer offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFileEntry {
    /// Forward-slash-delimited relative path inside the staging folder.
    /// Must never be empty, absolute, or contain `..`.
    pub rel_path: String,
    pub size: u64,
    pub is_dir: bool,
    pub sha256: [u8; 32],
}

/// Validate a relative path from an untrusted peer so file staging cannot
/// escape the destination staging directory.
pub fn validate_rel_path(rel_path: &str) -> Result<(), &'static str> {
    if rel_path.is_empty() {
        return Err("empty relative path");
    }
    if rel_path.contains('\0') || rel_path.contains('\\') {
        return Err("invalid characters in relative path");
    }
    if rel_path.starts_with('/') {
        return Err("absolute path is not allowed");
    }
    // Reject Windows drive prefixes like `C:`
    if rel_path.as_bytes().get(1) == Some(&b':') {
        return Err("drive-qualified path is not allowed");
    }
    for segment in rel_path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err("invalid path segment");
        }
    }
    Ok(())
}

/// A single protocol message. Serialized as tagged JSON inside a Control frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    // --- Pairing (used before both peers have added each other to trust store) ---
    PairInit {
        device_id: DeviceId,
        name: String,
        os: OsKind,
        arch: ArchKind,
        version: u32,
        client_nonce: [u8; 16],
    },
    PairChallenge {
        device_id: DeviceId,
        name: String,
        os: OsKind,
        arch: ArchKind,
        server_nonce: [u8; 16],
        requires_pin: bool,
    },
    PairProof {
        proof_hmac: [u8; 32],
    },
    PairAccept {
        proof_hmac: [u8; 32],
    },
    PairReject {
        reason: String,
    },

    // --- Authenticated Session Handshake ---
    Hello {
        device_id: DeviceId,
        name: String,
        os: OsKind,
        arch: ArchKind,
        version: u32,
        channel: ChannelRole,
        session_nonce: u64,
        monitors: Vec<Monitor>,
        capabilities: PeerCapabilities,
    },
    Welcome {
        device_id: DeviceId,
        name: String,
        os: OsKind,
        arch: ArchKind,
        session_nonce: u64,
        monitors: Vec<Monitor>,
        capabilities: PeerCapabilities,
        active_controller: Option<(u64, DeviceId)>,
    },
    Reject {
        reason: String,
    },
    Goodbye {
        reason: String,
    },

    // --- Topology & Ownership Coordination ---
    MonitorsUpdated {
        monitors: Vec<Monitor>,
    },
    CapabilitiesUpdated {
        capabilities: PeerCapabilities,
    },
    LayoutSync {
        epoch: u64,
        placements: Vec<DevicePlacement>,
        boundary_mode: BoundaryMode,
    },
    ClaimController {
        epoch: u64,
        controller_id: DeviceId,
    },
    ControllerAck {
        epoch: u64,
        controller_id: DeviceId,
    },
    FocusChange {
        epoch: u64,
        controller_id: DeviceId,
        target_device: DeviceId,
    },

    // --- Real-Time Input Routing ---
    /// Focus has entered the target device's screen at OS coordinates `(x, y)`.
    Enter { x: i32, y: i32 },
    /// Focus has left the target device's screen; release any held inputs.
    Leave,
    /// Emergency / safety release of all held keys and buttons.
    ReleaseAllInput,
    /// Absolute pointer target in the target device's OS input coordinates, plus
    /// original relative motion deltas for games/raw consumers.
    PointerMoveAbs { x: i32, y: i32, dx: i32, dy: i32 },
    /// Relative pointer motion fallback.
    PointerMoveRel { dx: i32, dy: i32 },
    /// Mouse button transition.
    MouseButton { button: Button, pressed: bool },
    /// Scroll wheel delta (positive dy = scroll up, positive dx = scroll right).
    Scroll { dx: f64, dy: f64, continuous: bool },
    /// Keyboard key transition using a normalized physical key code plus optional
    /// Unicode text resolved by the source layout.
    KeyEvent {
        key: KeyCode,
        pressed: bool,
        text: Option<String>,
        modifiers: ModifierFlags,
    },
    /// Controlled device notifies active controller when its local physical mouse
    /// moves, keeping virtual pointer position synchronized without jumping.
    LocalCursorSync { x: i32, y: i32 },

    // --- Liveness & Latency Measurement ---
    Heartbeat { seq: u64, sent_ms: u64 },
    HeartbeatAck { seq: u64, echoed_ms: u64 },

    // --- Universal Clipboard & Secure Local File Transfer ---
    ClipboardText {
        seq: u64,
        origin: DeviceId,
        sha256: [u8; 32],
        plain: String,
        html: Option<String>,
        rtf: Option<String>,
    },
    ClipboardImageOffer {
        transfer_id: u64,
        seq: u64,
        origin: DeviceId,
        sha256: [u8; 32],
        total_bytes: u64,
        width: u32,
        height: u32,
    },
    ClipboardFilesOffer {
        transfer_id: u64,
        seq: u64,
        origin: DeviceId,
        sha256: [u8; 32],
        total_bytes: u64,
        entries: Vec<SharedFileEntry>,
    },
    TransferAck {
        transfer_id: u64,
        accepted: bool,
        reason: Option<String>,
    },
    TransferComplete {
        transfer_id: u64,
        ok: bool,
        error: Option<String>,
    },
}

/// Mouse buttons forwarded across devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
    Other(u8),
}

/// Snapshot of held modifiers at the moment a key event occurred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModifierFlags {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
    pub caps_lock: bool,
    pub alt_gr: bool,
}

impl ModifierFlags {
    pub fn has_command_modifier(self) -> bool {
        self.ctrl || self.alt || self.meta || self.alt_gr
    }
}

/// Normalized physical key identifier modeled after USB HID / W3C `KeyboardEvent.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    // Letters
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,

    // Top row digits
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,

    // Punctuation & symbols
    Minus,
    Equal,
    BracketLeft,
    BracketRight,
    Backslash,
    Semicolon,
    Quote,
    Backquote,
    Comma,
    Period,
    Slash,
    IntlBackslash,

    // Control & whitespace
    Enter,
    Escape,
    Backspace,
    Tab,
    Space,
    CapsLock,

    // Modifiers
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    MetaLeft,
    MetaRight,
    ContextMenu,

    // Function keys
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    F13,
    F14,
    F15,
    F16,
    F17,
    F18,
    F19,
    F20,
    F21,
    F22,
    F23,
    F24,

    // Navigation & editing
    PrintScreen,
    ScrollLock,
    Pause,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,

    // Numpad
    NumLock,
    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,
    NumpadAdd,
    NumpadSubtract,
    NumpadMultiply,
    NumpadDivide,
    NumpadDecimal,
    NumpadEnter,
    NumpadEqual,

    // Media
    AudioVolumeMute,
    AudioVolumeDown,
    AudioVolumeUp,
    MediaPlayPause,
    MediaStop,
    MediaTrackNext,
    MediaTrackPrevious,

    /// Unmapped Unicode char fallback (for synthetic/demo streams).
    Unicode(char),
    /// Platform fallback code when no standard key exists.
    Unknown(u32),
}

impl KeyCode {
    pub fn is_modifier(self) -> bool {
        matches!(
            self,
            Self::ShiftLeft
                | Self::ShiftRight
                | Self::ControlLeft
                | Self::ControlRight
                | Self::AltLeft
                | Self::AltRight
                | Self::MetaLeft
                | Self::MetaRight
                | Self::CapsLock
        )
    }

    pub fn from_ascii_char(c: char) -> Option<Self> {
        Some(match c.to_ascii_lowercase() {
            'a' => Self::KeyA,
            'b' => Self::KeyB,
            'c' => Self::KeyC,
            'd' => Self::KeyD,
            'e' => Self::KeyE,
            'f' => Self::KeyF,
            'g' => Self::KeyG,
            'h' => Self::KeyH,
            'i' => Self::KeyI,
            'j' => Self::KeyJ,
            'k' => Self::KeyK,
            'l' => Self::KeyL,
            'm' => Self::KeyM,
            'n' => Self::KeyN,
            'o' => Self::KeyO,
            'p' => Self::KeyP,
            'q' => Self::KeyQ,
            'r' => Self::KeyR,
            's' => Self::KeyS,
            't' => Self::KeyT,
            'u' => Self::KeyU,
            'v' => Self::KeyV,
            'w' => Self::KeyW,
            'x' => Self::KeyX,
            'y' => Self::KeyY,
            'z' => Self::KeyZ,
            '0' => Self::Digit0,
            '1' => Self::Digit1,
            '2' => Self::Digit2,
            '3' => Self::Digit3,
            '4' => Self::Digit4,
            '5' => Self::Digit5,
            '6' => Self::Digit6,
            '7' => Self::Digit7,
            '8' => Self::Digit8,
            '9' => Self::Digit9,
            ' ' => Self::Space,
            '\n' | '\r' => Self::Enter,
            '\t' => Self::Tab,
            '-' => Self::Minus,
            '=' => Self::Equal,
            '[' => Self::BracketLeft,
            ']' => Self::BracketRight,
            '\\' => Self::Backslash,
            ';' => Self::Semicolon,
            '\'' => Self::Quote,
            '`' => Self::Backquote,
            ',' => Self::Comma,
            '.' => Self::Period,
            '/' => Self::Slash,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_json_round_trips() {
        let cases = vec![
            Message::Hello {
                device_id: DeviceId::from("abc12345"),
                name: "macbook".into(),
                os: OsKind::MacOs,
                arch: ArchKind::Aarch64,
                version: PROTOCOL_VERSION,
                channel: ChannelRole::Control,
                session_nonce: 77,
                monitors: vec![Monitor {
                    id: "main".into(),
                    name: "Built-in".into(),
                    x: 0,
                    y: 0,
                    width: 1440,
                    height: 900,
                    physical_width: 2880,
                    physical_height: 1800,
                    scale_factor: 2.0,
                    is_primary: true,
                }],
                capabilities: PeerCapabilities::default(),
            },
            Message::Enter { x: 100, y: 720 },
            Message::Leave,
            Message::ReleaseAllInput,
            Message::PointerMoveAbs {
                x: 500,
                y: 300,
                dx: -3,
                dy: 7,
            },
            Message::MouseButton {
                button: Button::Back,
                pressed: true,
            },
            Message::Scroll {
                dx: 0.0,
                dy: -1.5,
                continuous: true,
            },
            Message::KeyEvent {
                key: KeyCode::KeyA,
                pressed: true,
                text: Some("a".into()),
                modifiers: ModifierFlags::default(),
            },
            Message::ClipboardText {
                seq: 3,
                origin: DeviceId::from("dev-1"),
                sha256: [7u8; 32],
                plain: "hello 世界".into(),
                html: Some("<b>hello</b>".into()),
                rtf: None,
            },
            Message::Heartbeat {
                seq: 5,
                sent_ms: 123456,
            },
        ];
        for msg in cases {
            let bytes = serde_json::to_vec(&msg).expect("serialize");
            let back: Message = serde_json::from_slice(&bytes).expect("deserialize");
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn validates_relative_paths_against_traversal() {
        assert!(validate_rel_path("report.pdf").is_ok());
        assert!(validate_rel_path("folder/sub/file.txt").is_ok());
        assert!(validate_rel_path("").is_err());
        assert!(validate_rel_path("/etc/passwd").is_err());
        assert!(validate_rel_path("../secret").is_err());
        assert!(validate_rel_path("a/../b").is_err());
        assert!(validate_rel_path("a\\b").is_err());
        assert!(validate_rel_path("C:foo").is_err());
        assert!(validate_rel_path("a//b").is_err());
    }
}
