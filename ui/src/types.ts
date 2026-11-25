export type OsKind = "macos" | "windows" | "linux" | "other";
export type ArchKind = "aarch64" | "x86_64" | "other";
export type BoundaryMode = "overlapping" | "proportional";
export type ModifierMappingMode = "standard" | "swap_cmd_ctrl_cross_os";

export interface Monitor {
  id: string;
  name: string;
  x: number;
  y: number;
  width: number;
  height: number;
  physical_width: number;
  physical_height: number;
  scale_factor: number;
  is_primary: boolean;
}

export interface EdgeMask {
  left: boolean;
  right: boolean;
  top: boolean;
  bottom: boolean;
}

export interface DevicePlacement {
  device_id: string;
  x: number;
  y: number;
  enabled_edges: EdgeMask;
}

export interface DeviceTopology {
  device_id: string;
  name: string;
  monitors: Monitor[];
  placement: DevicePlacement;
  enabled: boolean;
}

export interface ClusterTopology {
  devices: DeviceTopology[];
  boundary_mode: BoundaryMode;
}

export interface PermissionStatus {
  accessibility_granted: boolean;
  input_monitoring_granted: boolean;
  can_capture: boolean;
  can_inject: boolean;
  session_type: string;
  summary: string;
  action_hint: string | null;
}

export interface PeerCapabilities {
  can_capture: boolean;
  can_inject: boolean;
  clipboard_text: boolean;
  clipboard_image: boolean;
  clipboard_files: boolean;
  wayland_session: boolean;
  status_note: string | null;
}

export interface PeerView {
  device_id: string;
  name: string;
  display_name: string;
  os: OsKind;
  arch: ArchKind;
  enabled: boolean;
  connected: boolean;
  latency_ms: number;
  addr: string | null;
  monitors: Monitor[];
  capabilities: PeerCapabilities;
}

export interface DiscoveredPeer {
  device_id: string;
  name: string;
  os: OsKind;
  arch: ArchKind;
  port: number;
  addrs: string[];
  pairing_open: boolean;
  protocol_version: number;
}

export interface PairingStatus {
  active: boolean;
  pin: string | null;
  expires_in_secs: number;
}

export interface PendingPairView {
  request_id: number;
  device_id: string;
  name: string;
  os: OsKind;
  addr: string;
  pin: string;
}

export interface Shortcut {
  key: string;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
  meta: boolean;
}

export interface ShortcutSettings {
  cycle_screen: Shortcut;
  claim_controller: Shortcut;
  emergency_release: Shortcut;
  toggle_sharing: Shortcut;
}

export interface EdgeSensitivity {
  pointer_sensitivity: number;
  edge_resistance_ms: number;
  edge_barrier_px: number;
  corner_deadzone_px: number;
  adjacency_tolerance_px: number;
}

export interface ClipboardSettings {
  enabled: boolean;
  sync_text: boolean;
  sync_rich_text: boolean;
  sync_images: boolean;
  sync_files: boolean;
  max_payload_mb: number;
  max_file_mb: number;
}

export interface AppConfig {
  device_name: string;
  bind_ip: string;
  listen_port: number;
  lan_only_ips: boolean;
  allow_routable_ips: boolean;
  sharing_enabled: boolean;
  keyboard_sharing: boolean;
  mouse_sharing: boolean;
  auto_reconnect: boolean;
  launch_on_startup: boolean;
  allow_remote_claim_controller: boolean;
  preferred_controller: string | null;
  scroll_speed: number;
  invert_remote_scroll: boolean;
  modifier_mapping: ModifierMappingMode;
  boundary_mode: BoundaryMode;
  sensitivity: EdgeSensitivity;
  clipboard: ClipboardSettings;
  shortcuts: ShortcutSettings;
  placements: DevicePlacement[];
}

export interface LocalDeviceView {
  device_id: string;
  name: string;
  os: OsKind;
  arch: ArchKind;
  listen_port: number;
  local_ips: string[];
  monitors: Monitor[];
  permissions: PermissionStatus;
}

export type DisplayRect = Pick<Monitor, "x" | "y" | "width" | "height">;

export interface ClusterSnapshot {
  local: LocalDeviceView;
  sharing_enabled: boolean;
  active_controller_epoch: number;
  active_controller: string;
  focused_device: string;
  is_grabbing: boolean;
  peers: PeerView[];
  discovered: DiscoveredPeer[];
  pairing: PairingStatus;
  pending_pair_requests: PendingPairView[];
  topology: ClusterTopology;
  monitor_layouts: Record<string, DisplayRect[]>;
  config: AppConfig;
}
