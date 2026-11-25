import { invoke } from "@tauri-apps/api/core";
import { listen, UnlistenFn } from "@tauri-apps/api/event";
import type {
  AppConfig,
  BoundaryMode,
  ClusterSnapshot,
  ClusterTopology,
  DevicePlacement,
  PairingStatus,
  PendingPairView,
} from "./types";

let configUpdates = Promise.resolve();

export const api = {
  getSnapshot: () => invoke<ClusterSnapshot>("get_snapshot"),
  openPairingWindow: (customPin?: string) =>
    invoke<PairingStatus>("open_pairing_window", { customPin: customPin ?? null }),
  closePairingWindow: () => invoke<void>("close_pairing_window"),
  respondPairRequest: (requestId: number, accept: boolean) =>
    invoke<void>("respond_pair_request", { requestId, accept }),
  pairWithAddr: (addr: string, expectedId: string | null, pin: string) =>
    invoke<void>("pair_with_addr", { addr, expectedId, pin }),
  connectAddr: (peerId: string, addr: string) =>
    invoke<void>("connect_addr", { peerId, addr }),
  disconnectPeer: (deviceId: string) => invoke<void>("disconnect_peer", { deviceId }),
  renamePeer: (deviceId: string, alias: string | null) =>
    invoke<void>("rename_peer", { deviceId, alias }),
  setPeerEnabled: (deviceId: string, enabled: boolean) =>
    invoke<void>("set_peer_enabled", { deviceId, enabled }),
  revokePeer: (deviceId: string) => invoke<void>("revoke_peer", { deviceId }),
  setController: (deviceId: string) => invoke<void>("set_controller", { deviceId }),
  focusDevice: (deviceId: string) => invoke<void>("focus_device", { deviceId }),
  emergencyRelease: () => invoke<void>("emergency_release"),
  setSharingEnabled: (enabled: boolean) => invoke<void>("set_sharing_enabled", { enabled }),
  updatePlacements: (placements: DevicePlacement[], boundaryMode: BoundaryMode) =>
    invoke<void>("update_placements", { placements, boundaryMode }),
  snapPlacement: (
    topology: ClusterTopology,
    movingId: string,
    x: number,
    y: number,
    threshold: number
  ) => invoke<[number, number]>("snap_placement", { topology, movingId, x, y, threshold }),
  updateConfig: (patch: (current: AppConfig) => Partial<AppConfig>) => {
    const update = configUpdates.then(async () => {
      const { config } = await api.getSnapshot();
      await invoke<void>("update_config", { config: { ...config, ...patch(config) } });
    });
    configUpdates = update.catch(() => {});
    return update;
  },
  clearClipboardCache: () => invoke<void>("clear_clipboard_cache"),
  refreshMonitors: () => invoke<void>("refresh_monitors"),
  openPermissionSettings: (kind: string) => invoke<void>("open_permission_settings", { kind }),

  onOpenSettings: (cb: () => void): Promise<UnlistenFn> => listen("mf://open-settings", () => cb()),
  onSnapshot: (cb: (snap: ClusterSnapshot) => void): Promise<UnlistenFn> =>
    listen<ClusterSnapshot>("mf://snapshot", (e) => cb(e.payload)),
  onIncomingPair: (cb: (req: PendingPairView) => void): Promise<UnlistenFn> =>
    listen<PendingPairView>("mf://incoming-pair", (e) => cb(e.payload)),
  onNotification: (
    cb: (note: { level: string; message: string }) => void
  ): Promise<UnlistenFn> =>
    listen<{ level: string; message: string }>("mf://notification", (e) => cb(e.payload)),
};
