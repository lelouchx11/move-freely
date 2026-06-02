//! OS input abstraction: capturing local input (server) and injecting remote
//! input (client). The cross-platform baseline uses `rdev` for capture/grab and
//! `enigo` for injection; both are wrapped behind the traits here so native
//! backends can replace them per-OS later without touching the rest of the app.

pub mod capture;
pub mod inject;

use mf_protocol::{Button, Key};

/// A normalized input event flowing through the app, independent of any OS API.
#[derive(Debug, Clone, PartialEq)]
pub enum InputEvent {
    /// Relative pointer motion in pixels.
    MouseMoveRel { dx: i32, dy: i32 },
    MouseButton { button: Button, pressed: bool },
    Scroll { dx: i32, dy: i32 },
    Key { key: Key, pressed: bool },
    /// The focus-toggle hotkey was pressed on the server (capture-only).
    ToggleFocus,
}

impl InputEvent {
    /// Convert to a wire [`Message`](mf_protocol::Message), if this event is one
    /// that gets forwarded to a client. `ToggleFocus` is local-only and returns
    /// `None`.
    pub fn to_message(&self) -> Option<mf_protocol::Message> {
        use mf_protocol::Message;
        Some(match *self {
            InputEvent::MouseMoveRel { dx, dy } => Message::MouseMoveRel { dx, dy },
            InputEvent::MouseButton { button, pressed } => {
                Message::MouseButton { button, pressed }
            }
            InputEvent::Scroll { dx, dy } => Message::Scroll { dx, dy },
            InputEvent::Key { key, pressed } => Message::KeyEvent { key, pressed },
            InputEvent::ToggleFocus => return None,
        })
    }
}

/// Injects normalized input into the local OS.
///
/// Not `Send`: some backends (enigo on macOS) hold non-`Send` OS handles, and
/// the injector lives entirely inside the client's single (non-spawned) task.
pub trait Injector {
    fn inject(&mut self, ev: &InputEvent) -> anyhow::Result<()>;
}

/// Construct the platform default injector (enigo-backed).
pub fn new_injector() -> anyhow::Result<Box<dyn Injector>> {
    Ok(Box::new(inject::EnigoInjector::new()?))
}

/// Move the pointer in a small square and type a couple of keys, to confirm OS
/// injection works (and that permissions are granted) on this machine.
pub fn selftest() -> anyhow::Result<()> {
    tracing::info!("self-test: moving the pointer in a square and typing \"hi\"");
    let mut inj = new_injector()?;
    let step = 80;
    let square = [(step, 0), (0, step), (-step, 0), (0, -step)];
    for (dx, dy) in square {
        inj.inject(&InputEvent::MouseMoveRel { dx, dy })?;
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    for c in ['h', 'i'] {
        inj.inject(&InputEvent::Key { key: Key::Char(c), pressed: true })?;
        inj.inject(&InputEvent::Key { key: Key::Char(c), pressed: false })?;
    }
    tracing::info!("self-test complete — if the pointer moved, injection works");
    Ok(())
}

// --- enigo / rdev <-> protocol mappings, kept next to `InputEvent`. ---

pub(crate) fn enigo_button(button: Button) -> Option<enigo::Button> {
    Some(match button {
        Button::Left => enigo::Button::Left,
        Button::Right => enigo::Button::Right,
        Button::Middle => enigo::Button::Middle,
        Button::Other(_) => return None, // extra buttons unsupported by enigo on all OSes
    })
}

pub(crate) fn enigo_key(key: Key) -> enigo::Key {
    use enigo::Key as E;
    match key {
        Key::Char(c) => E::Unicode(c),
        Key::Enter => E::Return,
        Key::Escape => E::Escape,
        Key::Backspace => E::Backspace,
        Key::Tab => E::Tab,
        Key::Space => E::Space,
        Key::ArrowUp => E::UpArrow,
        Key::ArrowDown => E::DownArrow,
        Key::ArrowLeft => E::LeftArrow,
        Key::ArrowRight => E::RightArrow,
        Key::ShiftLeft => E::LShift,
        Key::ShiftRight => E::RShift,
        Key::ControlLeft => E::LControl,
        Key::ControlRight => E::RControl,
        Key::AltLeft | Key::AltRight => E::Alt,
        Key::MetaLeft | Key::MetaRight => E::Meta,
        Key::Raw(code) => E::Other(code),
    }
}

/// Map an rdev key (plus the OS-provided produced text, if any) to a protocol key.
pub(crate) fn protocol_key(key: rdev::Key, name: Option<&str>) -> Key {
    use rdev::Key as R;
    match key {
        R::Return => Key::Enter,
        R::Escape => Key::Escape,
        R::Backspace => Key::Backspace,
        R::Tab => Key::Tab,
        R::Space => Key::Space,
        R::UpArrow => Key::ArrowUp,
        R::DownArrow => Key::ArrowDown,
        R::LeftArrow => Key::ArrowLeft,
        R::RightArrow => Key::ArrowRight,
        R::ShiftLeft => Key::ShiftLeft,
        R::ShiftRight => Key::ShiftRight,
        R::ControlLeft => Key::ControlLeft,
        R::ControlRight => Key::ControlRight,
        R::Alt | R::AltGr => Key::AltLeft,
        R::MetaLeft => Key::MetaLeft,
        R::MetaRight => Key::MetaRight,
        other => {
            // Prefer the character the OS says this key produced (layout-correct);
            // otherwise fall back to a raw, platform-neutral code so nothing drops.
            match name.and_then(|s| s.chars().next()) {
                Some(c) if !c.is_control() && name.map(|s| s.chars().count()) == Some(1) => {
                    Key::Char(c)
                }
                _ => Key::Raw(rdev_key_code(other)),
            }
        }
    }
}

pub(crate) fn protocol_button(button: rdev::Button) -> Button {
    match button {
        rdev::Button::Left => Button::Left,
        rdev::Button::Right => Button::Right,
        rdev::Button::Middle => Button::Middle,
        rdev::Button::Unknown(n) => Button::Other(n),
    }
}

/// A stable-ish numeric code for rdev keys we don't name, via their debug form's
/// hash. Good enough as an opaque fallback that round-trips on the same OS.
fn rdev_key_code(key: rdev::Key) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{key:?}").hash(&mut h);
    h.finish() as u32
}
