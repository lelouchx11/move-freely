import { useCallback, useEffect, useRef, useState } from "react";
import { CircleAlert, CircleCheck, Info, LayoutDashboard, Link2, LoaderCircle, Monitor, MonitorCog, PanelsTopLeft, ShieldAlert, ShieldCheck, X } from "lucide-react";
import { api } from "./api";
import { ArrangementCanvas } from "./components/ArrangementCanvas";
import { DashboardView } from "./components/DashboardView";
import { DevicesView } from "./components/DevicesView";
import { PairForm } from "./components/PairForm";
import { SettingsView } from "./components/SettingsView";
import { Actions, deviceName, Dialog, osLabel, PinDisplay } from "./components/controls";
import type { ClusterSnapshot, DiscoveredPeer, PendingPairView } from "./types";

const navigation = [
  { id: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { id: "arrangement", label: "Arrangement", icon: PanelsTopLeft },
  { id: "devices", label: "Devices", icon: Monitor },
  { id: "settings", label: "Settings", icon: MonitorCog },
] as const;
type Tab = typeof navigation[number]["id"];

export function App() {
  const [tab, setTab] = useState<Tab>("dashboard");
  const [snapshot, setSnapshot] = useState<ClusterSnapshot | null>(null);
  const [startError, setStartError] = useState<string | null>(null);
  const [pairTarget, setPairTarget] = useState<DiscoveredPeer | null | undefined>(undefined);
  const [incomingReq, setIncomingReq] = useState<PendingPairView | null>(null);
  const [toast, setToast] = useState<{ level: string; message: string } | null>(null);
  const mainRef = useRef<HTMLElement>(null);
  const receivedSnapshot = useRef(false);

  const notify = useCallback((message: string, level = "info") => setToast({ level, message }), []);

  const run = useCallback(async (action: Promise<unknown>) => {
    try {
      await action;
      return true;
    } catch (error) {
      notify(String(error), "error");
      return false;
    }
  }, [notify]);

  const loadSnapshot = useCallback(async () => {
    setStartError(null);
    try {
      const current = await api.getSnapshot();
      setSnapshot((existing) => existing ?? current);
      if (!receivedSnapshot.current) setIncomingReq((existing) => existing ?? current.pending_pair_requests[0] ?? null);
    } catch (error) {
      setStartError(String(error));
    }
  }, []);

  useEffect(() => {
    let active = true;
    const subscriptions = [
      api.onSnapshot((next) => {
        if (!active) return;
        receivedSnapshot.current = true;
        setSnapshot(next);
        setIncomingReq((current) =>
          next.pending_pair_requests.find((request) => request.request_id === current?.request_id)
          ?? next.pending_pair_requests[0] ?? null);
      }),
      api.onIncomingPair((request) => { if (active) setIncomingReq((current) => current ?? request); }),
      api.onNotification(({ level, message }) => { if (active) notify(message, level); }),
      api.onOpenSettings(() => { if (active) setTab("settings"); }),
    ];
    void Promise.allSettled(subscriptions).then(() => { if (active) void loadSnapshot(); });
    return () => {
      active = false;
      subscriptions.forEach((subscription) => { void subscription.then((unlisten) => unlisten(), () => {}); });
    };
  }, [loadSnapshot, notify]);

  const pairingPin = snapshot?.pairing.pin;
  useEffect(() => {
    if (!pairingPin) return;
    let active = true;
    const timer = window.setInterval(() => {
      void api.getSnapshot().then(({ pairing }) => {
        if (active) setSnapshot((current) => current?.pairing.pin === pairingPin
          ? { ...current, pairing }
          : current);
      }, () => {});
    }, 1000);
    return () => { active = false; window.clearInterval(timer); };
  }, [pairingPin]);

  useEffect(() => { mainRef.current?.scrollTo(0, 0); }, [tab]);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), toast.level === "error" ? 7500 : 4500);
    return () => window.clearTimeout(timer);
  }, [toast]);

  if (!snapshot) {
    return (
      <div className="startup">
        <span className="brand-mark">MF</span>
        <h1>Move Freely</h1>
        {startError ? (
          <>
            <p>Couldn’t connect to the desktop service.</p>
            <p className="muted startup-detail">{startError}</p>
            <button className="btn btn-primary" onClick={loadSnapshot}>Try again</button>
          </>
        ) : <p className="row" role="status"><LoaderCircle className="spin" /> Starting the desktop service…</p>}
      </div>
    );
  }

  const connected = snapshot.peers.filter((peer) => peer.connected).length;
  const controller = deviceName(snapshot, snapshot.active_controller);
  const openPair = (peer?: DiscoveredPeer) => setPairTarget(peer ?? null);

  return (
    <Actions.Provider value={{ run, notify }}>
      <div className="app-shell">
        <aside className="sidebar">
          <div className="brand">
            <span className="brand-mark" aria-hidden="true">MF</span>
            <div className="brand-copy">
              <div className="brand-title">Move Freely</div>
              <div className="brand-sub mono" title={snapshot.local.device_id}>ID {snapshot.local.device_id.slice(0, 8)}</div>
            </div>
          </div>

          <nav className="nav-list" aria-label="Main navigation">
            {navigation.map(({ id, label, icon: Icon }) => {
              const count = id === "dashboard" ? connected : id === "devices" ? snapshot.discovered.length : null;
              return (
                <button key={id} className={`nav-btn ${tab === id ? "active" : ""}`} aria-current={tab === id ? "page" : undefined}
                  title={label} onClick={() => setTab(id)}>
                  <Icon aria-hidden="true" />
                  <span className="nav-label">{label}</span>
                  {count !== null && (id === "dashboard" || count > 0) && <span
                    className="nav-count" aria-label={`${count} ${id === "dashboard" ? "connected" : "nearby"}`}
                  >{count}</span>}
                </button>
              );
            })}
          </nav>

          <div className="sidebar-footer">
            <div className="sidebar-controller" title={`Active controller: ${controller}`}>
              <span className="sidebar-controller-icon"><Monitor aria-hidden="true" /><span className="status-dot" /></span>
              <div className="sidebar-controller-copy">
                <span>Active controller</span>
                <strong>{controller}</strong>
              </div>
            </div>
            <button className="btn sidebar-emergency" onClick={() => run(api.emergencyRelease())} title="Emergency release">
              <ShieldAlert aria-hidden="true" /><span>Emergency release</span>
            </button>
          </div>
        </aside>

        <main className="main-area" ref={mainRef} id="main-content">
          <div className="page-content" key={tab}>
            {tab === "dashboard" && <DashboardView snapshot={snapshot} onOpenPairModal={openPair} onOpenArrangement={() => setTab("arrangement")} />}
            {tab === "arrangement" && <ArrangementCanvas snapshot={snapshot} />}
            {tab === "devices" && <DevicesView snapshot={snapshot} onOpenPairModal={openPair} />}
            {tab === "settings" && <SettingsView snapshot={snapshot} />}
          </div>
        </main>

        {pairTarget !== undefined && (
          <PairComputer key={pairTarget?.device_id ?? "manual"} target={pairTarget} onClose={() => setPairTarget(undefined)} />
        )}
        {incomingReq && <IncomingPair key={incomingReq.request_id} request={incomingReq} />}

        {toast && (
          <div className={`toast ${toast.level === "error" ? "toast-error" : ""}`} role={toast.level === "error" ? "alert" : "status"}>
            {toast.level === "error" ? <CircleAlert /> : toast.level === "success" ? <CircleCheck /> : <Info />}
            <span>{toast.message}</span>
            <button className="btn btn-icon btn-quiet" aria-label="Dismiss notification" onClick={() => setToast(null)}><X /></button>
          </div>
        )}
      </div>
    </Actions.Provider>
  );
}

function PairComputer({ target, onClose }: { target: DiscoveredPeer | null; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  return (
    <Dialog title={target ? `Pair ${target.name}` : "Pair a computer"} icon={<Link2 />} onClose={busy ? undefined : onClose}>
      <p className="muted">On the other computer, open Devices, select Show pairing PIN, then enter the code here.</p>
      <PairForm target={target} onSuccess={onClose} onCancel={onClose} onBusy={setBusy} />
    </Dialog>
  );
}

function IncomingPair({ request }: { request: PendingPairView }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function respond(accept: boolean) {
    setBusy(true);
    setError(null);
    try {
      await api.respondPairRequest(request.request_id, accept);
    } catch (failure) {
      setError(String(failure));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Incoming pairing request" icon={<ShieldCheck />}>
      <p><strong>{request.name}</strong> wants to pair with this computer.</p>
      <p className="muted">{osLabel(request.os)} · <span className="mono">{request.addr}</span></p>
      <PinDisplay pin={request.pin} />
      <p className="muted">Check that this PIN matches on the other computer before accepting.</p>
      {error && <p className="form-error" role="alert">{error}</p>}
      <div className="row modal-actions">
        <button className="btn" disabled={busy} data-autofocus onClick={() => respond(false)}>Decline</button>
        <button className="btn btn-primary" disabled={busy} onClick={() => respond(true)}><ShieldCheck /> Accept & trust</button>
      </div>
    </Dialog>
  );
}
