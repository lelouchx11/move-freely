//! Unified peer-to-peer multi-device KVM engine coordinating transport,
//! discovery, topology, input capture/injection, clipboard sync, and state events.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use mf_protocol::{
    BinaryChunk, BoundaryMode, ClusterTopology, DeviceId, DevicePlacement,
    DeviceTopology, EdgeMask, EdgeResistanceTracker, Message,
    ModifierFlags, Monitor, OsKind, PeerCapabilities, PointerRoutingState, StepOutcome,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::autostart;
use crate::clipboard::platform::{new_platform_clipboard, ClipboardBackend, MemoryClipboard};
use crate::clipboard::{build_chunks_for_bytes, ClipboardPayload, ClipboardSyncCoordinator, CHUNK_SIZE};
use crate::config::{default_data_dir, AppConfig};
use crate::discovery::{local_ip_addresses, DiscoveredPeer, DiscoveryService};
use crate::input::headless::{new_headless_capture, HeadlessCaptureController, HeadlessInjector};
use crate::input::{
    check_permissions, enumerate_monitors, new_native_injector, open_permission_settings,
    remap_key_for_target, start_native_capture, CaptureController, HeldInputTracker, InputEvent,
    PermissionStatus, PlatformInjector,
};
use crate::network::{LocalAdvertiseState, NetworkEvent, NetworkManager, PeerInfo};
use crate::security::{DeviceIdentity, PairingManager, PairingStatus, TrustedPeerStore};

/// Selects real OS hardware backends vs virtual/headless backends for testing and `--echo` mode.
#[derive(Clone)]
pub enum EngineMode {
    Native,
    Headless {
        echo: bool,
        monitors: Vec<Monitor>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerView {
    pub device_id: DeviceId,
    pub name: String,
    pub display_name: String,
    pub os: OsKind,
    pub arch: mf_protocol::ArchKind,
    pub enabled: bool,
    pub connected: bool,
    pub latency_ms: u64,
    pub addr: Option<String>,
    pub monitors: Vec<Monitor>,
    pub capabilities: PeerCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalDeviceView {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub arch: mf_protocol::ArchKind,
    pub listen_port: u16,
    pub local_ips: Vec<String>,
    pub monitors: Vec<Monitor>,
    pub permissions: PermissionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPairView {
    pub request_id: u64,
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub addr: String,
    pub pin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterSnapshot {
    pub local: LocalDeviceView,
    pub sharing_enabled: bool,
    pub active_controller_epoch: u64,
    pub active_controller: DeviceId,
    pub focused_device: DeviceId,
    pub is_grabbing: bool,
    pub peers: Vec<PeerView>,
    pub discovered: Vec<DiscoveredPeer>,
    pub pairing: PairingStatus,
    pub pending_pair_requests: Vec<PendingPairView>,
    pub topology: ClusterTopology,
    pub config: AppConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum EngineEvent {
    Snapshot(ClusterSnapshot),
    IncomingPair(PendingPairView),
    Notification { level: String, message: String },
}

pub enum EngineCommand {
    GetSnapshot(oneshot::Sender<ClusterSnapshot>),
    OpenPairingWindow {
        custom_pin: Option<String>,
        reply: oneshot::Sender<Result<PairingStatus>>,
    },
    ClosePairingWindow,
    RespondPairRequest {
        request_id: u64,
        accept: bool,
    },
    PairWithAddress {
        addr: SocketAddr,
        expected_id: Option<DeviceId>,
        pin: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ConnectAddress {
        peer_id: DeviceId,
        addr: SocketAddr,
        reply: oneshot::Sender<Result<()>>,
    },
    DisconnectPeer(DeviceId),
    RenamePeer {
        device_id: DeviceId,
        alias: Option<String>,
    },
    SetPeerEnabled {
        device_id: DeviceId,
        enabled: bool,
    },
    RevokePeer(DeviceId),
    SetController(DeviceId),
    FocusDevice(DeviceId),
    EmergencyRelease,
    SetSharingEnabled(bool),
    UpdatePlacements {
        placements: Vec<DevicePlacement>,
        boundary_mode: BoundaryMode,
    },
    UpdateConfig(AppConfig),
    ClearClipboardCache,
    RefreshMonitors,
    OpenPermissionSettings(String),
    Shutdown,
}

/// Cloneable async handle to control the running [`MoveFreelyEngine`].
#[derive(Clone)]
pub struct EngineHandle {
    pub local_id: DeviceId,
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    events_tx: broadcast::Sender<EngineEvent>,
}

impl EngineHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events_tx.subscribe()
    }

    pub async fn snapshot(&self) -> Result<ClusterSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(EngineCommand::GetSnapshot(tx))?;
        Ok(rx.await?)
    }

    pub async fn open_pairing_window(&self, custom_pin: Option<String>) -> Result<PairingStatus> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(EngineCommand::OpenPairingWindow {
            custom_pin,
            reply: tx,
        })?;
        rx.await?
    }

    pub fn close_pairing_window(&self) {
        let _ = self.cmd_tx.send(EngineCommand::ClosePairingWindow);
    }

    pub fn respond_pair_request(&self, request_id: u64, accept: bool) {
        let _ = self
            .cmd_tx
            .send(EngineCommand::RespondPairRequest { request_id, accept });
    }

    pub async fn pair_with_addr(
        &self,
        addr: SocketAddr,
        expected_id: Option<DeviceId>,
        pin: String,
    ) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(EngineCommand::PairWithAddress {
            addr,
            expected_id,
            pin,
            reply: tx,
        })?;
        rx.await?
    }

    pub async fn connect_addr(&self, peer_id: DeviceId, addr: SocketAddr) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(EngineCommand::ConnectAddress {
            peer_id,
            addr,
            reply: tx,
        })?;
        rx.await?
    }

    pub fn disconnect_peer(&self, id: DeviceId) {
        let _ = self.cmd_tx.send(EngineCommand::DisconnectPeer(id));
    }

    pub fn rename_peer(&self, device_id: DeviceId, alias: Option<String>) {
        let _ = self
            .cmd_tx
            .send(EngineCommand::RenamePeer { device_id, alias });
    }

    pub fn set_peer_enabled(&self, device_id: DeviceId, enabled: bool) {
        let _ = self
            .cmd_tx
            .send(EngineCommand::SetPeerEnabled { device_id, enabled });
    }

    pub fn revoke_peer(&self, device_id: DeviceId) {
        let _ = self.cmd_tx.send(EngineCommand::RevokePeer(device_id));
    }

    pub fn set_controller(&self, device_id: DeviceId) {
        let _ = self.cmd_tx.send(EngineCommand::SetController(device_id));
    }

    pub fn focus_device(&self, device_id: DeviceId) {
        let _ = self.cmd_tx.send(EngineCommand::FocusDevice(device_id));
    }

    pub fn emergency_release(&self) {
        let _ = self.cmd_tx.send(EngineCommand::EmergencyRelease);
    }

    pub fn set_sharing_enabled(&self, enabled: bool) {
        let _ = self.cmd_tx.send(EngineCommand::SetSharingEnabled(enabled));
    }

    pub fn update_placements(&self, placements: Vec<DevicePlacement>, boundary_mode: BoundaryMode) {
        let _ = self.cmd_tx.send(EngineCommand::UpdatePlacements {
            placements,
            boundary_mode,
        });
    }

    pub fn update_config(&self, config: AppConfig) {
        let _ = self.cmd_tx.send(EngineCommand::UpdateConfig(config));
    }

    pub fn clear_clipboard_cache(&self) {
        let _ = self.cmd_tx.send(EngineCommand::ClearClipboardCache);
    }

    pub fn refresh_monitors(&self) {
        let _ = self.cmd_tx.send(EngineCommand::RefreshMonitors);
    }

    pub fn open_permission_settings(&self, kind: String) {
        let _ = self
            .cmd_tx
            .send(EngineCommand::OpenPermissionSettings(kind));
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(EngineCommand::Shutdown);
    }
}

/// Optional direct access to headless test instrumentation when started in `EngineMode::Headless`.
pub struct HeadlessTestHandles {
    pub capture_tx: mpsc::UnboundedSender<InputEvent>,
    pub capture_ctrl: Arc<HeadlessCaptureController>,
    pub injector: HeadlessInjector,
    pub clipboard: Arc<Mutex<MemoryClipboard>>,
}

pub struct MoveFreelyEngine;

struct PendingPromptEntry {
    view: PendingPairView,
    responder: oneshot::Sender<bool>,
}

struct EngineState {
    config_path: PathBuf,
    config: Arc<Mutex<AppConfig>>,
    local_id: DeviceId,
    local_monitors: Vec<Monitor>,
    permissions: PermissionStatus,
    actual_listen_port: u16,

    // Ownership & routing state
    controller_epoch: u64,
    active_controller: DeviceId,
    focused_device: DeviceId,
    remote_routing: Option<PointerRoutingState>,
    routing_topology: Option<ClusterTopology>,
    local_resistance: EdgeResistanceTracker,
    emergency_cooldown_until: Option<Instant>,
    remote_held: HeldInputTracker,
    last_local_sync_sent: Instant,

    // Connected & discovered peer metadata
    connected_peers: HashMap<DeviceId, PeerInfo>,
    discovered_peers: Vec<DiscoveredPeer>,
    reconnect_backoff: HashMap<DeviceId, (Instant, u32)>,

    // Pending pair prompts
    next_prompt_id: u64,
    pending_prompts: HashMap<u64, PendingPromptEntry>,

    // Subsystems
    net: NetworkManager,
    discovery: DiscoveryService,
    capture_ctrl: Option<Arc<dyn CaptureController>>,
    injector: Option<Box<dyn PlatformInjector>>,
    clipboard_coord: Arc<Mutex<ClipboardSyncCoordinator>>,
    clipboard_backend: Arc<Mutex<Box<dyn ClipboardBackend>>>,
    events_tx: broadcast::Sender<EngineEvent>,
    started_at: Instant,
}

impl MoveFreelyEngine {
    pub async fn start(
        data_dir: Option<PathBuf>,
        override_config: Option<AppConfig>,
        mode: EngineMode,
    ) -> Result<(EngineHandle, Option<HeadlessTestHandles>)> {
        let data_dir = data_dir.unwrap_or_else(default_data_dir);
        std::fs::create_dir_all(&data_dir)?;
        let config_path = data_dir.join("config.toml");

        let mut cfg = match override_config {
            Some(c) => {
                c.save(&config_path)?;
                c
            }
            None => AppConfig::load_or_create(&config_path)?,
        };

        let identity = DeviceIdentity::load_or_generate(&data_dir)?;
        let trust_store = TrustedPeerStore::load_or_create(&data_dir)?;
        let pairing = Arc::new(PairingManager::new());

        let (local_monitors, permissions, mut capture_rx_opt, capture_ctrl, injector, headless_handles) =
            match mode {
                EngineMode::Native => {
                    let mons = enumerate_monitors();
                    let mut perms = check_permissions(false);
                    let (cap_rx, cap_ctrl) = match start_native_capture() {
                        Ok(s) => (Some(s.events), Some(s.controller)),
                        Err(e) => {
                            tracing::warn!(error = %e, "native input capture unavailable");
                            (None, None)
                        }
                    };
                    let inj = match new_native_injector() {
                        Ok(i) => Some(i),
                        Err(e) => {
                            tracing::warn!(error = %e, "native input injection unavailable");
                            None
                        }
                    };
                    perms.can_capture &= cap_ctrl.is_some();
                    perms.can_inject &= inj.is_some();
                    (mons, perms, cap_rx, cap_ctrl, inj, None)
                }
                EngineMode::Headless { echo, monitors } => {
                    let mons = if monitors.is_empty() {
                        vec![crate::input::default_fallback_monitor()]
                    } else {
                        monitors
                    };
                    let perms = PermissionStatus {
                        accessibility_granted: true,
                        input_monitoring_granted: true,
                        can_capture: true,
                        can_inject: true,
                        session_type: "headless".into(),
                        summary: "Headless virtual backend active".into(),
                        action_hint: None,
                    };
                    let (cap_sess, cap_tx, cap_ctrl) = new_headless_capture();
                    let inj = HeadlessInjector::new(echo);
                    let mem_clip = Arc::new(Mutex::new(MemoryClipboard::default()));
                    let handles = HeadlessTestHandles {
                        capture_tx: cap_tx,
                        capture_ctrl: cap_ctrl.clone(),
                        injector: inj.clone(),
                        clipboard: mem_clip,
                    };
                    (
                        mons,
                        perms,
                        Some(cap_sess.events),
                        Some(cap_ctrl as Arc<dyn CaptureController>),
                        Some(Box::new(inj) as Box<dyn PlatformInjector>),
                        Some(handles),
                    )
                }
            };

        let local_id = identity.device_id.clone();
        // Ensure local device has a placement entry
        if !cfg.placements.iter().any(|p| p.device_id == local_id) {
            cfg.placements.push(DevicePlacement {
                device_id: local_id.clone(),
                x: 0,
                y: 0,
                enabled_edges: EdgeMask::default(),
            });
            let _ = cfg.save(&config_path);
        }

        let shared_cfg = Arc::new(Mutex::new(cfg.clone()));
        let (net_tx, mut net_rx) = mpsc::unbounded_channel::<NetworkEvent>();
        let capabilities = PeerCapabilities {
            can_capture: permissions.can_capture,
            can_inject: permissions.can_inject,
            clipboard_text: cfg.clipboard.enabled && cfg.clipboard.sync_text,
            clipboard_image: cfg.clipboard.enabled && cfg.clipboard.sync_images,
            clipboard_files: cfg.clipboard.enabled && cfg.clipboard.sync_files,
            wayland_session: permissions.session_type == "wayland",
            status_note: permissions.action_hint.clone(),
        };

        let net = NetworkManager::new(
            identity.clone(),
            trust_store.clone(),
            pairing.clone(),
            shared_cfg.clone(),
            LocalAdvertiseState {
                name: cfg.device_name.clone(),
                monitors: local_monitors.clone(),
                capabilities,
                active_controller: Some((1, local_id.clone())),
            },
            net_tx,
        );

        let (shut_tx, shut_rx) = watch::channel(false);
        let listen_addr = net.start_listener(shut_rx).await?;
        let actual_port = listen_addr.port();

        let discovery = DiscoveryService::start(
            local_id.clone(),
            &cfg.device_name,
            actual_port,
            false,
        )?;
        let mut disc_rx = discovery.subscribe();

        let (events_tx, _) = broadcast::channel(64);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<EngineCommand>();

        let staging_root = std::env::temp_dir().join(format!("move-freely-clip-{}", local_id.short()));
        let clipboard_coord = Arc::new(Mutex::new(ClipboardSyncCoordinator::new(
            local_id.clone(),
            staging_root,
        )));

        let clipboard_backend: Arc<Mutex<Box<dyn ClipboardBackend>>> = match &headless_handles {
            Some(h) => Arc::new(Mutex::new(Box::new(SharedMemoryClipboardAdapter(
                h.clipboard.clone(),
            )))),
            None => {
                let backend: Box<dyn ClipboardBackend> = new_platform_clipboard()
                    .unwrap_or_else(|_| Box::new(MemoryClipboard::default()));
                Arc::new(Mutex::new(backend))
            }
        };

        let mut state = EngineState {
            config_path,
            config: shared_cfg,
            local_id: local_id.clone(),
            local_monitors,
            permissions,
            actual_listen_port: actual_port,
            controller_epoch: 1,
            active_controller: local_id.clone(),
            focused_device: local_id.clone(),
            remote_routing: None,
            routing_topology: None,
            local_resistance: EdgeResistanceTracker::default(),
            emergency_cooldown_until: None,
            remote_held: HeldInputTracker::default(),
            last_local_sync_sent: Instant::now(),
            connected_peers: HashMap::new(),
            discovered_peers: Vec::new(),
            reconnect_backoff: HashMap::new(),
            next_prompt_id: 1,
            pending_prompts: HashMap::new(),
            net,
            discovery,
            capture_ctrl,
            injector,
            clipboard_coord,
            clipboard_backend,
            events_tx: events_tx.clone(),
            started_at: Instant::now(),
        };

        let handle = EngineHandle {
            local_id: local_id.clone(),
            cmd_tx: cmd_tx.clone(),
            events_tx: events_tx.clone(),
        };

        // Main event-loop task
        tokio::spawn(async move {
            let mut supervisor_tick = tokio::time::interval(Duration::from_secs(1));
            supervisor_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut clip_tick = tokio::time::interval(Duration::from_millis(250));
            clip_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_wall = SystemTime::now();
            let mut last_clip_token: Option<u64> = None;
            let mut clip_poll_counter: u32 = 0;

            loop {
                tokio::select! {
                    Some(cmd) = cmd_rx.recv() => {
                        if matches!(cmd, EngineCommand::Shutdown) {
                            state.shutdown();
                            let _ = shut_tx.send(true);
                            break;
                        }
                        state.handle_command(cmd);
                    }
                    Some(nev) = net_rx.recv() => {
                        state.handle_network_event(nev);
                    }
                    Ok(peers) = disc_rx.recv() => {
                        state.discovered_peers = peers;
                        state.trigger_auto_connect();
                        state.emit_snapshot();
                    }
                    Some(iev) = async {
                        match capture_rx_opt.as_mut() {
                            Some(rx) => rx.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        state.handle_input_event(iev);
                    }
                    _ = clip_tick.tick() => {
                        clip_poll_counter = clip_poll_counter.wrapping_add(1);
                        state.poll_local_clipboard(&mut last_clip_token, clip_poll_counter);
                    }
                    _ = supervisor_tick.tick() => {
                        // Detect sleep/wake jump (>4s gap on a 1s tick)
                        let now_wall = SystemTime::now();
                        let jumped = now_wall
                            .duration_since(last_wall)
                            .map(|d| d > Duration::from_secs(4))
                            .unwrap_or(true);
                        last_wall = now_wall;
                        if jumped {
                            tracing::info!("system wake detected — releasing grabs, refreshing monitors & mDNS");
                            state.emergency_release();
                            state.refresh_local_monitors();
                            let (name, port, pair_open) = {
                                let cfg = state.config.lock().unwrap();
                                (cfg.device_name.clone(), state.actual_listen_port, state.net.pairing.current_pin().is_some())
                            };
                            let _ = state.discovery.advertise(&name, port, pair_open);
                            state.reconnect_backoff.clear();
                        }
                        state.trigger_auto_connect();
                    }
                }
            }
        });

        Ok((handle, headless_handles))
    }
}

impl EngineState {
    fn snapshot(&self) -> ClusterSnapshot {
        let cfg = self.config.lock().unwrap().clone();
        let trusted = self.net.trust_store.list();
        let connected = self.net.connected_peers();
        let connected_map: HashMap<&DeviceId, &PeerInfo> =
            connected.iter().map(|p| (&p.device_id, p)).collect();

        let peers: Vec<PeerView> = trusted
            .iter()
            .map(|tp| {
                let live = connected_map.get(&tp.device_id).copied();
                PeerView {
                    device_id: tp.device_id.clone(),
                    name: tp.name.clone(),
                    display_name: tp.display_name().to_string(),
                    os: tp.os,
                    arch: tp.arch,
                    enabled: tp.enabled,
                    connected: live.is_some(),
                    latency_ms: live.map(|l| l.latency_ms).unwrap_or(0),
                    addr: live
                        .map(|l| l.addr.to_string())
                        .or_else(|| tp.last_addrs.first().map(|a| a.to_string())),
                    monitors: live.map(|l| l.monitors.clone()).unwrap_or_default(),
                    capabilities: self
                        .connected_peers
                        .get(&tp.device_id)
                        .or(live)
                        .map(|peer| peer.capabilities.clone())
                        .unwrap_or_default(),
                }
            })
            .collect();

        // Filter mDNS list to devices not already paired (or show pairing state)
        let discovered: Vec<DiscoveredPeer> = self
            .discovered_peers
            .iter()
            .filter(|d| self.net.trust_store.get(&d.device_id).is_none())
            .cloned()
            .collect();

        let topology = self.build_topology();
        let pending_pair_requests = self
            .pending_prompts
            .values()
            .map(|p| p.view.clone())
            .collect();

        ClusterSnapshot {
            local: LocalDeviceView {
                device_id: self.local_id.clone(),
                name: cfg.device_name.clone(),
                os: OsKind::current(),
                arch: mf_protocol::ArchKind::current(),
                listen_port: self.actual_listen_port,
                local_ips: local_ip_addresses().into_iter().map(|ip| ip.to_string()).collect(),
                monitors: self.local_monitors.clone(),
                permissions: self.permissions.clone(),
            },
            sharing_enabled: cfg.sharing_enabled,
            active_controller_epoch: self.controller_epoch,
            active_controller: self.active_controller.clone(),
            focused_device: self.focused_device.clone(),
            is_grabbing: self
                .capture_ctrl
                .as_ref()
                .is_some_and(|c| c.is_grabbing()),
            peers,
            discovered,
            pairing: self.net.pairing.status(),
            pending_pair_requests,
            topology,
            config: cfg,
        }
    }

    fn emit_snapshot(&self) {
        let _ = self.events_tx.send(EngineEvent::Snapshot(self.snapshot()));
    }

    fn notify(&self, level: &str, message: impl Into<String>) {
        let _ = self.events_tx.send(EngineEvent::Notification {
            level: level.into(),
            message: message.into(),
        });
    }

    fn build_topology(&self) -> ClusterTopology {
        let cfg = self.config.lock().unwrap();
        let mut nodes = Vec::new();
        let local_placement = cfg
            .placements
            .iter()
            .find(|p| p.device_id == self.local_id)
            .cloned()
            .unwrap_or(DevicePlacement {
                device_id: self.local_id.clone(),
                x: 0,
                y: 0,
                enabled_edges: EdgeMask::default(),
            });
        nodes.push(DeviceTopology {
            device_id: self.local_id.clone(),
            name: cfg.device_name.clone(),
            monitors: self.local_monitors.clone(),
            placement: local_placement,
            enabled: true,
        });

        for tp in self.net.trust_store.list() {
            let live = self.connected_peers.get(&tp.device_id);
            let mons = live
                .map(|p| p.monitors.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| vec![crate::input::default_fallback_monitor()]);
            let placement = cfg
                .placements
                .iter()
                .find(|p| p.device_id == tp.device_id)
                .cloned()
                .unwrap_or(DevicePlacement {
                    device_id: tp.device_id.clone(),
                    x: 1920,
                    y: 0,
                    enabled_edges: EdgeMask::default(),
                });
            let disp_name = tp.display_name().to_string();
            nodes.push(DeviceTopology {
                device_id: tp.device_id,
                name: disp_name,
                monitors: mons,
                placement,
                enabled: tp.enabled && live.is_some(),
            });
        }
        ClusterTopology::new(nodes, cfg.boundary_mode)
    }

    fn ensure_placement_for(&mut self, id: &DeviceId, monitors: &[Monitor]) {
        let topo = self.build_topology();
        let mut cfg = self.config.lock().unwrap();
        if cfg.placements.iter().any(|p| &p.device_id == id) {
            return;
        }
        // Auto-place immediately to the right of all existing screens.
        let max_right = topo
            .devices
            .iter()
            .filter(|d| &d.device_id != id)
            .filter_map(|d| topo.device_bounds(&d.device_id))
            .map(|r| r.right())
            .max()
            .unwrap_or(1440);
        let _ = monitors;
        cfg.placements.push(DevicePlacement {
            device_id: id.clone(),
            x: max_right,
            y: 0,
            enabled_edges: EdgeMask::default(),
        });
        let _ = cfg.save(&self.config_path);
    }

    fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::GetSnapshot(reply) => {
                let _ = reply.send(self.snapshot());
            }
            EngineCommand::OpenPairingWindow { custom_pin, reply } => {
                let res = self
                    .net
                    .pairing
                    .open_window(custom_pin, Duration::from_secs(120))
                    .map(|_| self.net.pairing.status());
                if res.is_ok() {
                    let (name, port) = {
                        let c = self.config.lock().unwrap();
                        (c.device_name.clone(), self.actual_listen_port)
                    };
                    let _ = self.discovery.advertise(&name, port, true);
                }
                let _ = reply.send(res);
                self.emit_snapshot();
            }
            EngineCommand::ClosePairingWindow => {
                self.net.pairing.close_window();
                let (name, port) = {
                    let c = self.config.lock().unwrap();
                    (c.device_name.clone(), self.actual_listen_port)
                };
                let _ = self.discovery.advertise(&name, port, false);
                self.emit_snapshot();
            }
            EngineCommand::RespondPairRequest { request_id, accept } => {
                if let Some(entry) = self.pending_prompts.remove(&request_id) {
                    let _ = entry.responder.send(accept);
                }
                self.emit_snapshot();
            }
            EngineCommand::PairWithAddress {
                addr,
                expected_id,
                pin,
                reply,
            } => {
                let net = self.net.clone();
                let cmd_tx_dummy = self.events_tx.clone();
                tokio::spawn(async move {
                    let res = net.pair_with_addr(addr, expected_id, &pin).await;
                    match res {
                        Ok(trusted) => {
                            let _ = net.connect_trusted_peer(trusted.device_id, addr).await;
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = cmd_tx_dummy.send(EngineEvent::Notification {
                                level: "error".into(),
                                message: format!("Pairing failed: {e}"),
                            });
                            let _ = reply.send(Err(e));
                        }
                    }
                });
            }
            EngineCommand::ConnectAddress {
                peer_id,
                addr,
                reply,
            } => {
                let net = self.net.clone();
                tokio::spawn(async move {
                    let res = net.connect_trusted_peer(peer_id, addr).await;
                    let _ = reply.send(res);
                });
            }
            EngineCommand::DisconnectPeer(id) => {
                self.routing_topology = None;
                if self.focused_device == id {
                    self.release_focus_to_local();
                }
                self.net.disconnect_peer(&id, "disconnected by user");
                self.emit_snapshot();
            }
            EngineCommand::RenamePeer { device_id, alias } => {
                if device_id == self.local_id {
                    if let Some(new_name) = alias.filter(|s| !s.trim().is_empty()) {
                        let port = self.actual_listen_port;
                        let pair_open = self.net.pairing.current_pin().is_some();
                        {
                            let mut c = self.config.lock().unwrap();
                            c.device_name = new_name.clone();
                            let _ = c.save(&self.config_path);
                        }
                        self.net.update_local_state(|s| s.name = new_name.clone());
                        let _ = self.discovery.advertise(&new_name, port, pair_open);
                    }
                } else {
                    let _ = self.net.trust_store.rename(&device_id, alias);
                }
                self.emit_snapshot();
            }
            EngineCommand::SetPeerEnabled { device_id, enabled } => {
                self.routing_topology = None;
                let _ = self.net.trust_store.set_enabled(&device_id, enabled);
                if !enabled {
                    if self.focused_device == device_id {
                        self.release_focus_to_local();
                    }
                    self.net.disconnect_peer(&device_id, "disabled by user");
                }
                self.emit_snapshot();
            }
            EngineCommand::RevokePeer(device_id) => {
                self.routing_topology = None;
                if self.focused_device == device_id {
                    self.release_focus_to_local();
                }
                self.net.disconnect_peer(&device_id, "device revoked");
                let _ = self.net.trust_store.revoke(&device_id);
                {
                    let mut c = self.config.lock().unwrap();
                    c.placements.retain(|p| p.device_id != device_id);
                    let _ = c.save(&self.config_path);
                }
                self.emit_snapshot();
            }
            EngineCommand::SetController(target) => {
                self.switch_controller_to(target);
                self.emit_snapshot();
            }
            EngineCommand::FocusDevice(target) => {
                self.focus_on_device(target);
                self.emit_snapshot();
            }
            EngineCommand::EmergencyRelease => {
                self.emergency_release();
                self.emit_snapshot();
            }
            EngineCommand::SetSharingEnabled(enabled) => {
                {
                    let mut c = self.config.lock().unwrap();
                    c.sharing_enabled = enabled;
                    let _ = c.save(&self.config_path);
                }
                if !enabled {
                    self.emergency_release();
                }
                self.emit_snapshot();
            }
            EngineCommand::UpdatePlacements {
                placements,
                boundary_mode,
            } => {
                self.routing_topology = None;
                {
                    let mut c = self.config.lock().unwrap();
                    c.placements = placements.clone();
                    c.boundary_mode = boundary_mode;
                    let _ = c.save(&self.config_path);
                }
                self.net.broadcast_normal(&Message::LayoutSync {
                    epoch: self.controller_epoch,
                    placements,
                    boundary_mode,
                });
                self.emit_snapshot();
            }
            EngineCommand::UpdateConfig(new_cfg) => {
                self.routing_topology = None;
                let old_startup = self.config.lock().unwrap().launch_on_startup;
                if new_cfg.launch_on_startup != old_startup {
                    if let Err(e) = autostart::set_launch_on_startup(new_cfg.launch_on_startup) {
                        self.notify("error", format!("Failed updating startup setting: {e}"));
                    }
                }
                {
                    let mut c = self.config.lock().unwrap();
                    *c = new_cfg.clone();
                    let _ = c.save(&self.config_path);
                }
                let caps = PeerCapabilities {
                    can_capture: self.permissions.can_capture,
                    can_inject: self.permissions.can_inject,
                    clipboard_text: new_cfg.clipboard.enabled && new_cfg.clipboard.sync_text,
                    clipboard_image: new_cfg.clipboard.enabled && new_cfg.clipboard.sync_images,
                    clipboard_files: new_cfg.clipboard.enabled && new_cfg.clipboard.sync_files,
                    wayland_session: self.permissions.session_type == "wayland",
                    status_note: self.permissions.action_hint.clone(),
                };
                self.net.update_local_state(|s| {
                    s.name = new_cfg.device_name.clone();
                    s.capabilities = caps.clone();
                });
                self.net
                    .broadcast_normal(&Message::CapabilitiesUpdated { capabilities: caps });
                self.emit_snapshot();
            }
            EngineCommand::ClearClipboardCache => {
                self.clipboard_coord.lock().unwrap().clear_all_cache();
                self.notify("info", "Cleared ephemeral clipboard staging cache");
            }
            EngineCommand::RefreshMonitors => {
                self.refresh_local_monitors();
                self.emit_snapshot();
            }
            EngineCommand::OpenPermissionSettings(kind) => {
                let _ = open_permission_settings(&kind);
            }
            EngineCommand::Shutdown => {}
        }
    }

    fn refresh_local_monitors(&mut self) {
        self.routing_topology = None;
        let mons = enumerate_monitors();
        self.local_monitors = mons.clone();
        self.net
            .update_local_state(|s| s.monitors = mons.clone());
        self.net
            .broadcast_normal(&Message::MonitorsUpdated { monitors: mons });
    }

    fn handle_network_event(&mut self, ev: NetworkEvent) {
        match ev {
            NetworkEvent::PeerConnected(info) => {
                self.routing_topology = None;
                let pid = info.device_id.clone();
                self.ensure_placement_for(&pid, &info.monitors);
                if let Some((r_epoch, r_ctrl)) = &info.active_controller {
                    if (*r_epoch, r_ctrl.clone()) > (self.controller_epoch, self.active_controller.clone()) {
                        self.controller_epoch = *r_epoch;
                        self.active_controller = r_ctrl.clone();
                        self.focused_device = r_ctrl.clone();
                        self.release_local_grab();
                    }
                }
                // Check if preferred_controller matches newly joined peer
                let preferred = self.config.lock().unwrap().preferred_controller.clone();
                if preferred.as_ref() == Some(&pid) && self.active_controller != pid {
                    self.switch_controller_to(pid.clone());
                } else if self.active_controller == self.local_id {
                    self.net.send_high(
                        &pid,
                        Message::ControllerAck {
                            epoch: self.controller_epoch,
                            controller_id: self.local_id.clone(),
                        },
                    );
                    let cfg = self.config.lock().unwrap();
                    self.net.send_normal(
                        &pid,
                        Message::LayoutSync {
                            epoch: self.controller_epoch,
                            placements: cfg.placements.clone(),
                            boundary_mode: cfg.boundary_mode,
                        },
                    );
                }
                self.connected_peers.insert(pid, info);
                self.emit_snapshot();
            }
            NetworkEvent::PeerDisconnected { device_id, reason } => {
                self.routing_topology = None;
                self.connected_peers.remove(&device_id);
                if self.focused_device == device_id {
                    tracing::warn!(%device_id, %reason, "focused peer disconnected — returning control to local");
                    self.release_focus_to_local();
                }
                if self.active_controller == device_id {
                    // Reclaim controller locally if active controller went offline.
                    self.controller_epoch += 1;
                    self.active_controller = self.local_id.clone();
                    self.focused_device = self.local_id.clone();
                    if let Some(inj) = self.injector.as_mut() {
                        let _ = inj.release_all();
                    }
                }
                self.emit_snapshot();
            }
            NetworkEvent::LatencyUpdated {
                device_id,
                latency_ms,
            } => {
                if let Some(p) = self.connected_peers.get_mut(&device_id) {
                    p.latency_ms = latency_ms;
                }
                self.emit_snapshot();
            }
            NetworkEvent::IncomingPairRequest(prompt) => {
                let req_id = self.next_prompt_id;
                self.next_prompt_id += 1;
                let view = PendingPairView {
                    request_id: req_id,
                    device_id: prompt.device_id,
                    name: prompt.name,
                    os: prompt.os,
                    addr: prompt.addr.to_string(),
                    pin: prompt.pin,
                };
                self.pending_prompts.insert(
                    req_id,
                    PendingPromptEntry {
                        view: view.clone(),
                        responder: prompt.responder,
                    },
                );
                let _ = self.events_tx.send(EngineEvent::IncomingPair(view));
                self.emit_snapshot();
            }
            NetworkEvent::ControlMessage { from, msg } => {
                self.handle_control_message(from, msg);
            }
            NetworkEvent::DataChunk { from, chunk } => {
                self.handle_data_chunk(from, chunk);
            }
        }
    }

    fn handle_control_message(&mut self, from: DeviceId, msg: Message) {
        match msg {
            Message::MonitorsUpdated { monitors } => {
                self.routing_topology = None;
                if let Some(p) = self.connected_peers.get_mut(&from) {
                    p.monitors = monitors;
                }
                self.emit_snapshot();
            }
            Message::CapabilitiesUpdated { capabilities } => {
                self.routing_topology = None;
                let can_inject = capabilities.can_inject;
                if let Some(p) = self.connected_peers.get_mut(&from) {
                    p.capabilities = capabilities;
                }
                if !can_inject && self.active_controller == self.local_id && self.focused_device == from {
                    self.release_focus_to_local();
                }
                self.emit_snapshot();
            }
            Message::LayoutSync {
                epoch,
                placements,
                boundary_mode,
            } => {
                if epoch >= self.controller_epoch {
                    self.routing_topology = None;
                    let mut c = self.config.lock().unwrap();
                    c.placements = placements;
                    c.boundary_mode = boundary_mode;
                    let _ = c.save(&self.config_path);
                    drop(c);
                    self.emit_snapshot();
                }
            }
            Message::ClaimController {
                epoch,
                controller_id,
            }
            | Message::ControllerAck {
                epoch,
                controller_id,
            } => {
                let allow_remote = self.config.lock().unwrap().allow_remote_claim_controller;
                if !allow_remote && controller_id != self.local_id {
                    self.controller_epoch = epoch.max(self.controller_epoch) + 1;
                    self.active_controller = self.local_id.clone();
                    self.net.broadcast_high(&Message::ControllerAck {
                        epoch: self.controller_epoch,
                        controller_id: self.local_id.clone(),
                    });
                    return;
                }
                if (epoch, controller_id.clone())
                    > (self.controller_epoch, self.active_controller.clone())
                {
                    if self.active_controller == self.local_id && controller_id != self.local_id {
                        self.release_focus_to_local();
                    }
                    self.controller_epoch = epoch;
                    self.active_controller = controller_id.clone();
                    self.focused_device = controller_id.clone();
                    self.net.update_local_state(|s| {
                        s.active_controller = Some((epoch, controller_id.clone()));
                    });
                    if let Some(inj) = self.injector.as_mut() {
                        let _ = inj.release_all();
                    }
                    self.emit_snapshot();
                }
            }
            Message::FocusChange {
                epoch,
                controller_id,
                target_device,
            } => {
                if (epoch, controller_id.clone())
                    >= (self.controller_epoch, self.active_controller.clone())
                {
                    self.controller_epoch = epoch;
                    self.active_controller = controller_id;
                    self.focused_device = target_device;
                    self.emit_snapshot();
                }
            }
            // Input injection from the active controller!
            Message::Enter { x, y } => {
                if from != self.active_controller || self.active_controller == self.local_id {
                    // Reject input from a non-controller peer to prevent competing controllers.
                    return;
                }
                self.focused_device = self.local_id.clone();
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.warp_cursor(x, y);
                }
                self.emit_snapshot();
            }
            Message::Leave | Message::ReleaseAllInput => {
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.release_all();
                }
            }
            Message::PointerMoveAbs { x, y, dx, dy } => {
                if !self.receives_remote_mouse(&from) {
                    return;
                }
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.inject_pointer_abs(x, y, dx, dy);
                }
            }
            Message::PointerMoveRel { dx, dy } => {
                if !self.receives_remote_mouse(&from) {
                    return;
                }
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.inject_pointer_rel(dx, dy);
                }
            }
            Message::MouseButton { button, pressed } => {
                if !self.receives_remote_mouse(&from) {
                    return;
                }
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.inject_button(button, pressed);
                }
            }
            Message::Scroll { dx, dy, continuous } => {
                if !self.receives_remote_mouse(&from) {
                    return;
                }
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.inject_scroll(dx, dy, continuous);
                }
            }
            Message::KeyEvent {
                key,
                pressed,
                text,
                modifiers,
            } => {
                if from != self.active_controller
                    || self.focused_device != self.local_id
                    || !self.config.lock().unwrap().keyboard_sharing
                {
                    return;
                }
                if let Some(inj) = self.injector.as_mut() {
                    let _ = inj.inject_key(key, pressed, text.as_deref(), modifiers);
                }
            }
            Message::LocalCursorSync { x, y } => {
                // Controlled device moved its own physical mouse -> keep controller virtual
                // cursor aligned without jumping!
                if self.active_controller == self.local_id && self.focused_device == from {
                    let topo = self.build_topology();
                    if let Some((cx, cy)) = topo.os_to_canvas(&from, x, y) {
                        if let Some(st) = self.remote_routing.as_mut() {
                            if st.focused_device == from {
                                st.canvas_x = cx;
                                st.canvas_y = cy;
                            }
                        }
                    }
                }
            }
            Message::ClipboardText {
                seq,
                origin,
                sha256,
                plain,
                html,
                rtf,
            } => {
                let clip_cfg = self.config.lock().unwrap().clipboard.clone();
                let maybe_payload = self.clipboard_coord.lock().unwrap().on_remote_text(
                    seq, origin, sha256, plain, html, rtf, &clip_cfg,
                );
                if let Some(payload) = maybe_payload {
                    if let Err(e) = self.clipboard_backend.lock().unwrap().write_snapshot(&payload) {
                        tracing::warn!(error = %e, "failed writing clipboard text");
                    }
                }
            }
            Message::ClipboardImageOffer {
                transfer_id,
                seq,
                origin,
                sha256,
                total_bytes,
                width,
                height,
            } => {
                let clip_cfg = self.config.lock().unwrap().clipboard.clone();
                let accepted = self
                    .clipboard_coord
                    .lock()
                    .unwrap()
                    .on_remote_image_offer(
                        transfer_id,
                        seq,
                        origin,
                        sha256,
                        total_bytes,
                        width,
                        height,
                        &clip_cfg,
                    );
                self.net.send_normal(
                    &from,
                    Message::TransferAck {
                        transfer_id,
                        accepted,
                        reason: if accepted {
                            None
                        } else {
                            Some("rejected by clipboard policy or stale".into())
                        },
                    },
                );
            }
            Message::ClipboardFilesOffer {
                transfer_id,
                seq,
                origin,
                sha256,
                total_bytes,
                entries,
            } => {
                let clip_cfg = self.config.lock().unwrap().clipboard.clone();
                let res = self.clipboard_coord.lock().unwrap().on_remote_files_offer(
                    transfer_id,
                    seq,
                    origin,
                    sha256,
                    total_bytes,
                    entries,
                    &clip_cfg,
                );
                let (accepted, reason) = match res {
                    Ok(true) => (true, None),
                    Ok(false) => (false, Some("rejected by policy".into())),
                    Err(e) => (false, Some(e.to_string())),
                };
                self.net.send_normal(
                    &from,
                    Message::TransferAck {
                        transfer_id,
                        accepted,
                        reason,
                    },
                );
            }
            _ => {}
        }
    }

    fn handle_data_chunk(&mut self, from: DeviceId, chunk: BinaryChunk) {
        let tid = chunk.transfer_id;
        let res = self.clipboard_coord.lock().unwrap().on_data_chunk(chunk);
        match res {
            Ok(Some(payload)) => {
                if let Err(e) = self.clipboard_backend.lock().unwrap().write_snapshot(&payload) {
                    tracing::warn!(error = %e, "failed writing transferred clipboard payload");
                }
                self.net.send_normal(
                    &from,
                    Message::TransferComplete {
                        transfer_id: tid,
                        ok: true,
                        error: None,
                    },
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, transfer_id = tid, "clipboard transfer failed");
                self.net.send_normal(
                    &from,
                    Message::TransferComplete {
                        transfer_id: tid,
                        ok: false,
                        error: Some(e.to_string()),
                    },
                );
            }
        }
    }

    fn receives_remote_mouse(&self, from: &DeviceId) -> bool {
        from == &self.active_controller
            && self.focused_device == self.local_id
            && self.config.lock().unwrap().mouse_sharing
    }

    fn handle_input_event(&mut self, ev: InputEvent) {
        if matches!(ev, InputEvent::MonitorsChanged) {
            self.refresh_local_monitors();
            self.emit_snapshot();
            return;
        }

        // 1. Check global shortcuts on key press!
        if let InputEvent::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = &ev
        {
            let shortcuts = self.config.lock().unwrap().shortcuts.clone();
            if shortcuts.emergency_release.matches(*key, *modifiers) {
                tracing::warn!("Emergency release shortcut triggered");
                self.emergency_release();
                self.emit_snapshot();
                return;
            }
            if shortcuts.toggle_sharing.matches(*key, *modifiers) {
                let next = !self.config.lock().unwrap().sharing_enabled;
                self.handle_command(EngineCommand::SetSharingEnabled(next));
                return;
            }
            if shortcuts.claim_controller.matches(*key, *modifiers) {
                self.switch_controller_to(self.local_id.clone());
                self.emit_snapshot();
                return;
            }
            if shortcuts.cycle_screen.matches(*key, *modifiers) {
                if self.active_controller != self.local_id {
                    self.switch_controller_to(self.local_id.clone());
                }
                self.cycle_focused_screen();
                self.emit_snapshot();
                return;
            }
        }

        let (mouse_sharing, keyboard_sharing, sensitivity, invert_scroll, scroll_speed, modifier_mapping) = {
            let cfg = self.config.lock().unwrap();
            if !cfg.sharing_enabled {
                return;
            }
            (
                cfg.mouse_sharing,
                cfg.keyboard_sharing,
                cfg.sensitivity.clone(),
                cfg.invert_remote_scroll,
                cfg.scroll_speed,
                cfg.modifier_mapping,
            )
        };

        // If we are NOT the active controller, report local physical mouse activity
        // to the active controller (rate-limited) so its virtual pointer stays synced!
        if self.active_controller != self.local_id {
            if let InputEvent::LocalPointerMove { x, y, .. } = ev {
                if self.focused_device == self.local_id
                    && self.last_local_sync_sent.elapsed() >= Duration::from_millis(50)
                {
                    self.last_local_sync_sent = Instant::now();
                    self.net.send_high(
                        &self.active_controller,
                        Message::LocalCursorSync { x, y },
                    );
                }
            }
            return;
        }

        if matches!(ev, InputEvent::LocalPointerMove { .. } | InputEvent::RemotePointerDelta { .. })
            && self.routing_topology.is_none()
        {
            let mut topo = self.build_topology();
            for device in &mut topo.devices {
                if device.device_id != self.local_id {
                    device.enabled &= self
                        .connected_peers
                        .get(&device.device_id)
                        .is_some_and(|peer| peer.capabilities.can_inject);
                }
            }
            self.routing_topology = Some(topo);
        }
        let now_ms = self.started_at.elapsed().as_millis() as u64;

        // 2. Active controller on local screen -> check automatic edge crossing!
        if self.focused_device == self.local_id {
            if let (InputEvent::LocalPointerMove { x, y, dx, dy }, Some(topo)) =
                (ev, &self.routing_topology)
            {
                if !mouse_sharing {
                    return;
                }
                if self
                    .emergency_cooldown_until
                    .is_some_and(|t| Instant::now() < t)
                {
                    return;
                }
                if let Some(StepOutcome::Crossed {
                    target_device,
                    os_x,
                    os_y,
                    canvas_x,
                    canvas_y,
                }) = topo.check_local_edge_crossing(
                    &self.local_id,
                    x,
                    y,
                    dx,
                    dy,
                    now_ms,
                    &mut self.local_resistance,
                    &sensitivity,
                ) {
                    self.enter_remote_device(target_device, os_x, os_y, canvas_x, canvas_y);
                    self.emit_snapshot();
                }
            }
            return;
        }

        // 3. Active controller driving a REMOTE screen!
        let target = &self.focused_device;
        match ev {
            InputEvent::RemotePointerDelta { dx, dy } => {
                if !mouse_sharing {
                    return;
                }
                let (Some(st), Some(topo)) = (self.remote_routing.as_mut(), &self.routing_topology)
                else {
                    return;
                };
                if let Some(outcome) = topo.step_remote(st, dx, dy, now_ms, &sensitivity) {
                    match outcome {
                        StepOutcome::Stay { os_x, os_y } => {
                            self.net.send_high(
                                target,
                                Message::PointerMoveAbs { x: os_x, y: os_y, dx, dy },
                            );
                        }
                        StepOutcome::Crossed {
                            target_device,
                            os_x,
                            os_y,
                            canvas_x,
                            canvas_y,
                        } => {
                            let target = target.clone();
                            self.release_remote_held_on(&target);
                            self.net.send_high(&target, Message::Leave);
                            if target_device == self.local_id {
                                self.focused_device = self.local_id.clone();
                                self.remote_routing = None;
                                if let Some(ctrl) = &self.capture_ctrl {
                                    ctrl.set_grabbing(false, Some((os_x, os_y)));
                                }
                                self.net.broadcast_high(&Message::FocusChange {
                                    epoch: self.controller_epoch,
                                    controller_id: self.local_id.clone(),
                                    target_device: self.local_id.clone(),
                                });
                            } else {
                                self.enter_remote_device(
                                    target_device,
                                    os_x,
                                    os_y,
                                    canvas_x,
                                    canvas_y,
                                );
                            }
                            self.emit_snapshot();
                        }
                    }
                }
            }
            InputEvent::MouseButton { button, pressed } => {
                if mouse_sharing {
                    self.remote_held.record_button(button, pressed);
                    self.net
                        .send_high(target, Message::MouseButton { button, pressed });
                }
            }
            InputEvent::Scroll { dx, dy, continuous } => {
                if mouse_sharing {
                    let sign = if invert_scroll { -1.0 } else { 1.0 };
                    let speed = scroll_speed.clamp(0.1, 5.0) * sign;
                    self.net.send_high(
                        target,
                        Message::Scroll {
                            dx: dx * speed,
                            dy: dy * speed,
                            continuous,
                        },
                    );
                }
            }
            InputEvent::Key {
                key,
                pressed,
                text,
                modifiers,
            } => {
                if keyboard_sharing {
                    let target_os = self
                        .connected_peers
                        .get(target)
                        .map(|p| p.os)
                        .unwrap_or(OsKind::Other);
                    let (mapped_key, mapped_mods) = remap_key_for_target(
                        key,
                        modifiers,
                        OsKind::current(),
                        target_os,
                        modifier_mapping,
                    );
                    self.remote_held.record_key(mapped_key, pressed);
                    self.net.send_high(
                        target,
                        Message::KeyEvent {
                            key: mapped_key,
                            pressed,
                            text,
                            modifiers: mapped_mods,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    fn can_route_to(&self, target: &DeviceId) -> bool {
        self.net.trust_store.is_trusted_and_enabled(target)
            && self.net.is_connected(target)
            && self.connected_peers.get(target).is_some_and(|peer| peer.capabilities.can_inject)
    }

    fn enter_remote_device(
        &mut self,
        target: DeviceId,
        os_x: i32,
        os_y: i32,
        canvas_x: f64,
        canvas_y: f64,
    ) {
        if !self.permissions.can_capture || !self.can_route_to(&target) {
            return;
        }
        let Some(ctrl) = &self.capture_ctrl else {
            return;
        };
        ctrl.set_grabbing(true, None);
        if !ctrl.is_grabbing() {
            self.emergency_cooldown_until = Some(Instant::now() + Duration::from_secs(1));
            self.notify("warning", "Could not capture local input. Control stayed on this computer.");
            return;
        }
        self.focused_device = target.clone();
        self.remote_routing = Some(PointerRoutingState::new(target.clone(), canvas_x, canvas_y));
        self.net
            .send_high(&target, Message::Enter { x: os_x, y: os_y });
        self.net.broadcast_high(&Message::FocusChange {
            epoch: self.controller_epoch,
            controller_id: self.local_id.clone(),
            target_device: target,
        });
    }

    fn focus_on_device(&mut self, target: DeviceId) {
        if target != self.local_id {
            if !self.permissions.can_capture {
                self.notify("warning", "This computer needs input capture access before it can send keyboard and mouse input.");
                return;
            }
            if !self.can_route_to(&target) {
                self.notify("warning", "The destination cannot receive input. Check its connection and input permissions.");
                return;
            }
        }
        if self.active_controller != self.local_id {
            self.switch_controller_to(self.local_id.clone());
        }
        if target == self.local_id {
            self.release_focus_to_local();
            return;
        }
        if self.focused_device != self.local_id && self.focused_device != target {
            let old = self.focused_device.clone();
            self.release_remote_held_on(&old);
            self.net.send_high(&old, Message::Leave);
        }
        let topo = self.build_topology();
        let (cx, cy, ox, oy) = if let Some(b) = topo.device_bounds(&target) {
            let cx = b.x as f64 + b.width as f64 / 2.0;
            let cy = b.y as f64 + b.height as f64 / 2.0;
            let (ox, oy) = topo.canvas_to_os(&target, cx, cy).unwrap_or((100, 100));
            (cx, cy, ox, oy)
        } else {
            (100.0, 100.0, 100, 100)
        };
        self.enter_remote_device(target, ox, oy, cx, cy);
    }

    fn cycle_focused_screen(&mut self) {
        let mut ring = vec![self.local_id.clone()];
        let mut peer_ids: Vec<DeviceId> = self
            .connected_peers
            .keys()
            .filter(|id| self.can_route_to(id))
            .cloned()
            .collect();
        peer_ids.sort();
        ring.extend(peer_ids);
        if ring.len() <= 1 {
            return;
        }
        let idx = ring
            .iter()
            .position(|id| id == &self.focused_device)
            .unwrap_or(0);
        let next = ring[(idx + 1) % ring.len()].clone();
        self.focus_on_device(next);
    }

    fn switch_controller_to(&mut self, new_controller: DeviceId) {
        if new_controller != self.local_id && !self.net.is_connected(&new_controller) {
            return;
        }
        if self.focused_device != self.local_id {
            let old = self.focused_device.clone();
            self.release_remote_held_on(&old);
            self.net.send_high(&old, Message::Leave);
        }
        self.release_local_grab();
        self.controller_epoch += 1;
        self.active_controller = new_controller.clone();
        self.focused_device = new_controller.clone();
        self.remote_routing = None;
        self.net.update_local_state(|s| {
            s.active_controller = Some((self.controller_epoch, new_controller.clone()));
        });
        self.net.broadcast_high(&Message::ClaimController {
            epoch: self.controller_epoch,
            controller_id: new_controller,
        });
    }

    fn release_focus_to_local(&mut self) {
        if self.focused_device != self.local_id {
            let old = self.focused_device.clone();
            self.release_remote_held_on(&old);
            self.net.send_high(&old, Message::Leave);
        }
        self.focused_device = self.local_id.clone();
        self.remote_routing = None;
        self.release_local_grab();
        self.net.broadcast_high(&Message::FocusChange {
            epoch: self.controller_epoch,
            controller_id: self.active_controller.clone(),
            target_device: self.local_id.clone(),
        });
    }

    fn release_local_grab(&self) {
        if let Some(ctrl) = &self.capture_ctrl {
            ctrl.set_grabbing(false, None);
        }
    }

    fn release_remote_held_on(&mut self, target: &DeviceId) {
        let (buttons, keys) = self.remote_held.drain_release_order();
        for b in buttons {
            self.net
                .send_high(target, Message::MouseButton { button: b, pressed: false });
        }
        for k in keys {
            self.net.send_high(
                target,
                Message::KeyEvent {
                    key: k,
                    pressed: false,
                    text: None,
                    modifiers: ModifierFlags::default(),
                },
            );
        }
        self.net.send_high(target, Message::ReleaseAllInput);
    }

    fn emergency_release(&mut self) {
        self.emergency_cooldown_until = Some(Instant::now() + Duration::from_millis(1000));
        self.local_resistance.reset();
        if self.focused_device != self.local_id {
            let old = self.focused_device.clone();
            self.release_remote_held_on(&old);
            self.net.send_high(&old, Message::Leave);
        }
        self.net.broadcast_high(&Message::ReleaseAllInput);
        if let Some(inj) = self.injector.as_mut() {
            let _ = inj.release_all();
        }
        self.focused_device = self.local_id.clone();
        self.remote_routing = None;
        self.release_local_grab();
    }

    fn poll_local_clipboard(&mut self, last_token: &mut Option<u64>, tick_count: u32) {
        let clip_cfg = self.config.lock().unwrap().clipboard.clone();
        if !clip_cfg.enabled || self.connected_peers.is_empty() {
            return;
        }
        let payload = {
            let mut backend = self.clipboard_backend.lock().unwrap();
            if let Some(tok) = backend.change_token() {
                if *last_token == Some(tok) {
                    return;
                }
                *last_token = Some(tok);
            } else if tick_count % 2 != 0 {
                // Poll every 500ms when OS doesn't provide a cheap change token
                return;
            }
            match backend.read_snapshot() {
                Ok(Some(p)) => p,
                _ => return,
            }
        };

        let change = self
            .clipboard_coord
            .lock()
            .unwrap()
            .on_local_change(payload, &clip_cfg);
        let Some((seq, hash, payload)) = change else {
            return;
        };

        match payload {
            ClipboardPayload::Text { plain, html, rtf } => {
                self.net.broadcast_normal(&Message::ClipboardText {
                    seq,
                    origin: self.local_id.clone(),
                    sha256: hash,
                    plain,
                    html,
                    rtf,
                });
            }
            ClipboardPayload::Image {
                png_bytes,
                width,
                height,
            } => {
                let transfer_id = self.clipboard_coord.lock().unwrap().alloc_transfer_id();
                let offer = Message::ClipboardImageOffer {
                    transfer_id,
                    seq,
                    origin: self.local_id.clone(),
                    sha256: hash,
                    total_bytes: png_bytes.len() as u64,
                    width,
                    height,
                };
                let chunks = build_chunks_for_bytes(transfer_id, &png_bytes);
                let peer_ids: Vec<DeviceId> = self.connected_peers.keys().cloned().collect();
                for pid in peer_ids {
                    self.net.send_normal(&pid, offer.clone());
                    let net = self.net.clone();
                    let chunks_cloned = chunks.clone();
                    tokio::spawn(async move {
                        for ch in chunks_cloned {
                            if net.send_data_chunk(&pid, ch).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
            ClipboardPayload::Files { paths } => {
                let prepared = self
                    .clipboard_coord
                    .lock()
                    .unwrap()
                    .prepare_files_offer(seq, &paths, &clip_cfg);
                let Ok((offer, sources)) = prepared else {
                    return;
                };
                let Message::ClipboardFilesOffer { transfer_id, .. } = &offer else {
                    return;
                };
                let tid = *transfer_id;
                let peer_ids: Vec<DeviceId> = self.connected_peers.keys().cloned().collect();
                for pid in peer_ids {
                    self.net.send_normal(&pid, offer.clone());
                    let net = self.net.clone();
                    let sources_cloned = sources.clone();
                    tokio::spawn(async move {
                        stream_files_to_peer(&net, &pid, tid, &sources_cloned).await;
                    });
                }
            }
        }
    }

    fn trigger_auto_connect(&mut self) {
        let auto = self.config.lock().unwrap().auto_reconnect;
        if !auto {
            return;
        }
        let trusted = self.net.trust_store.list();
        for tp in trusted {
            if !tp.enabled || self.net.is_connected(&tp.device_id) {
                continue;
            }
            // Deterministic dial initiator avoids simultaneous connect churn (though transport
            // also tie-breaks), OR dial if we have a reachable address and backoff elapsed.
            let should_wait = self
                .reconnect_backoff
                .get(&tp.device_id)
                .is_some_and(|(until, _)| Instant::now() < *until);
            if should_wait {
                continue;
            }
            // Prefer mDNS-discovered address over cached `last_addrs`.
            let addr = self
                .discovered_peers
                .iter()
                .find(|d| d.device_id == tp.device_id)
                .and_then(|d| d.addrs.first().copied())
                .or_else(|| tp.last_addrs.first().copied());

            let Some(addr) = addr else {
                continue;
            };

            let attempts = self
                .reconnect_backoff
                .get(&tp.device_id)
                .map(|(_, a)| *a + 1)
                .unwrap_or(1);
            let delay_secs = (1u64 << attempts.min(4)).min(10);
            self.reconnect_backoff
                .insert(tp.device_id.clone(), (Instant::now() + Duration::from_secs(delay_secs), attempts));

            let net = self.net.clone();
            let pid = tp.device_id.clone();
            tokio::spawn(async move {
                let _ = net.connect_trusted_peer(pid, addr).await;
            });
        }
    }

    fn shutdown(&mut self) {
        self.emergency_release();
        if let Some(ctrl) = &self.capture_ctrl {
            ctrl.stop();
        }
        self.net.disconnect_all("application shutting down");
        self.discovery.shutdown();
        self.clipboard_coord.lock().unwrap().clear_all_cache();
    }
}

async fn stream_files_to_peer(
    net: &NetworkManager,
    peer_id: &DeviceId,
    transfer_id: u64,
    sources: &[(u32, PathBuf)],
) {
    let total_files = sources.len();
    for (pos, (file_index, path)) in sources.iter().enumerate() {
        let is_last_file = pos + 1 == total_files;
        let Ok(mut f) = File::open(path) else {
            return;
        };
        let mut offset: u64 = 0;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let n = match f.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return,
            };
            if n == 0 {
                if offset == 0 && is_last_file {
                    let _ = net
                        .send_data_chunk(
                            peer_id,
                            BinaryChunk {
                                transfer_id,
                                file_index: *file_index,
                                offset: 0,
                                is_last: true,
                                data: Vec::new(),
                            },
                        )
                        .await;
                }
                break;
            }
            // Peek ahead by reading or detecting EOF via file len
            let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);
            let next_offset = offset + n as u64;
            let at_eof = next_offset >= file_len;
            let is_last = is_last_file && at_eof;
            if net
                .send_data_chunk(
                    peer_id,
                    BinaryChunk {
                        transfer_id,
                        file_index: *file_index,
                        offset,
                        is_last,
                        data: buf[..n].to_vec(),
                    },
                )
                .await
                .is_err()
            {
                return;
            }
            offset = next_offset;
        }
    }
}

struct SharedMemoryClipboardAdapter(Arc<Mutex<MemoryClipboard>>);

impl ClipboardBackend for SharedMemoryClipboardAdapter {
    fn change_token(&mut self) -> Option<u64> {
        self.0.lock().unwrap().change_token()
    }

    fn read_snapshot(&mut self) -> Result<Option<ClipboardPayload>> {
        self.0.lock().unwrap().read_snapshot()
    }

    fn write_snapshot(&mut self, payload: &ClipboardPayload) -> Result<()> {
        self.0.lock().unwrap().write_snapshot(payload)
    }
}
