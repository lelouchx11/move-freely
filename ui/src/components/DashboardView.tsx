import { ArrowDownRight, ArrowRight, CircleAlert, Command, Crosshair, Link2, Monitor, MousePointer2, Pause, Play, Plus, ShieldAlert, Signal, Workflow } from "lucide-react";
import { api } from "../api";
import type { ClusterSnapshot, DiscoveredPeer } from "../types";
import { deviceName, osLabel, PageHeader, PanelHeading, Switch, useActions } from "./controls";
import { DiscoveredDevices } from "./DiscoveredDevices";

interface Props {
  snapshot: ClusterSnapshot;
  onOpenPairModal: (peer?: DiscoveredPeer) => void;
  onOpenArrangement: () => void;
}

export function DashboardView({ snapshot, onOpenPairModal, onOpenArrangement }: Props) {
  const { local, peers, discovered, sharing_enabled: sharing } = snapshot;
  const { run } = useActions();
  const controller = deviceName(snapshot, snapshot.active_controller);
  const destination = deviceName(snapshot, snapshot.focused_device);
  const sameComputer = snapshot.active_controller === snapshot.focused_device;
  const localInput = snapshot.focused_device === local.device_id;
  const permissionsReady = local.permissions.can_capture && local.permissions.can_inject;
  const connected = peers.filter((peer) => peer.connected).length;
  const controllerCandidates = peers.filter((peer) => (peer.connected && peer.enabled) || peer.device_id === snapshot.active_controller);

  return (
    <>
      <PageHeader title="Dashboard" description="See where your keyboard and mouse are routed.">
        <button className="btn" onClick={() => run(api.setSharingEnabled(!sharing))}>
          {sharing ? <Pause /> : <Play />}{sharing ? "Pause sharing" : "Resume sharing"}
        </button>
        <button className="btn btn-icon btn-danger-soft" onClick={() => run(api.emergencyRelease())}
          aria-label="Emergency release" title="Emergency release"><ShieldAlert /></button>
        <button className="btn btn-primary" onClick={() => onOpenPairModal()}><Plus /> Pair device</button>
      </PageHeader>

      {!permissionsReady && (
        <section className="permission-banner">
          <span className="icon-tile icon-warn"><CircleAlert /></span>
          <div>
            <h2>Input permissions required</h2>
            <p>{local.permissions.summary}</p>
            {local.permissions.action_hint && <p>{local.permissions.action_hint}</p>}
          </div>
          {local.os === "macos" && (
            <div className="row permission-actions">
              {!local.permissions.accessibility_granted && (
                <button className="btn btn-sm" onClick={() => run(api.openPermissionSettings("accessibility"))}>Open Accessibility</button>
              )}
              {!local.permissions.input_monitoring_granted && (
                <button className="btn btn-sm" onClick={() => run(api.openPermissionSettings("input_monitoring"))}>Open Input Monitoring</button>
              )}
            </div>
          )}
        </section>
      )}

      <section className="sharing-card" aria-labelledby="sharing-title">
        <div className="sharing-copy">
          <div className={`sharing-state ${sharing && permissionsReady ? "" : "sharing-limited"}`}>
            <span className="status-dot" />
            {!sharing ? "SHARING PAUSED" : permissionsReady ? "SHARING ACTIVE" : "PERMISSIONS REQUIRED"}
          </div>
          <h2 id="sharing-title">Input routing</h2>
          <p>{!sharing ? "Resume sharing to send keyboard and mouse input to another computer."
            : !permissionsReady ? "Grant system permissions to share keyboard and mouse input."
            : sameComputer ? "Keyboard and mouse input stays on the controller."
            : "Keyboard and mouse input is routed to the active destination."}</p>
          <button className="btn sharing-link" onClick={onOpenArrangement}>Screen arrangement <ArrowRight /></button>
        </div>
        <div className={`routing-visual ${sharing && permissionsReady ? "routing-live" : ""}`}>
          <div className="routing-node">
            <div className="routing-screen"><Command /></div>
            <span className="routing-role">CONTROLLER</span>
            <strong title={controller}>{controller}</strong>
          </div>
          <div className={`routing-path ${sameComputer ? "routing-local" : ""}`} aria-hidden="true"><ArrowRight /></div>
          <div className="routing-node routing-destination">
            <div className="routing-screen"><MousePointer2 /></div>
            <span className="routing-role">INPUT DESTINATION</span>
            <strong title={destination}>{destination}</strong>
          </div>
        </div>
      </section>

      <div className="grid-3 summary-grid">
        <section className="card summary-card">
          <div className="row summary-label"><span className="icon-tile"><Command /></span><h2>Active controller</h2></div>
          <div className="summary-value" title={controller}>{controller}</div>
          <p className="muted">Ownership epoch {snapshot.active_controller_epoch}</p>
          <select className="select summary-select" aria-label="Switch active controller" value={snapshot.active_controller}
            disabled={!controllerCandidates.length && snapshot.active_controller === local.device_id}
            onChange={(event) => run(api.setController(event.target.value))}
          >
            <option value={local.device_id}>{local.name} (this computer)</option>
            {controllerCandidates.map((peer) => <option value={peer.device_id} key={peer.device_id}>{peer.display_name}</option>)}
            {snapshot.active_controller !== local.device_id && !controllerCandidates.some((peer) => peer.device_id === snapshot.active_controller)
              && <option value={snapshot.active_controller}>{controller}</option>}
          </select>
        </section>
        <section className="card summary-card">
          <div className="row summary-label"><span className="icon-tile icon-teal"><MousePointer2 /></span><h2>Input destination</h2></div>
          <div className="summary-value" title={destination}>{destination}</div>
          <p className="muted">{localInput ? "This computer" : "Remote computer"}</p>
          <div className="row summary-status">
            <span className={`pill ${sharing && permissionsReady ? "pill-ok" : "pill-warn"}`}><span className="status-dot" />{!sharing ? "Paused" : permissionsReady ? "Sharing active" : "Permissions required"}</span>
            {sharing && permissionsReady && snapshot.is_grabbing && !localInput && <span className="pill pill-accent">Routing remotely</span>}
          </div>
        </section>
        <section className="card summary-card">
          <div className="row summary-label"><span className="icon-tile icon-sky"><Monitor /></span><h2>This computer</h2></div>
          <div className="summary-value" title={local.name}>{local.name}</div>
          <p className="muted">{osLabel(local.os)} · {local.arch} · {local.monitors.length} {local.monitors.length === 1 ? "display" : "displays"}</p>
          <p className="summary-address mono" title={`Port ${local.listen_port} · ${local.local_ips.join(", ")}`}>
            {local.local_ips.map((ip) => ip.includes(":") ? `[${ip}]:${local.listen_port}` : `${ip}:${local.listen_port}`).join(", ") || `Port ${local.listen_port}`}
          </p>
        </section>
      </div>

      <section className="card device-panel">
        <PanelHeading icon={<Workflow />} title="Paired devices">
          <span className={`pill ${connected ? "pill-ok" : "pill-neutral"}`}><span className="status-dot" />{connected} connected</span>
        </PanelHeading>
        {peers.length ? (
          <ul className="device-list">
            {peers.map((peer) => (
              <li className="device-row" key={peer.device_id}>
                <div className="row device-identity">
                  <span className="icon-tile device-icon"><Monitor /></span>
                  <div className="device-copy">
                    <div className="row device-name">
                      <strong>{peer.display_name}</strong>
                      <span className={`pill ${peer.connected ? "pill-ok" : "pill-neutral"}`}>
                        {peer.connected ? <><Signal /> {peer.latency_ms} ms</> : "Offline"}
                      </span>
                      {!peer.enabled && <span className="pill pill-warn">Disabled</span>}
                      {snapshot.active_controller === peer.device_id && <span className="pill pill-accent">Controller</span>}
                      {snapshot.focused_device === peer.device_id && <span className="pill pill-ok">Input here</span>}
                    </div>
                    <p className="muted">{[
                      osLabel(peer.os), peer.arch,
                      peer.monitors.map((monitor) => `${monitor.physical_width || monitor.width}×${monitor.physical_height || monitor.height}`).join(", ") || "No display details",
                      peer.addr,
                    ].filter(Boolean).join(" · ")}</p>
                  </div>
                </div>
                <div className="row device-actions">
                  {peer.connected && peer.enabled && <>
                    <button className="btn btn-sm" disabled={!sharing || snapshot.focused_device === peer.device_id} onClick={() => run(api.focusDevice(peer.device_id))}><Crosshair /> Focus here</button>
                    <button className="btn btn-sm" disabled={snapshot.active_controller === peer.device_id} onClick={() => run(api.setController(peer.device_id))}>Make controller</button>
                    <button className="btn btn-sm btn-quiet" onClick={() => run(api.disconnectPeer(peer.device_id))}>Disconnect</button>
                  </>}
                  <Switch label={`Enable ${peer.display_name}`} checked={peer.enabled} onChange={(enabled) => run(api.setPeerEnabled(peer.device_id, enabled))} />
                </div>
              </li>
            ))}
          </ul>
        ) : (
          <div className="paired-empty">
            <span className="icon-tile device-icon"><Link2 /></span>
            <div><strong>No paired devices yet</strong><p className="muted">Pair by IP address or choose a computer from the nearby list.</p></div>
            <ArrowDownRight className="empty-arrow" aria-hidden="true" />
          </div>
        )}
      </section>

      <DiscoveredDevices peers={discovered} onPair={onOpenPairModal} />
    </>
  );
}
