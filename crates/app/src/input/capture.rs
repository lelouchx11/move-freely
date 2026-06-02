//! `rdev`-backed global input capture with grab (used by the server).
//!
//! `rdev::grab` runs a blocking OS event loop, so it lives on a dedicated
//! thread. Its callback decides synchronously whether each event passes through
//! to the local OS (`Some`) or is swallowed (`None`). While *grabbing* (focus is
//! on a remote screen) we swallow everything and forward a normalized
//! [`InputEvent`] over a channel; otherwise events flow through untouched.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use rdev::{Event, EventType};
use tokio::sync::mpsc;

use super::{protocol_button, protocol_key, InputEvent};

/// Handle to the running capture thread.
pub struct Capture {
    /// Normalized events captured while grabbing (plus `ToggleFocus` hotkeys).
    pub events: mpsc::UnboundedReceiver<InputEvent>,
    grabbing: Arc<AtomicBool>,
}

impl Capture {
    /// A cheap, cloneable handle to toggle grabbing (swallowing of local input)
    /// independently of the event receiver, so a consumer can own the receiver
    /// and the toggle separately. Enable when focus moves to a remote screen;
    /// disable when it returns to the local (server) screen.
    pub fn grab_handle(&self) -> GrabHandle {
        GrabHandle(self.grabbing.clone())
    }
}

/// Detached control over the capture thread's grab state.
#[derive(Clone)]
pub struct GrabHandle(Arc<AtomicBool>);

impl GrabHandle {
    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

/// Start global capture. `hotkey` toggles focus and is always swallowed so it
/// never leaks to the local app.
pub fn start(hotkey: rdev::Key) -> Result<Capture> {
    let (tx, rx) = mpsc::unbounded_channel();
    let grabbing = Arc::new(AtomicBool::new(false));
    let grabbing_cb = grabbing.clone();

    std::thread::Builder::new()
        .name("mf-capture".into())
        .spawn(move || {
            // rdev's grab callback is `Fn`, so cross-event state lives in a Cell
            // (the callback only ever runs on this one capture thread).
            let last_pos: std::cell::Cell<Option<(f64, f64)>> = std::cell::Cell::new(None);
            let result = rdev::grab(move |event: Event| {
                callback(&event, &hotkey, &grabbing_cb, &tx, &last_pos)
            });
            if let Err(e) = result {
                tracing::error!(
                    "input capture failed: {e:?} — on macOS grant Accessibility AND \
                     Input Monitoring to this binary in System Settings → Privacy"
                );
            }
        })?;

    Ok(Capture { events: rx, grabbing })
}

/// The grab callback. Returns `Some(event)` to let it reach the local OS, or
/// `None` to swallow it.
fn callback(
    event: &Event,
    hotkey: &rdev::Key,
    grabbing: &AtomicBool,
    tx: &mpsc::UnboundedSender<InputEvent>,
    last_pos: &std::cell::Cell<Option<(f64, f64)>>,
) -> Option<Event> {
    // Hotkey is always consumed and toggles focus.
    if let EventType::KeyPress(k) = event.event_type {
        if k == *hotkey {
            let _ = tx.send(InputEvent::ToggleFocus);
            return None;
        }
    }

    let grab = grabbing.load(Ordering::SeqCst);

    let mapped = match &event.event_type {
        EventType::MouseMove { x, y } => {
            let delta = last_pos.get().map(|(px, py)| ((x - px) as i32, (y - py) as i32));
            last_pos.set(Some((*x, *y)));
            match delta {
                Some((dx, dy)) if (dx, dy) != (0, 0) => {
                    Some(InputEvent::MouseMoveRel { dx, dy })
                }
                _ => None,
            }
        }
        EventType::ButtonPress(b) => Some(InputEvent::MouseButton {
            button: protocol_button(*b),
            pressed: true,
        }),
        EventType::ButtonRelease(b) => Some(InputEvent::MouseButton {
            button: protocol_button(*b),
            pressed: false,
        }),
        EventType::Wheel { delta_x, delta_y } => Some(InputEvent::Scroll {
            dx: *delta_x as i32,
            dy: *delta_y as i32,
        }),
        EventType::KeyPress(k) => Some(InputEvent::Key {
            key: protocol_key(*k, event.name.as_deref()),
            pressed: true,
        }),
        EventType::KeyRelease(k) => Some(InputEvent::Key {
            key: protocol_key(*k, event.name.as_deref()),
            pressed: false,
        }),
    };

    if grab {
        if let Some(ev) = mapped {
            let _ = tx.send(ev);
        }
        None // swallow: focus is on a remote screen
    } else {
        Some(event.clone()) // pass through: focus is local
    }
}
