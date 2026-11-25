import { useState } from "react";
import type { FormEvent } from "react";
import { Link2, LoaderCircle } from "lucide-react";
import { api } from "../api";
import type { DiscoveredPeer } from "../types";
import { socketAddress, useActions } from "./controls";

export function PairForm({ target, onSuccess, onCancel, onBusy }: {
  target?: DiscoveredPeer | null;
  onSuccess?: () => void;
  onCancel?: () => void;
  onBusy?: (busy: boolean) => void;
}) {
  const [address, setAddress] = useState(target?.addrs[0] ?? "");
  const [pin, setPin] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { notify } = useActions();

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!address.trim() || pin.length !== 6 || busy) return;
    setBusy(true);
    onBusy?.(true);
    setError(null);
    try {
      await api.pairWithAddr(socketAddress(address), target?.device_id ?? null, pin);
      setAddress("");
      setPin("");
      onSuccess?.();
      notify(target ? `Paired with ${target.name}.` : "Computer paired successfully.", "success");
    } catch (failure) {
      setError(String(failure));
    } finally {
      onBusy?.(false);
      setBusy(false);
    }
  }

  return (
    <form className="form-stack" onSubmit={submit}>
      <div className={`pair-fields ${onCancel ? "" : "pair-fields-inline"}`}>
        <label className="field">
          IP address and port
          <input className="input mono" value={address} onChange={(event) => setAddress(event.target.value)}
            placeholder="192.168.1.42:24800" autoComplete="off" spellCheck={false}
            data-autofocus={onCancel && !target?.addrs.length ? "" : undefined} required />
        </label>
        <label className="field">
          Pairing PIN
          <input className="input pin-input mono" value={pin} onChange={(event) => setPin(event.target.value.replace(/\D/g, "").slice(0, 6))}
            placeholder="000000" inputMode="numeric" autoComplete="one-time-code"
            data-autofocus={onCancel && target?.addrs.length ? "" : undefined} required />
        </label>
      </div>
      {busy && <p className="muted" role="status">Accept the pairing request on the other computer if prompted.</p>}
      {error && <p className="form-error" role="alert">{error}</p>}
      <div className={`row ${onCancel ? "modal-actions" : "form-buttons"}`}>
        {!onCancel && <span className="muted">Get the PIN on the other computer.</span>}
        {onCancel && <button type="button" className="btn" disabled={busy} onClick={onCancel}>Cancel</button>}
        <button type="submit" className="btn btn-primary" disabled={busy || !address.trim() || pin.length !== 6}>
          {busy ? <LoaderCircle className="spin" /> : <Link2 />}{busy ? "Pairing…" : "Connect & pair"}
        </button>
      </div>
    </form>
  );
}
