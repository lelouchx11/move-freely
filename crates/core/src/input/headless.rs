//! Headless / virtual capture & injection backends used by automated tests,
//! `--echo` mode, and headless CI environments.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use mf_protocol::{Button, KeyCode, ModifierFlags};
use tokio::sync::mpsc;

use super::{CaptureController, CaptureSession, FlagGrabState, HeldInputTracker, InputEvent, PlatformInjector};

#[derive(Debug, Clone, PartialEq)]
pub enum RecordedInjection {
    PointerAbs { x: i32, y: i32, dx: i32, dy: i32 },
    PointerRel { dx: i32, dy: i32 },
    Button { button: Button, pressed: bool },
    Scroll { dx: f64, dy: f64, continuous: bool },
    Key { key: KeyCode, pressed: bool, text: Option<String>, modifiers: ModifierFlags },
    Warp { x: i32, y: i32 },
    ReleaseAll,
}

#[derive(Clone, Default)]
pub struct HeadlessInjector {
    pub log: Arc<Mutex<Vec<RecordedInjection>>>,
    held: Arc<Mutex<HeldInputTracker>>,
    pub echo_to_tracing: bool,
}

impl HeadlessInjector {
    pub fn new(echo_to_tracing: bool) -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            held: Arc::new(Mutex::new(HeldInputTracker::default())),
            echo_to_tracing,
        }
    }

    pub fn snapshot(&self) -> Vec<RecordedInjection> {
        self.log.lock().unwrap().clone()
    }
}

impl PlatformInjector for HeadlessInjector {
    fn inject_pointer_abs(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> Result<()> {
        if self.echo_to_tracing {
            tracing::info!(x, y, dx, dy, "[echo] PointerAbs");
        }
        self.log
            .lock()
            .unwrap()
            .push(RecordedInjection::PointerAbs { x, y, dx, dy });
        Ok(())
    }

    fn inject_pointer_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        if self.echo_to_tracing {
            tracing::info!(dx, dy, "[echo] PointerRel");
        }
        self.log
            .lock()
            .unwrap()
            .push(RecordedInjection::PointerRel { dx, dy });
        Ok(())
    }

    fn inject_button(&mut self, button: Button, pressed: bool) -> Result<()> {
        if self.echo_to_tracing {
            tracing::info!(?button, pressed, "[echo] MouseButton");
        }
        self.held.lock().unwrap().record_button(button, pressed);
        self.log
            .lock()
            .unwrap()
            .push(RecordedInjection::Button { button, pressed });
        Ok(())
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64, continuous: bool) -> Result<()> {
        if self.echo_to_tracing {
            tracing::info!(dx, dy, continuous, "[echo] Scroll");
        }
        self.log.lock().unwrap().push(RecordedInjection::Scroll {
            dx,
            dy,
            continuous,
        });
        Ok(())
    }

    fn inject_key(
        &mut self,
        key: KeyCode,
        pressed: bool,
        text: Option<&str>,
        modifiers: ModifierFlags,
    ) -> Result<()> {
        if self.echo_to_tracing {
            tracing::info!(?key, pressed, ?modifiers, "[echo] KeyEvent");
        }
        self.held.lock().unwrap().record_key(key, pressed);
        self.log.lock().unwrap().push(RecordedInjection::Key {
            key,
            pressed,
            text: text.map(|s| s.to_owned()),
            modifiers,
        });
        Ok(())
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        if self.echo_to_tracing {
            tracing::info!(x, y, "[echo] WarpCursor");
        }
        self.log
            .lock()
            .unwrap()
            .push(RecordedInjection::Warp { x, y });
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let (buttons, keys) = self.held.lock().unwrap().drain_release_order();
        for b in buttons {
            self.log.lock().unwrap().push(RecordedInjection::Button {
                button: b,
                pressed: false,
            });
        }
        for k in keys {
            self.log.lock().unwrap().push(RecordedInjection::Key {
                key: k,
                pressed: false,
                text: None,
                modifiers: ModifierFlags::default(),
            });
        }
        self.log.lock().unwrap().push(RecordedInjection::ReleaseAll);
        Ok(())
    }
}

pub struct HeadlessCaptureController {
    state: Arc<FlagGrabState>,
    pub grab_history: Arc<Mutex<Vec<(bool, Option<(i32, i32)>)>>>,
}

impl CaptureController for HeadlessCaptureController {
    fn set_grabbing(&self, on: bool, warp_to: Option<(i32, i32)>) {
        self.state
            .grabbing
            .store(on, std::sync::atomic::Ordering::SeqCst);
        self.grab_history.lock().unwrap().push((on, warp_to));
    }

    fn is_grabbing(&self) -> bool {
        self.state.is_grabbing()
    }

    fn stop(&self) {
        self.state
            .stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub fn new_headless_capture() -> (CaptureSession, mpsc::UnboundedSender<InputEvent>, Arc<HeadlessCaptureController>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let controller = Arc::new(HeadlessCaptureController {
        state: FlagGrabState::new(),
        grab_history: Arc::new(Mutex::new(Vec::new())),
    });
    let session = CaptureSession {
        events: rx,
        controller: controller.clone(),
    };
    (session, tx, controller)
}
