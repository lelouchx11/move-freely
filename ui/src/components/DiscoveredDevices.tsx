import { Laptop, Radar } from "lucide-react";
import type { DiscoveredPeer } from "../types";
import { osLabel, PanelHeading } from "./controls";

export function DiscoveredDevices({ peers, onPair }: {
  peers: DiscoveredPeer[];
  onPair: (peer: DiscoveredPeer) => void;
}) {
  return (
    <section className="card device-panel">
      <PanelHeading
        icon={<Radar />}
        title="Nearby devices"
        description="Computers running Move Freely on your local network."
      >
        <span className="pill pill-neutral"><span className="status-dot" />{peers.length ? `${peers.length} found` : "Scanning"}</span>
      </PanelHeading>
      {peers.length ? (
        <ul className="device-list">
          {peers.map((peer) => (
            <li key={peer.device_id} className="device-row">
              <div className="row device-identity">
                <span className="icon-tile device-icon"><Laptop /></span>
                <div className="device-copy">
                  <div className="row device-name">
                    <strong>{peer.name}</strong>
                    {peer.pairing_open && <span className="pill pill-ok">PIN ready</span>}
                  </div>
                  <p className="muted">{[osLabel(peer.os), peer.arch, ...peer.addrs].join(" · ")}</p>
                </div>
              </div>
              <button className="btn btn-sm" onClick={() => onPair(peer)} aria-label={`Pair ${peer.name}`}>Pair</button>
            </li>
          ))}
        </ul>
      ) : (
        <div className="inline-empty">No nearby computers found yet. Keep both computers on the same Wi-Fi or Ethernet network.</div>
      )}
    </section>
  );
}
