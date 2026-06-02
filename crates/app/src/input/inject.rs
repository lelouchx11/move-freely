//! `enigo`-backed input injection (used by the client).

use anyhow::{anyhow, Result};
use enigo::{Axis, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};

use super::{enigo_button, enigo_key, InputEvent, Injector};

pub struct EnigoInjector {
    enigo: Enigo,
}

impl EnigoInjector {
    pub fn new() -> Result<Self> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| anyhow!("initializing input injection: {e} (check OS permissions)"))?;
        Ok(EnigoInjector { enigo })
    }
}

impl Injector for EnigoInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::MouseMoveRel { dx, dy } => {
                self.enigo.move_mouse(dx, dy, Coordinate::Rel)?;
            }
            InputEvent::MouseButton { button, pressed } => {
                if let Some(b) = enigo_button(button) {
                    let dir = if pressed { Direction::Press } else { Direction::Release };
                    self.enigo.button(b, dir)?;
                }
            }
            InputEvent::Scroll { dx, dy } => {
                // enigo: positive length scrolls down / right; our dy>0 means up.
                if dy != 0 {
                    self.enigo.scroll(-dy, Axis::Vertical)?;
                }
                if dx != 0 {
                    self.enigo.scroll(dx, Axis::Horizontal)?;
                }
            }
            InputEvent::Key { key, pressed } => {
                let dir = if pressed { Direction::Press } else { Direction::Release };
                self.enigo.key(enigo_key(key), dir)?;
            }
            InputEvent::ToggleFocus => {} // never injected
        }
        Ok(())
    }
}
