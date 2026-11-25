//! Core engine, native OS backends, TLS peer mesh, mDNS discovery, and
//! universal clipboard synchronization for `move freely`.

pub mod autostart;
pub mod clipboard;
pub mod config;
pub mod discovery;
pub mod engine;
pub mod input;
pub mod network;
pub mod security;

pub use config::{AppConfig, ClipboardSettings, LegacyConfig, ModifierMappingMode, Shortcut, ShortcutSettings};
pub use discovery::{local_ip_addresses, DiscoveredPeer, DiscoveryService};
pub use engine::{
    ClusterSnapshot, EngineCommand, EngineEvent, EngineHandle, EngineMode, HeadlessTestHandles,
    MoveFreelyEngine, PeerView,
};
pub use input::{check_permissions, enumerate_monitors, init_platform_process, PermissionStatus};
pub use security::{DeviceIdentity, PairingStatus, TrustedPeer, TrustedPeerStore};
