import { useState } from "react";
import type { FormEvent } from "react";
import { Clock3, KeyRound, Link2, Monitor, ShieldCheck, Trash2, X } from "lucide-react";
import { api } from "../api";
import type { ClusterSnapshot, DiscoveredPeer, PeerView } from "../types";
import { Dialog, osLabel, PageHeader, PanelHeading, PinDisplay, socketAddress, Switch, useActions } from "./controls";
import { PairForm } from "./PairForm";
import { DiscoveredDevices } from "./DiscoveredDevices";

interface Props {
  snapshot: ClusterSnapshot;
  onOpenPairModal: (peer?: DiscoveredPeer) => void;
}

export function DevicesView({ snapshot, onOpenPairModal }: Props) {
  const { local, pairing, peers } = snapshot;
  const { run } = useActions();
  const [localName, setLocalName] = useState<string | null>(null);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [alias, setAlias] = useState("");
  const [remove, setRemove] = useState<PeerView | null>(null);
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);
  const [pinBusy, setPinBusy] = useState(false);
  const [reconnecting, setReconnecting] = useState<string[]>([]);
  const remaining = `${Math.floor(pairing.expires_in_secs / 60)}:${String(pairing.expires_in_secs % 60).padStart(2, "0")}`;

  async function togglePin() {
    setPinBusy(true);
    await run(pairing.active ? api.closePairingWindow() : api.openPairingWindow());
    setPinBusy(false);
  }

  async function saveName(event: FormEvent) {
    event.preventDefault();
    if (localName?.trim() && await run(api.renamePeer(local.device_id, localName.trim()))) setLocalName(null);
  }

  async function saveAlias(event: FormEvent, id: string) {
    event.preventDefault();
    if (await run(api.renamePeer(id, alias.trim() || null))) setEditingId(null);
  }

  async function reconnect(peer: PeerView) {
    if (!peer.addr) return;
    setReconnecting((current) => [...current, peer.device_id]);
    await run(api.connectAddr(peer.device_id, peer.addr));
    setReconnecting((current) => current.filter((id) => id !== peer.device_id));
  }

  async function confirmRemoval() {
    if (!remove) return;
    setRemoving(true);
    setRemoveError(null);
    try {
      await api.revokePeer(remove.device_id);
      setRemove(null);
    } catch (error) {
      setRemoveError(String(error));
    } finally {
      setRemoving(false);
    }
  }

  return (
    <>
      <PageHeader title="Devices" description="Pair computers and manage which ones you trust.">
        <button className={`btn ${pairing.active ? "" : "btn-primary"}`} disabled={pinBusy} onClick={togglePin}>
          {pairing.active ? <X /> : <KeyRound />}{pairing.active ? "Close pairing PIN" : "Show pairing PIN"}
        </button>
      </PageHeader>

      {pairing.active && pairing.pin && (
        <section className="card pairing-card">
          <div>
            <div className="row"><span className="icon-tile"><KeyRound /></span><h2>Pairing PIN</h2></div>
            <p>Enter this code in Move Freely on the other computer.</p>
            <p className="muted mono">{socketAddress(local.local_ips[0] ?? "127.0.0.1", local.listen_port)}</p>
          </div>
          <div>
            <PinDisplay pin={pairing.pin} />
            <div className="row pin-countdown"><Clock3 /> Expires in <span className="mono">{remaining}</span></div>
          </div>
        </section>
      )}

      <div className="grid-2">
        <section className="card form-panel">
          <PanelHeading icon={<Monitor />} title="This computer" description="Your name is visible to other computers on the network." />
          <form className="form-stack" onSubmit={saveName}>
            <label className="field">Computer name
              <input className="input" value={localName ?? local.name} onChange={(event) => setLocalName(event.target.value)} required />
            </label>
            <div className="row form-buttons">
              <span className="muted mono" title={local.device_id}>ID {local.device_id.slice(0, 12)}</span>
              <button className="btn" type="submit" disabled={!localName?.trim() || localName.trim() === local.name}>Save name</button>
            </div>
          </form>
        </section>

        <section className="card form-panel">
          <PanelHeading icon={<Link2 />} title="Pair by IP address" description="Use the other computer’s network address when it isn’t listed nearby." />
          <PairForm />
        </section>
      </div>

      <section className="card device-panel">
        <PanelHeading icon={<ShieldCheck />} title="Trusted devices">
          <span className="pill pill-neutral">{peers.length} trusted</span>
        </PanelHeading>
        {peers.length ? (
          <ul className="device-list">
            {peers.map((peer) => (
              <li className="device-row" key={peer.device_id}>
                <div className="row device-identity">
                  <span className="icon-tile device-icon"><Monitor /></span>
                  {editingId === peer.device_id ? (
                    <form className="row rename-form" onSubmit={(event) => saveAlias(event, peer.device_id)}>
                      <input className="input" aria-label={`Alias for ${peer.name}`} value={alias}
                        onChange={(event) => setAlias(event.target.value)} autoFocus />
                      <button className="btn btn-primary btn-sm" type="submit">Save</button>
                      <button className="btn btn-sm" type="button" onClick={() => setEditingId(null)}>Cancel</button>
                    </form>
                  ) : (
                    <div className="device-copy">
                      <div className="row device-name">
                        <strong>{peer.display_name}</strong>
                        <span className={`pill ${peer.connected ? "pill-ok" : "pill-neutral"}`}><span className="status-dot" />
                          {peer.connected ? `Online · ${peer.latency_ms} ms` : "Offline"}
                        </span>
                        {!peer.enabled && <span className="pill pill-warn">Disabled</span>}
                        {snapshot.active_controller === peer.device_id && <span className="pill pill-accent">Controller</span>}
                      </div>
                      <p className="muted">{[osLabel(peer.os), peer.addr, `ID ${peer.device_id.slice(0, 12)}`].filter(Boolean).join(" · ")}</p>
                    </div>
                  )}
                </div>
                {editingId !== peer.device_id && (
                  <div className="row device-actions">
                    {!peer.connected && peer.enabled && peer.addr && (
                      <button className="btn btn-sm" disabled={reconnecting.includes(peer.device_id)} onClick={() => reconnect(peer)}>
                        {reconnecting.includes(peer.device_id) ? "Reconnecting…" : "Reconnect"}
                      </button>
                    )}
                    <button className="btn btn-sm" onClick={() => { setAlias(peer.display_name); setEditingId(peer.device_id); }}>Rename</button>
                    <button className="btn btn-sm" disabled={!peer.connected || !peer.enabled || snapshot.active_controller === peer.device_id}
                      onClick={() => run(api.setController(peer.device_id))}
                    >Set controller</button>
                    <Switch label={`Enable ${peer.display_name}`} checked={peer.enabled} onChange={(enabled) => run(api.setPeerEnabled(peer.device_id, enabled))} />
                    <button className="btn btn-icon btn-danger-soft" aria-label={`Remove ${peer.display_name}`} title="Remove trusted device"
                      onClick={() => { setRemoveError(null); setRemove(peer); }}><Trash2 /></button>
                  </div>
                )}
              </li>
            ))}
          </ul>
        ) : <div className="inline-empty">Computers you pair with will appear here.</div>}
      </section>

      <DiscoveredDevices peers={snapshot.discovered} onPair={onOpenPairModal} />

      {remove && (
        <Dialog title={`Remove ${remove.display_name}?`} icon={<Trash2 />} onClose={removing ? undefined : () => setRemove(null)}>
          <p className="muted">This disconnects the device and removes its certificate from your trusted list. You’ll need to pair again to reconnect.</p>
          {removeError && <p className="form-error" role="alert">{removeError}</p>}
          <div className="row modal-actions">
            <button className="btn" data-autofocus disabled={removing} onClick={() => setRemove(null)}>Cancel</button>
            <button className="btn btn-danger" disabled={removing} onClick={confirmRemoval}>Remove device</button>
          </div>
        </Dialog>
      )}
    </>
  );
}
