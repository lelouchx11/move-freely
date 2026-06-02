//! Server configuration loaded from a TOML file.

use std::path::Path;

use anyhow::{bail, Context, Result};
use mf_protocol::layout::{Layout, Position, Screen};
use serde::Deserialize;

/// Top-level server config, e.g. `config/layout.example.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    /// One `[[screen]]` table per display, including the server's own (`center`).
    #[serde(default)]
    pub screen: Vec<ScreenConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Socket address to bind, e.g. `0.0.0.0:24800`.
    pub bind: String,
    /// Shared secret; clients must present a matching token.
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScreenConfig {
    pub name: String,
    pub position: Position,
    pub width: u32,
    pub height: u32,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.screen.is_empty() {
            bail!("config must define at least one [[screen]]");
        }
        if self.screen.iter().filter(|s| s.position == Position::Center).count() != 1 {
            bail!("config must define exactly one screen with position = \"center\"");
        }
        // Reject duplicate positions or names so routing stays unambiguous.
        for (i, a) in self.screen.iter().enumerate() {
            for b in &self.screen[i + 1..] {
                if a.position == b.position {
                    bail!("two screens share position {:?}", a.position);
                }
                if a.name == b.name {
                    bail!("two screens share name {:?}", a.name);
                }
            }
        }
        Ok(())
    }

    /// Build the OS-agnostic [`Layout`] from the configured screens.
    pub fn layout(&self) -> Layout {
        Layout::new(
            self.screen
                .iter()
                .map(|s| Screen {
                    name: s.name.clone(),
                    position: s.position,
                    width: s.width,
                    height: s.height,
                })
                .collect(),
        )
    }
}
