import { useState } from "react";
import type { KeyboardEvent, ReactNode } from "react";
import { Clipboard, Fingerprint, Keyboard, MousePointer2, Network, ShieldCheck, Trash2 } from "lucide-react";
import { api } from "../api";
import type { AppConfig, ClusterSnapshot, ModifierMappingMode, Shortcut, ShortcutSettings } from "../types";
import { PageHeader, PanelHeading, RangeSetting, Switch, useActions } from "./controls";

const categories = [
  { id: "input", label: "Input & control", icon: MousePointer2 },
  { id: "clipboard", label: "Clipboard", icon: Clipboard },
  { id: "shortcuts", label: "Shortcuts", icon: Keyboard },
  { id: "connection", label: "Connection & security", icon: ShieldCheck },
] as const;

const shortcutFields = [
  ["cycle_screen", "Cycle active screen"],
  ["claim_controller", "Claim controller"],
  ["emergency_release", "Emergency release"],
  ["toggle_sharing", "Pause or resume sharing"],
] as const;

const shortcutCodes = new Set([
  "Escape", "Tab", "Space", "Enter", "Backspace", "Delete", "Insert", "Home", "End",
  "PageUp", "PageDown", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Pause", "ScrollLock",
  "Backquote", "Minus", "Equal", "BracketLeft", "BracketRight", "Backslash", "Semicolon",
  "Quote", "Comma", "Period", "Slash",
]);

function formatShortcut(shortcut: Shortcut) {
  return [
    shortcut.ctrl && "Ctrl",
    shortcut.alt && "Alt",
    shortcut.shift && "Shift",
    shortcut.meta && "Meta",
    shortcut.key.replace(/^(Key|Digit)/, ""),
  ].filter(Boolean);
}

function Setting({ title, description, children }: { title: string; description?: string; children: ReactNode }) {
  return (
    <div className="setting-row">
      <div className="setting-copy"><div className="setting-label">{title}</div>{description && <p className="muted">{description}</p>}</div>
      {children}
    </div>
  );
}

function ToggleSetting({ title, description, checked, onChange }: {
  title: string;
  description?: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return <Setting title={title} description={description}><Switch label={title} checked={checked} onChange={onChange} /></Setting>;
}

function LimitSetting({ title, value, max, onCommit }: {
  title: string;
  value: number;
  max: number;
  onCommit: (value: number) => Promise<unknown>;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  return (
    <Setting title={title}>
      <div className="row limit-control">
        <input className="input mono" type="number" aria-label={title} min={1} max={max} step={1}
          value={draft ?? value} onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); }}
          onBlur={(event) => {
            const next = event.target.valueAsNumber;
            if (Number.isFinite(next) && next !== value) void onCommit(Math.min(max, Math.max(1, Math.round(next))));
            setDraft(null);
          }} />
        <span className="muted">MiB</span>
      </div>
    </Setting>
  );
}

export function SettingsView({ snapshot }: { snapshot: ClusterSnapshot }) {
  const { config, local, peers } = snapshot;
  const { run } = useActions();
  const [category, setCategory] = useState<typeof categories[number]["id"]>("input");
  const [recording, setRecording] = useState<keyof ShortcutSettings | null>(null);

  const save = (patch: Partial<AppConfig>) => run(api.updateConfig(() => patch));
  const saveClipboard = (patch: Partial<AppConfig["clipboard"]>) => run(api.updateConfig((current) =>
    ({ clipboard: { ...current.clipboard, ...patch } })));

  function record(field: keyof ShortcutSettings, event: KeyboardEvent<HTMLButtonElement>) {
    event.preventDefault();
    event.stopPropagation();
    if (event.repeat || !(shortcutCodes.has(event.code) || /^(Key[A-Z]|Digit[0-9]|F([1-9]|1[0-9]|2[0-4]))$/.test(event.code))) return;
    const shortcut = { key: event.code, ctrl: event.ctrlKey, alt: event.altKey, shift: event.shiftKey, meta: event.metaKey };
    void run(api.updateConfig((current) => ({ shortcuts: { ...current.shortcuts, [field]: shortcut } })));
    setRecording(null);
  }

  return (
    <>
      <PageHeader title="Settings" description="Customize input, clipboard, shortcuts, and network access." />
      <div className="settings-layout">
        <nav className="settings-navigation card" aria-label="Settings categories">
          {categories.map(({ id, label, icon: Icon }) => (
            <button className="settings-nav-btn" key={id} aria-pressed={category === id}
              onClick={() => { setCategory(id); setRecording(null); }}><Icon /> {label}</button>
          ))}
        </nav>

        {category === "input" && (
          <section className="card settings-panel">
            <PanelHeading icon={<MousePointer2 />} title="Input & control" description="Choose which inputs can move between computers." />
            <div className="settings-rows">
              <ToggleSetting title="Keyboard sharing" description="Send keyboard input to the focused computer." checked={config.keyboard_sharing} onChange={(checked) => save({ keyboard_sharing: checked })} />
              <ToggleSetting title="Mouse sharing" description="Send pointer movement, clicks, and scrolling." checked={config.mouse_sharing} onChange={(checked) => save({ mouse_sharing: checked })} />
              <ToggleSetting title="Remote controller switching" description="Allow a paired computer to claim control." checked={config.allow_remote_claim_controller} onChange={(checked) => save({ allow_remote_claim_controller: checked })} />
              <Setting title="Preferred controller" description="Give this computer control when it connects.">
                <select className="select" value={config.preferred_controller ?? ""} aria-label="Preferred controller"
                  onChange={(event) => save({ preferred_controller: event.target.value || null })}>
                  <option value="">Any computer</option>
                  <option value={local.device_id}>{local.name} (this computer)</option>
                  {peers.map((peer) => <option value={peer.device_id} key={peer.device_id}>{peer.display_name}</option>)}
                  {config.preferred_controller && config.preferred_controller !== local.device_id && !peers.some((peer) => peer.device_id === config.preferred_controller)
                    && <option value={config.preferred_controller}>Unavailable computer</option>}
                </select>
              </Setting>
              <Setting title="Cross-OS modifier mapping" description="Choose how Command and Control map across operating systems.">
                <select className="select" value={config.modifier_mapping} aria-label="Cross-OS modifier mapping"
                  onChange={(event) => save({ modifier_mapping: event.target.value as ModifierMappingMode })}>
                  <option value="standard">Standard</option>
                  <option value="swap_cmd_ctrl_cross_os">Swap Cmd ↔ Ctrl</option>
                </select>
              </Setting>
              <RangeSetting title="Remote scroll speed" min={0.2} max={3} step={0.1} value={config.scroll_speed}
                format={(value) => `${value.toFixed(1)}×`} onCommit={(value) => save({ scroll_speed: value })} />
              <ToggleSetting title="Invert remote scrolling" checked={config.invert_remote_scroll} onChange={(checked) => save({ invert_remote_scroll: checked })} />
            </div>
          </section>
        )}

        {category === "clipboard" && (
          <section className="card settings-panel">
            <PanelHeading icon={<Clipboard />} title="Universal clipboard" description="Copy and paste between connected computers over your encrypted connection." />
            <div className="settings-rows">
              <ToggleSetting title="Clipboard sharing" checked={config.clipboard.enabled} onChange={(checked) => saveClipboard({ enabled: checked })} />
              <ToggleSetting title="Plain & multilingual text" checked={config.clipboard.sync_text} onChange={(checked) => saveClipboard({ sync_text: checked })} />
              <ToggleSetting title="Rich text & HTML" checked={config.clipboard.sync_rich_text} onChange={(checked) => saveClipboard({ sync_rich_text: checked })} />
              <ToggleSetting title="Images (PNG)" checked={config.clipboard.sync_images} onChange={(checked) => saveClipboard({ sync_images: checked })} />
              <ToggleSetting title="File copy & paste" checked={config.clipboard.sync_files} onChange={(checked) => saveClipboard({ sync_files: checked })} />
              <LimitSetting title="Text and image limit" max={512} value={config.clipboard.max_payload_mb} onCommit={(value) => saveClipboard({ max_payload_mb: value })} />
              <LimitSetting title="File bundle limit" max={4096} value={config.clipboard.max_file_mb} onCommit={(value) => saveClipboard({ max_file_mb: value })} />
              <Setting title="File transfer cache" description="Remove temporary copies staged for clipboard transfers.">
                <button className="btn btn-sm" onClick={() => run(api.clearClipboardCache())}><Trash2 /> Clear cache</button>
              </Setting>
            </div>
          </section>
        )}

        {category === "shortcuts" && (
          <section className="card settings-panel">
            <PanelHeading icon={<Keyboard />} title="Keyboard shortcuts" description="Select a shortcut, then press the new key combination." />
            <div className="settings-rows">
              {shortcutFields.map(([field, label]) => (
                <Setting key={field} title={label}>
                  {recording === field ? (
                    <div className="row">
                      <button key="recording" className="btn btn-sm btn-primary" onKeyDown={(event) => record(field, event)}
                        onBlur={() => setRecording(null)} onClick={() => setRecording(null)} autoFocus>Press a combination…</button>
                      <button className="btn btn-sm" onClick={() => setRecording(null)}>Cancel</button>
                    </div>
                  ) : (
                    <button key="shortcut" className="btn btn-sm shortcut" aria-label={`Record shortcut for ${label}: ${formatShortcut(config.shortcuts[field]).join(" + ")}`}
                      onClick={() => setRecording(field)}>
                      {formatShortcut(config.shortcuts[field]).map((key, index) => <kbd key={index}>{key}</kbd>)}
                    </button>
                  )}
                </Setting>
              ))}
            </div>
          </section>
        )}

        {category === "connection" && (
          <section className="card settings-panel">
            <PanelHeading icon={<Network />} title="Connection & security" description="Manage startup, network access, and operating system permissions." />
            <div className="settings-rows">
              <ToggleSetting title="Automatic reconnection" description="Reconnect after a network drop or when this computer wakes." checked={config.auto_reconnect} onChange={(checked) => save({ auto_reconnect: checked })} />
              <ToggleSetting title="Launch on startup" description="Start Move Freely automatically when you sign in." checked={config.launch_on_startup} onChange={(checked) => save({ launch_on_startup: checked })} />
              <ToggleSetting title="Local network only" description="Block connections to public IP addresses." checked={config.lan_only_ips} onChange={(checked) => save({ lan_only_ips: checked })} />
              <Setting title={`OS permissions · ${local.permissions.session_type}`} description={local.permissions.summary}>
                {local.os === "macos" && (
                  <div className="row permission-actions">
                    <button className="btn btn-sm" onClick={() => run(api.openPermissionSettings("accessibility"))}>Accessibility</button>
                    <button className="btn btn-sm" onClick={() => run(api.openPermissionSettings("input_monitoring"))}>Input Monitoring</button>
                  </div>
                )}
              </Setting>
              {local.permissions.action_hint && <p className="muted permission-hint">{local.permissions.action_hint}</p>}
              <div className="fingerprint">
                <div className="row setting-label"><Fingerprint /> TLS certificate fingerprint</div>
                <code className="mono">{local.device_id}</code>
              </div>
            </div>
          </section>
        )}
      </div>
    </>
  );
}
