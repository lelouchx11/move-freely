//! Core configuration, shortcuts, and legacy TOML layout support.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use mf_protocol::{
    BoundaryMode, DeviceId, DevicePlacement, EdgeSensitivity, KeyCode, Layout, ModifierFlags,
    Position, Screen,
};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 24800;

/// Controls how modifier keys map between macOS (`Cmd`/`Ctrl`) and Windows/Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModifierMappingMode {
    /// Pass modifiers 1:1 (`Meta` ↔ `Command`/`Super`, `Control` ↔ `Control`).
    #[default]
    Standard,
    /// Swap `Meta/Command` and `Control` whenever routing between macOS and non-macOS.
    SwapCmdCtrlCrossOs,
}

/// Global keyboard shortcut binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub key: KeyCode,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub meta: bool,
}

impl Shortcut {
    pub fn new(key: KeyCode, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Self {
        Self {
            key,
            ctrl,
            alt,
            shift,
            meta,
        }
    }

    pub fn matches(&self, key: KeyCode, mods: ModifierFlags) -> bool {
        self.key == key
            && self.ctrl == mods.ctrl
            && self.alt == (mods.alt || mods.alt_gr)
            && self.shift == mods.shift
            && self.meta == mods.meta
    }

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.alt {
            parts.push("Alt".to_string());
        }
        if self.shift {
            parts.push("Shift".to_string());
        }
        if self.meta {
            parts.push("Meta".to_string());
        }
        parts.push(format!("{:?}", self.key));
        parts.join("+")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShortcutSettings {
    pub cycle_screen: Shortcut,
    pub claim_controller: Shortcut,
    pub emergency_release: Shortcut,
    pub toggle_sharing: Shortcut,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            cycle_screen: Shortcut::new(KeyCode::F12, false, false, false, false),
            claim_controller: Shortcut::new(KeyCode::KeyC, true, true, true, false),
            emergency_release: Shortcut::new(KeyCode::Escape, true, true, true, false),
            toggle_sharing: Shortcut::new(KeyCode::KeyP, true, true, true, false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipboardSettings {
    pub enabled: bool,
    pub sync_text: bool,
    pub sync_rich_text: bool,
    pub sync_images: bool,
    pub sync_files: bool,
    /// Max size (MiB) for text/HTML/RTF/image clipboard payloads.
    pub max_payload_mb: u32,
    /// Max total size (MiB) for copied file bundles.
    pub max_file_mb: u32,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_text: true,
            sync_rich_text: true,
            sync_images: true,
            sync_files: true,
            max_payload_mb: 32,
            max_file_mb: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub device_name: String,
    pub bind_ip: String,
    pub listen_port: u16,
    pub lan_only_ips: bool,
    pub allow_routable_ips: bool,
    pub sharing_enabled: bool,
    pub keyboard_sharing: bool,
    pub mouse_sharing: bool,
    pub auto_reconnect: bool,
    pub launch_on_startup: bool,
    pub allow_remote_claim_controller: bool,
    pub preferred_controller: Option<DeviceId>,
    pub scroll_speed: f64,
    pub invert_remote_scroll: bool,
    pub modifier_mapping: ModifierMappingMode,
    pub boundary_mode: BoundaryMode,
    pub sensitivity: EdgeSensitivity,
    pub clipboard: ClipboardSettings,
    pub shortcuts: ShortcutSettings,
    pub placements: Vec<DevicePlacement>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device_name: default_device_name(),
            bind_ip: "0.0.0.0".into(),
            listen_port: DEFAULT_PORT,
            lan_only_ips: true,
            allow_routable_ips: false,
            sharing_enabled: true,
            keyboard_sharing: true,
            mouse_sharing: true,
            auto_reconnect: true,
            launch_on_startup: false,
            allow_remote_claim_controller: true,
            preferred_controller: None,
            scroll_speed: 1.0,
            invert_remote_scroll: false,
            modifier_mapping: ModifierMappingMode::Standard,
            boundary_mode: BoundaryMode::Overlapping,
            sensitivity: EdgeSensitivity::default(),
            clipboard: ClipboardSettings::default(),
            shortcuts: ShortcutSettings::default(),
            placements: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let cfg: AppConfig = toml::from_str(&raw)
                .with_context(|| format!("parsing config {}", path.display()))?;
            Ok(cfg)
        } else {
            let cfg = Self::default();
            cfg.save(path)?;
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

pub fn default_data_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var("MF_DATA_DIR") {
        if !env_dir.trim().is_empty() {
            return PathBuf::from(env_dir);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("move-freely")
}

pub fn default_device_name() -> String {
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")) {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return trimmed.split('.').next().unwrap_or(trimmed).to_string();
        }
    }
    #[cfg(unix)]
    {
        if let Ok(name) = std::process::Command::new("hostname").output() {
            if name.status.success() {
                let s = String::from_utf8_lossy(&name.stdout);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    return trimmed.split('.').next().unwrap_or(trimmed).to_string();
                }
            }
        }
    }
    "move-freely-device".into()
}

// ---------------------------------------------------------------------------
// Legacy v1 TOML config (`config/layout.toml`) support
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyConfig {
    pub server: LegacyServerConfig,
    #[serde(default)]
    pub screen: Vec<LegacyScreenConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyServerConfig {
    pub bind: String,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyScreenConfig {
    pub name: String,
    pub position: Position,
    pub width: u32,
    pub height: u32,
}

impl LegacyConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: LegacyConfig = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.screen.is_empty() {
            bail!("config must define at least one [[screen]]");
        }
        if self
            .screen
            .iter()
            .filter(|s| s.position == Position::Center)
            .count()
            != 1
        {
            bail!("config must define exactly one screen with position = \"center\"");
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_config_round_trip() {
        let dir = std::env::temp_dir().join(format!("mf-cfg-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.toml");
        let mut cfg = AppConfig::default();
        cfg.device_name = "studio-mac".into();
        cfg.listen_port = 24999;
        cfg.clipboard.max_payload_mb = 64;
        cfg.save(&path).unwrap();

        let loaded = AppConfig::load_or_create(&path).unwrap();
        assert_eq!(loaded, cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shortcut_matches_exact_modifiers() {
        let s = Shortcut::new(KeyCode::Escape, true, true, true, false);
        assert!(s.matches(
            KeyCode::Escape,
            ModifierFlags {
                ctrl: true,
                alt: true,
                shift: true,
                ..Default::default()
            }
        ));
        assert!(!s.matches(
            KeyCode::Escape,
            ModifierFlags {
                ctrl: true,
                alt: false,
                shift: true,
                ..Default::default()
            }
        ));
    }
}
