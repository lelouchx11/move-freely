//! Messages exchanged between server and clients.

use serde::{Deserialize, Serialize};

/// Bumped whenever the wire format changes incompatibly. The server rejects a
/// client whose `PROTOCOL_VERSION` does not match its own.
pub const PROTOCOL_VERSION: u32 = 1;

/// A single protocol message. Serialized as JSON for v1 (easy to debug); the
/// framing layer is format-agnostic so this can switch to bincode later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    /// First message a client sends after connecting.
    Hello {
        /// Human-readable screen name, must match an entry in the server layout.
        name: String,
        /// Client screen size in physical pixels, used for virtual-cursor math.
        width: u32,
        height: u32,
        /// Shared pre-shared token; the server rejects mismatches.
        token: String,
        version: u32,
    },
    /// Server's reply accepting a client, echoing its resolved layout position.
    Welcome { position: crate::layout::Position },
    /// Server's reply rejecting a client, with a human-readable reason.
    Reject { reason: String },

    /// Focus has entered this client's screen at the given entry coordinates
    /// (in the client's pixel space).
    Enter { x: i32, y: i32 },
    /// Focus has left this client's screen; it should stop acting on input.
    Leave,

    /// Relative mouse motion (forwarded while a remote screen is active).
    MouseMoveRel { dx: i32, dy: i32 },
    /// Mouse button transition.
    MouseButton { button: Button, pressed: bool },
    /// Scroll wheel delta (positive dy = scroll up).
    Scroll { dx: i32, dy: i32 },
    /// Keyboard key transition, using a normalized key identifier.
    KeyEvent { key: Key, pressed: bool },

    /// Liveness ping; either side may send it and expects nothing in return.
    Heartbeat,
}

/// Mouse buttons we forward. Extra buttons collapse to `Button::Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Other(u8),
}

/// A normalized, OS-independent key identifier.
///
/// v1 keeps this deliberately small: named keys we can map reliably across
/// platforms, plus a raw fallback carrying a platform-neutral code so nothing
/// is silently dropped. Backends translate to/from their native scancodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum Key {
    /// A printable Unicode character produced by the key (already layout-resolved).
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    MetaLeft,
    MetaRight,
    /// Anything not otherwise named, carried by a platform-neutral raw code.
    Raw(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_json_round_trips() {
        let cases = vec![
            Message::Hello {
                name: "macbook".into(),
                width: 2560,
                height: 1440,
                token: "secret".into(),
                version: PROTOCOL_VERSION,
            },
            Message::Welcome {
                position: crate::layout::Position::Right,
            },
            Message::Enter { x: 0, y: 720 },
            Message::Leave,
            Message::MouseMoveRel { dx: -3, dy: 7 },
            Message::MouseButton {
                button: Button::Left,
                pressed: true,
            },
            Message::Scroll { dx: 0, dy: -1 },
            Message::KeyEvent {
                key: Key::Char('a'),
                pressed: true,
            },
            Message::KeyEvent {
                key: Key::ArrowUp,
                pressed: false,
            },
            Message::Heartbeat,
        ];
        for msg in cases {
            let bytes = serde_json::to_vec(&msg).expect("serialize");
            let back: Message = serde_json::from_slice(&bytes).expect("deserialize");
            assert_eq!(msg, back);
        }
    }
}
