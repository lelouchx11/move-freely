import { useLayoutEffect, useRef, useState } from "react";
import type { KeyboardEvent, PointerEvent } from "react";
import { ArrowLeftRight, Grip, LayoutGrid, Monitor, RefreshCw, Scan, SlidersHorizontal } from "lucide-react";
import { api } from "../api";
import type { AppConfig, BoundaryMode, ClusterSnapshot, DevicePlacement, DeviceTopology, DisplayRect, EdgeMask } from "../types";
import { PageHeader, PanelHeading, RangeSetting, Switch, useActions } from "./controls";

type View = { scale: number; x: number; y: number };

interface ScreenGroup {
  device: DeviceTopology;
  displays: DisplayRect[];
  width: number;
  height: number;
}

interface Drag {
  pointer: number;
  startX: number;
  startY: number;
  group: ScreenGroup;
  view: View;
  revision: number;
}

const edges: (keyof EdgeMask)[] = ["left", "right", "top", "bottom"];
const keyboardDirections: Record<string, [number, number]> = {
  ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1],
};

function fitToCanvas(groups: ScreenGroup[], width: number, height: number): View {
  const visible = groups.filter((group) => group.displays.length);
  if (!visible.length) return { scale: 1, x: 0, y: 0 };
  const minX = Math.min(...visible.map(({ device }) => device.placement.x));
  const minY = Math.min(...visible.map(({ device }) => device.placement.y));
  const maxX = Math.max(...visible.map(({ device, width }) => device.placement.x + width));
  const maxY = Math.max(...visible.map(({ device, height }) => device.placement.y + height));
  const scale = Math.min(Math.max(1, width - 72) / (maxX - minX), Math.max(1, height - 64) / (maxY - minY), 0.25);
  return { scale, x: (width - (maxX + minX) * scale) / 2, y: (height - (maxY + minY) * scale) / 2 };
}

function edgeGuide(groups: ScreenGroup[], drag: Drag, axis: "x" | "y", snapped: number, raw: number) {
  if (raw === snapped) return undefined;
  const size = axis === "x" ? "width" : "height";
  const end = snapped + drag.group[size];
  const hasEndMatch = groups.some((group) => {
    const start = group.device.placement[axis];
    return group.device.device_id !== drag.group.device.device_id
      && group.displays.length && (start === end || start + group[size] === end);
  });
  return hasEndMatch ? end : snapped;
}

export function ArrangementCanvas({ snapshot }: { snapshot: ClusterSnapshot }) {
  const { topology, config } = snapshot;
  const { run } = useActions();
  const canvasRef = useRef<HTMLDivElement>(null);
  const drag = useRef<Drag | null>(null);
  const [canvasSize, setCanvasSize] = useState({ width: 0, height: 0 });
  const [selectedId, setSelectedId] = useState(snapshot.local.device_id);
  const [preview, setPreview] = useState<{ placement: DevicePlacement; view: View; guideX?: number; guideY?: number } | null>(null);
  const [savingMove, setSavingMove] = useState(false);
  const groups: ScreenGroup[] = topology.devices.map((device) => {
    const displays = snapshot.monitor_layouts[device.device_id] ?? [];
    return {
      device,
      displays,
      width: Math.max(0, ...displays.map((display) => display.x + display.width)),
      height: Math.max(0, ...displays.map((display) => display.y + display.height)),
    };
  });
  const fitted = fitToCanvas(groups, canvasSize.width, canvasSize.height);
  const view = preview?.view ?? fitted;
  const selected = groups.find(({ device }) => device.device_id === selectedId) ?? groups[0];

  useLayoutEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const measure = () => setCanvasSize({ width: canvas.clientWidth, height: canvas.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(canvas);
    return () => { observer.disconnect(); drag.current = null; };
  }, []);

  function beginDrag(event: PointerEvent<HTMLButtonElement>, group: ScreenGroup) {
    if (event.button !== 0 || drag.current || savingMove) return;
    setSelectedId(group.device.device_id);
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { pointer: event.pointerId, startX: event.clientX, startY: event.clientY, group, view: fitted, revision: 0 };
  }

  function propose(event: PointerEvent<HTMLButtonElement>, move: Drag) {
    const dx = event.clientX - move.startX;
    const dy = event.clientY - move.startY;
    const { x, y } = move.group.device.placement;
    return { x: Math.round(x + dx / move.view.scale), y: Math.round(y + dy / move.view.scale), moved: Math.hypot(dx, dy) >= 3 };
  }

  function showPreview(move: Drag, x: number, y: number, rawX: number, rawY: number) {
    setPreview({
      placement: { ...move.group.device.placement, x, y }, view: move.view,
      guideX: edgeGuide(groups, move, "x", x, rawX), guideY: edgeGuide(groups, move, "y", y, rawY),
    });
  }

  function moveDrag(event: PointerEvent<HTMLButtonElement>) {
    const current = drag.current;
    if (!current || current.pointer !== event.pointerId) return;
    const { x, y, moved } = propose(event, current);
    const revision = ++current.revision;
    if (!moved) { setPreview(null); return; }
    showPreview(current, x, y, x, y);
    void api.snapPlacement(topology, current.group.device.device_id, x, y, 65).then(([sx, sy]) => {
      if (drag.current === current && current.revision === revision) showPreview(current, sx, sy, x, y);
    }, () => {});
  }

  function writePlacement(id: string, patch: Partial<DevicePlacement>) {
    return run(api.updatePlacements(topology.devices.map(({ placement }) =>
      placement.device_id === id ? { ...placement, ...patch } : placement), topology.boundary_mode));
  }

  async function finishDrag(event: PointerEvent<HTMLButtonElement>) {
    const current = drag.current;
    if (!current || current.pointer !== event.pointerId) return;
    drag.current = null;
    const { x, y, moved } = propose(event, current);
    if (!moved) { setPreview(null); return; }
    setSavingMove(true);
    try {
      const [sx, sy] = await api.snapPlacement(topology, current.group.device.device_id, x, y, 65);
      showPreview(current, sx, sy, x, y);
      await writePlacement(current.group.device.device_id, { x: sx, y: sy });
    } catch (error) {
      await run(Promise.reject(error));
    } finally {
      setPreview(null);
      setSavingMove(false);
    }
  }

  function cancelDrag(event: PointerEvent<HTMLButtonElement>) {
    if (drag.current?.pointer !== event.pointerId) return;
    drag.current = null;
    setPreview(null);
  }

  function nudge(event: KeyboardEvent<HTMLButtonElement>, device: DeviceTopology) {
    const direction = keyboardDirections[event.key];
    if (!direction || drag.current || savingMove) return;
    event.preventDefault();
    const step = event.shiftKey ? 120 : 24;
    void writePlacement(device.device_id, {
      x: device.placement.x + step * direction[0],
      y: device.placement.y + step * direction[1],
    });
  }

  function autoArrange() {
    let x = 0;
    const localId = snapshot.local.device_id;
    const order = [...groups].sort((a, b) => Number(b.device.device_id === localId) - Number(a.device.device_id === localId)
      || a.device.name.localeCompare(b.device.name));
    const placements = order.map(({ device, width }) => {
      const placement = { ...device.placement, x, y: 0 };
      x += width;
      return placement;
    });
    void run(api.updatePlacements(placements, topology.boundary_mode));
  }

  function setBoundaryMode(boundaryMode: BoundaryMode) {
    void run(api.updatePlacements(topology.devices.map((device) => device.placement), boundaryMode));
  }

  const saveSensitivity = (patch: Partial<AppConfig["sensitivity"]>) => run(api.updateConfig((current) =>
    ({ sensitivity: { ...current.sensitivity, ...patch } })));

  return (
    <>
      <PageHeader title="Screen arrangement" description="Match the layout of your physical displays to move between computers.">
        <button className="btn" onClick={() => run(api.refreshMonitors())}><RefreshCw /> Refresh monitors</button>
        <button className="btn btn-primary" onClick={autoArrange} disabled={groups.length < 2 || savingMove}><LayoutGrid /> Auto-arrange row</button>
      </PageHeader>

      <section className="card canvas-panel">
        <PanelHeading icon={<Scan />} title="Display layout">
          <span className="pill pill-neutral">{groups.length} {groups.length === 1 ? "computer" : "computers"} · {groups.reduce((count, group) => count + group.displays.length, 0)} displays</span>
        </PanelHeading>
        <div className="canvas-wrap" ref={canvasRef} aria-label="Display layout" aria-describedby="canvas-instructions">
          {preview?.guideX !== undefined && <span className="snap-line snap-line-x" style={{ left: view.x + preview.guideX * view.scale }} />}
          {preview?.guideY !== undefined && <span className="snap-line snap-line-y" style={{ top: view.y + preview.guideY * view.scale }} />}
          {!groups.some((group) => group.displays.length) && <div className="canvas-empty">No display details available. Select Refresh monitors to try again.</div>}
          {groups.map((group) => {
            const { device, displays, width, height } = group;
            if (!displays.length || !canvasSize.width) return null;
            const placement = preview?.placement.device_id === device.device_id ? preview.placement : device.placement;
            const isSelected = selected?.device.device_id === device.device_id;
            const isFocused = snapshot.focused_device === device.device_id;
            const isController = snapshot.active_controller === device.device_id;
            const roles = [isController && "Controller", isFocused && "Input"].filter(Boolean);
            return (
              <button
                key={device.device_id}
                type="button"
                className={`canvas-node ${isFocused ? "focused" : ""} ${!device.enabled ? "canvas-offline" : ""} ${preview?.placement.device_id === device.device_id ? "dragging" : ""}`}
                style={{ left: view.x + placement.x * view.scale, top: view.y + placement.y * view.scale, width: width * view.scale, height: height * view.scale }}
                aria-label={[device.name, ...roles, !device.enabled && "Offline"].filter(Boolean).join(", ")}
                aria-pressed={isSelected}
                title={device.name}
                onClick={() => setSelectedId(device.device_id)}
                onPointerDown={(event) => beginDrag(event, group)}
                onPointerMove={moveDrag}
                onPointerUp={finishDrag}
                onPointerCancel={cancelDrag}
                onLostPointerCapture={cancelDrag}
                onKeyDown={(event) => nudge(event, device)}
              >
                {displays.map((display, index) => {
                  const monitor = device.monitors[index];
                  const large = display.width * view.scale >= 112 && display.height * view.scale >= 78;
                  return (
                    <span className={`monitor-box ${large ? "" : "monitor-compact"}`} key={index}
                      style={{ left: display.x * view.scale, top: display.y * view.scale, width: display.width * view.scale, height: display.height * view.scale }}
                    >
                      <Monitor className="canvas-monitor-icon" />
                      <strong>{device.name}{displays.length > 1 ? ` · ${index + 1}` : ""}</strong>
                      {large && monitor && <span className="mono monitor-resolution">{monitor.physical_width || monitor.width} × {monitor.physical_height || monitor.height}{monitor.scale_factor > 1.05 ? ` · ${monitor.scale_factor.toFixed(1)}×` : ""}</span>}
                      {large && roles.length > 0 && <span className="monitor-role">{roles.join(" · ")}</span>}
                    </span>
                  );
                })}
              </button>
            );
          })}
        </div>
        <div className="canvas-footer">
          <p className="row muted" id="canvas-instructions"><Grip /> Drag to position. Focus a computer and use arrow keys to fine-tune.</p>
          <div className="row canvas-legend"><span><i /> Selected</span><span><i /> Input destination</span></div>
        </div>
      </section>

      <div className="grid-2 arrangement-options">
        <section className="card form-panel">
          <PanelHeading icon={<ArrowLeftRight />} title="Screen boundaries" description="Choose which edges let the pointer cross." />
          {selected && (
            <div className="form-stack">
              <label className="field">Computer
                <select className="select" value={selected.device.device_id} onChange={(event) => setSelectedId(event.target.value)}>
                  {groups.map(({ device }) => <option key={device.device_id} value={device.device_id}>{device.name}</option>)}
                </select>
              </label>
              <p className="muted mono">{selected.device.placement.x}, {selected.device.placement.y} · {selected.displays.length} {selected.displays.length === 1 ? "display" : "displays"}</p>
              <div className="edge-controls">
                {edges.map((edge) => (
                  <div className="row-between edge-control" key={edge}>
                    <span>{edge}</span>
                    <Switch label={`Allow crossing the ${edge} edge`} checked={selected.device.placement.enabled_edges[edge]}
                      onChange={(checked) => writePlacement(selected.device.device_id, { enabled_edges: { ...selected.device.placement.enabled_edges, [edge]: checked } })} />
                  </div>
                ))}
              </div>
              <label className="field">Boundary mapping
                <select className="select" value={topology.boundary_mode} onChange={(event) => setBoundaryMode(event.target.value as BoundaryMode)}>
                  <option value="overlapping">Overlapping edges</option>
                  <option value="proportional">Proportional across the full edge</option>
                </select>
              </label>
              <p className="muted">{topology.boundary_mode === "overlapping"
                ? "Cross where screen edges line up physically."
                : "Map the full edge to the neighboring display."}</p>
            </div>
          )}
        </section>

        <section className="card settings-panel">
          <PanelHeading icon={<SlidersHorizontal />} title="Pointer transitions" description="Tune how the pointer moves between computers." />
          <div className="settings-rows">
            <RangeSetting title="Remote pointer sensitivity" value={config.sensitivity.pointer_sensitivity} min={0.25} max={3} step={0.05}
              format={(value) => `${value.toFixed(2)}×`} onCommit={(value) => saveSensitivity({ pointer_sensitivity: value })} />
            <RangeSetting title="Edge resistance dwell" value={config.sensitivity.edge_resistance_ms} min={0} max={400} step={20}
              format={(value) => `${value} ms`} onCommit={(value) => saveSensitivity({ edge_resistance_ms: value })} />
            <RangeSetting title="Edge push barrier" value={config.sensitivity.edge_barrier_px} min={0} max={60} step={5}
              format={(value) => `${value} px`} onCommit={(value) => saveSensitivity({ edge_barrier_px: value })} />
            <RangeSetting title="Corner deadzone" value={config.sensitivity.corner_deadzone_px} min={0} max={48} step={4}
              format={(value) => `${value} px`} onCommit={(value) => saveSensitivity({ corner_deadzone_px: value })} />
          </div>
        </section>
      </div>
    </>
  );
}
