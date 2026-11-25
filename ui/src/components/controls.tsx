import { createContext, useContext, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { X } from "lucide-react";
import type { ClusterSnapshot, OsKind } from "../types";

export const Actions = createContext({
  run: async (action: Promise<unknown>) => { await action; return true; },
  notify: (_message: string, _level?: string) => {},
});

export function useActions() {
  return useContext(Actions);
}

export function osLabel(os: OsKind) {
  return { macos: "macOS", windows: "Windows", linux: "Linux", other: "Other OS" }[os];
}

export function deviceName(snapshot: ClusterSnapshot, id: string) {
  return id === snapshot.local.device_id
    ? snapshot.local.name
    : (snapshot.peers.find((peer) => peer.device_id === id)?.display_name ?? id.slice(0, 8));
}

export function socketAddress(input: string, port = 24800) {
  const address = input.trim();
  if (address.endsWith("]")) return `${address}:${port}`;
  if (address.startsWith("[")) return address;
  const separators = address.split(":").length - 1;
  if (separators > 1) return `[${address}]:${port}`;
  return separators === 1 ? address : `${address}:${port}`;
}

export function Switch({ label, checked, onChange }: {
  label: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <label className="switch">
      <input type="checkbox" role="switch" aria-label={label} checked={checked}
        onChange={(event) => onChange(event.target.checked)} />
      <span className="slider" />
    </label>
  );
}

export function PageHeader({ title, description, children }: {
  title: string;
  description: string;
  children?: ReactNode;
}) {
  return (
    <header className="page-header">
      <div>
        <h1>{title}</h1>
        <p>{description}</p>
      </div>
      {children && <div className="row page-actions">{children}</div>}
    </header>
  );
}

export function PanelHeading({ icon, title, description, children }: {
  icon: ReactNode;
  title: string;
  description?: string;
  children?: ReactNode;
}) {
  return (
    <div className="panel-heading">
      <div className="row panel-title">
        <span className="icon-tile">{icon}</span>
        <div>
          <h2>{title}</h2>
          {description && <p>{description}</p>}
        </div>
      </div>
      {children && <div className="row">{children}</div>}
    </div>
  );
}

export function Dialog({ title, icon, children, onClose }: {
  title: string;
  icon: ReactNode;
  children: ReactNode;
  onClose?: () => void;
}) {
  const titleId = useId();
  const dialog = useRef<HTMLDialogElement>(null);

  useLayoutEffect(() => {
    const element = dialog.current;
    if (!element) return;
    const opener = document.activeElement;
    element.showModal();
    element.querySelector<HTMLElement>("[data-autofocus]")?.focus();
    return () => {
      element.close();
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus();
    };
  }, []);

  return (
    <dialog
      ref={dialog}
      className="modal-card"
      aria-labelledby={titleId}
      onCancel={(event) => { event.preventDefault(); onClose?.(); }}
      onClick={(event) => { if (event.target === event.currentTarget) onClose?.(); }}
    >
      <div className="modal-content">
        <div className="row-between">
          <span className="icon-tile">{icon}</span>
          {onClose && <button className="btn btn-icon btn-quiet" aria-label="Close dialog" onClick={onClose}><X /></button>}
        </div>
        <h2 id={titleId}>{title}</h2>
        {children}
      </div>
    </dialog>
  );
}

export function PinDisplay({ pin }: { pin: string }) {
  return (
    <div className="pin-display" role="group" aria-label={`PIN ${pin.split("").join(" ")}`}>
      {pin.split("").map((digit, index) => <span aria-hidden="true" key={index}>{digit}</span>)}
    </div>
  );
}

export function RangeSetting({ title, description, value, min, max, step, format, onCommit }: {
  title: string;
  description?: string;
  value: number;
  min: number;
  max: number;
  step: number;
  format: (value: number) => string;
  onCommit: (value: number) => Promise<unknown>;
}) {
  const [draft, setDraft] = useState<number | null>(null);
  const pending = useRef<number | null>(null);
  const timer = useRef<number | null>(null);
  const inputId = useId();
  const shown = draft ?? value;

  function commit() {
    if (timer.current !== null) window.clearTimeout(timer.current);
    const next = pending.current;
    if (next === null) return;
    pending.current = null;
    void onCommit(next).then(() => setDraft((current) => current === next ? null : current));
  }

  useEffect(() => () => { if (timer.current !== null) window.clearTimeout(timer.current); }, []);

  return (
    <div className="setting-row">
      <div className="setting-copy">
        <label className="setting-label" htmlFor={inputId}>{title}</label>
        {description && <p className="muted">{description}</p>}
      </div>
      <div className="range-control">
        <input id={inputId} type="range" min={min} max={max} step={step} value={shown}
          aria-valuetext={format(shown)} onBlur={commit} onPointerUp={commit}
          onChange={(event) => {
            const next = event.target.valueAsNumber;
            pending.current = next;
            setDraft(next);
            if (timer.current !== null) window.clearTimeout(timer.current);
            timer.current = window.setTimeout(commit, 140);
          }} />
        <output className="value-chip mono" htmlFor={inputId}>{format(shown)}</output>
      </div>
    </div>
  );
}
