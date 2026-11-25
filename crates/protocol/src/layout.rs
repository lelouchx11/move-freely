//! Screen layout and 2D multi-monitor edge-crossing geometry.
//!
//! Supports both the legacy 1D horizontal layout (`Position`, `Screen`, `Layout`)
//! for backward compatibility with `config/layout.toml` AND the full 2D
//! multi-monitor, DPI-normalized [`ClusterTopology`] used by the peer mesh and
//! interactive screen arrangement UI.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::message::DeviceId;

// ---------------------------------------------------------------------------
// Legacy v1 1D layout types (preserved for v1 TOML compatibility)
// ---------------------------------------------------------------------------

/// Where a screen sits relative to the server's own (`Center`) display in v1 configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Left,
    Center,
    Right,
}

impl Position {
    pub fn order(self) -> i32 {
        match self {
            Position::Left => -1,
            Position::Center => 0,
            Position::Right => 1,
        }
    }

    fn from_order(order: i32) -> Option<Position> {
        match order {
            -1 => Some(Position::Left),
            0 => Some(Position::Center),
            1 => Some(Position::Right),
            _ => None,
        }
    }
}

/// Which edge of a screen the pointer reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Self {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// A single screen in a legacy v1 layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub name: String,
    pub position: Position,
    pub width: u32,
    pub height: u32,
}

/// Legacy v1 horizontal layout.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub screens: Vec<Screen>,
}

impl Layout {
    pub fn new(screens: Vec<Screen>) -> Self {
        Layout { screens }
    }

    pub fn screen(&self, position: Position) -> Option<&Screen> {
        self.screens.iter().find(|s| s.position == position)
    }

    pub fn neighbour(&self, from: Position, edge: Edge) -> Option<&Screen> {
        let target_order = match edge {
            Edge::Right => from.order() + 1,
            Edge::Left => from.order() - 1,
            Edge::Top | Edge::Bottom => return None,
        };
        let target = Position::from_order(target_order)?;
        self.screen(target)
    }

    pub fn cross(&self, from: Position, edge: Edge, frac: f64) -> Option<(Position, i32, i32)> {
        let dest = self.neighbour(from, edge)?;
        let frac = frac.clamp(0.0, 1.0);
        let (x, y) = match edge {
            Edge::Right => (0, (frac * dest.height.saturating_sub(1) as f64).round() as i32),
            Edge::Left => (
                dest.width.saturating_sub(1) as i32,
                (frac * dest.height.saturating_sub(1) as f64).round() as i32,
            ),
            Edge::Top | Edge::Bottom => return None,
        };
        Some((dest.position, x, y))
    }
}

// ---------------------------------------------------------------------------
// v2 2D Multi-Monitor & DPI-Aware Cluster Topology
// ---------------------------------------------------------------------------

/// Axis-aligned integer rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    pub fn contains_point(self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    pub fn contains_f64(self, px: f64, py: f64) -> bool {
        px >= self.x as f64
            && px < self.right() as f64
            && py >= self.y as f64
            && py < self.bottom() as f64
    }

    pub fn clamp_point(self, px: i32, py: i32) -> (i32, i32) {
        let max_x = self.right().saturating_sub(1).max(self.x);
        let max_y = self.bottom().saturating_sub(1).max(self.y);
        (px.clamp(self.x, max_x), py.clamp(self.y, max_y))
    }
}

/// Physical monitor attached to a device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    /// Origin and size in the device's OS input coordinate space
    /// (logical points on macOS; physical pixels on Windows/X11).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// True physical resolution in pixels.
    pub physical_width: u32,
    pub physical_height: u32,
    /// Display scale factor (`2.0` on Retina / 200% Windows scaling).
    pub scale_factor: f64,
    pub is_primary: bool,
}

impl Monitor {
    pub fn os_rect(&self) -> Rect {
        Rect::new(self.x, self.y, self.width.max(1), self.height.max(1))
    }

    /// DPI-normalized logical dimensions used on the global arrangement canvas.
    pub fn logical_size(&self) -> (u32, u32) {
        let scale = if self.scale_factor.is_finite() && self.scale_factor >= 0.5 {
            self.scale_factor
        } else {
            1.0
        };
        let pw = if self.physical_width > 0 {
            self.physical_width
        } else {
            self.width
        };
        let ph = if self.physical_height > 0 {
            self.physical_height
        } else {
            self.height
        };
        let lw = ((pw as f64) / scale).round().max(1.0) as u32;
        let lh = ((ph as f64) / scale).round().max(1.0) as u32;
        (lw, lh)
    }
}

/// Controls which outer edges of a computer can transition to neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeMask {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl Default for EdgeMask {
    fn default() -> Self {
        Self {
            left: true,
            right: true,
            top: true,
            bottom: true,
        }
    }
}

impl EdgeMask {
    pub fn allows(self, edge: Edge) -> bool {
        match edge {
            Edge::Left => self.left,
            Edge::Right => self.right,
            Edge::Top => self.top,
            Edge::Bottom => self.bottom,
        }
    }
}

/// How edge crossings match points along adjacent screen boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoundaryMode {
    /// Only overlapping coordinate ranges cross (matches macOS Display Settings).
    #[default]
    Overlapping,
    /// Any point along the edge maps proportionally onto the neighbour's edge.
    Proportional,
}

/// Global canvas placement for one computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePlacement {
    pub device_id: DeviceId,
    /// Top-left corner of the device's composite bounding box on the global canvas.
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub enabled_edges: EdgeMask,
}

/// A computer participating in the 2D cluster layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceTopology {
    pub device_id: DeviceId,
    pub name: String,
    pub monitors: Vec<Monitor>,
    pub placement: DevicePlacement,
    pub enabled: bool,
}

/// Resolved monitor rectangle in global logical canvas space paired with its OS rect.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalMonitor {
    pub device_id: DeviceId,
    pub monitor_index: usize,
    pub canvas_rect: Rect,
    pub os_rect: Rect,
    pub scale_factor: f64,
}

impl GlobalMonitor {
    /// Map a global canvas coordinate inside this monitor to OS input coordinates.
    pub fn canvas_to_os(&self, cx: f64, cy: f64) -> (i32, i32) {
        let u = if self.canvas_rect.width > 1 {
            ((cx - self.canvas_rect.x as f64) / (self.canvas_rect.width as f64 - 1.0))
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let v = if self.canvas_rect.height > 1 {
            ((cy - self.canvas_rect.y as f64) / (self.canvas_rect.height as f64 - 1.0))
                .clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ox = self.os_rect.x
            + (u * (self.os_rect.width.saturating_sub(1)) as f64).round() as i32;
        let oy = self.os_rect.y
            + (v * (self.os_rect.height.saturating_sub(1)) as f64).round() as i32;
        self.os_rect.clamp_point(ox, oy)
    }

    /// Map an OS input coordinate on this monitor to global canvas coordinates.
    pub fn os_to_canvas(&self, ox: i32, oy: i32) -> (f64, f64) {
        let u = if self.os_rect.width > 1 {
            ((ox - self.os_rect.x) as f64 / (self.os_rect.width as f64 - 1.0)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let v = if self.os_rect.height > 1 {
            ((oy - self.os_rect.y) as f64 / (self.os_rect.height as f64 - 1.0)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let cx = self.canvas_rect.x as f64 + u * (self.canvas_rect.width.saturating_sub(1)) as f64;
        let cy =
            self.canvas_rect.y as f64 + v * (self.canvas_rect.height.saturating_sub(1)) as f64;
        (cx, cy)
    }
}

/// Sensitivity and edge-resistance tuning parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeSensitivity {
    /// Multiplier applied to remote cursor motion (`0.25 ..= 3.0`).
    pub pointer_sensitivity: f64,
    /// Minimum time (ms) pushing against an edge before transitioning (`0` = instant).
    pub edge_resistance_ms: u32,
    /// Minimum overshoot distance (logical px) pushed past an edge before crossing.
    pub edge_barrier_px: u32,
    /// Deadzone at screen corners (logical px) to avoid accidental corner switches.
    pub corner_deadzone_px: u32,
    /// Maximum gap (logical px) between placed screens considered adjacent.
    pub adjacency_tolerance_px: i32,
}

impl Default for EdgeSensitivity {
    fn default() -> Self {
        Self {
            pointer_sensitivity: 1.0,
            edge_resistance_ms: 0,
            edge_barrier_px: 0,
            corner_deadzone_px: 16,
            adjacency_tolerance_px: 28,
        }
    }
}

/// Tracks barrier overshoot and dwell time on a specific device edge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EdgeResistanceTracker {
    active_edge: Option<(DeviceId, Edge)>,
    entered_edge_ms: u64,
    accumulated_push: f64,
}

impl EdgeResistanceTracker {
    pub fn reset(&mut self) {
        self.active_edge = None;
        self.entered_edge_ms = 0;
        self.accumulated_push = 0.0;
    }

    /// Record outward push `push_px > 0` against `(device, edge)` at `now_ms`.
    /// Returns `true` when both dwell and barrier thresholds are satisfied.
    pub fn push(
        &mut self,
        device: &DeviceId,
        edge: Edge,
        push_px: f64,
        now_ms: u64,
        cfg: &EdgeSensitivity,
    ) -> bool {
        let same = self
            .active_edge
            .as_ref()
            .is_some_and(|(d, e)| d == device && *e == edge);
        if !same {
            self.active_edge = Some((device.clone(), edge));
            self.entered_edge_ms = now_ms;
            self.accumulated_push = push_px.max(0.0);
        } else {
            self.accumulated_push += push_px.max(0.0);
        }

        let elapsed = now_ms.saturating_sub(self.entered_edge_ms);
        let dwell_ok = elapsed >= cfg.edge_resistance_ms as u64;
        let barrier_ok = self.accumulated_push >= cfg.edge_barrier_px as f64;
        if dwell_ok && barrier_ok {
            self.reset();
            true
        } else {
            false
        }
    }
}

/// Mutable routing state held by the active controller.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerRoutingState {
    pub focused_device: DeviceId,
    pub canvas_x: f64,
    pub canvas_y: f64,
    pub rem_x: f64,
    pub rem_y: f64,
    pub resistance: EdgeResistanceTracker,
}

impl PointerRoutingState {
    pub fn new(focused_device: DeviceId, canvas_x: f64, canvas_y: f64) -> Self {
        Self {
            focused_device,
            canvas_x,
            canvas_y,
            rem_x: 0.0,
            rem_y: 0.0,
            resistance: EdgeResistanceTracker::default(),
        }
    }
}

/// Result of advancing the pointer inside [`ClusterTopology`].
#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    /// Pointer remains on the currently focused device at OS coordinates `(os_x, os_y)`.
    Stay { os_x: i32, os_y: i32 },
    /// Pointer crossed onto another device at its entry OS coordinates.
    Crossed {
        target_device: DeviceId,
        os_x: i32,
        os_y: i32,
        canvas_x: f64,
        canvas_y: f64,
    },
}

/// Full 2D cluster layout containing all known computers and their monitors.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClusterTopology {
    pub devices: Vec<DeviceTopology>,
    #[serde(default)]
    pub boundary_mode: BoundaryMode,
}

impl ClusterTopology {
    pub fn new(devices: Vec<DeviceTopology>, boundary_mode: BoundaryMode) -> Self {
        Self {
            devices,
            boundary_mode,
        }
    }

    pub fn device(&self, id: &DeviceId) -> Option<&DeviceTopology> {
        self.devices.iter().find(|d| &d.device_id == id)
    }

    pub fn device_mut(&mut self, id: &DeviceId) -> Option<&mut DeviceTopology> {
        self.devices.iter_mut().find(|d| &d.device_id == id)
    }

    /// Compute normalized logical monitor rectangles for a single device in local
    /// device-bounding-box coordinates (starting at `(0, 0)`), then shift by
    /// `device.placement`.
    pub fn global_monitors_for(&self, id: &DeviceId) -> Vec<GlobalMonitor> {
        let Some(dev) = self.device(id) else {
            return Vec::new();
        };
        let local = normalize_device_monitors(&dev.monitors);
        local
            .into_iter()
            .enumerate()
            .map(|(idx, (canvas_local, os_rect, scale))| GlobalMonitor {
                device_id: dev.device_id.clone(),
                monitor_index: idx,
                canvas_rect: Rect::new(
                    dev.placement.x + canvas_local.x,
                    dev.placement.y + canvas_local.y,
                    canvas_local.width,
                    canvas_local.height,
                ),
                os_rect,
                scale_factor: scale,
            })
            .collect()
    }

    /// Bounding rectangle of a device on the global canvas.
    pub fn device_bounds(&self, id: &DeviceId) -> Option<Rect> {
        let mons = self.global_monitors_for(id);
        bounding_rect(mons.iter().map(|m| m.canvas_rect))
    }

    /// Map OS coordinates on `id` into global canvas coordinates.
    pub fn os_to_canvas(&self, id: &DeviceId, os_x: i32, os_y: i32) -> Option<(f64, f64)> {
        let mons = self.global_monitors_for(id);
        let mon = mons
            .iter()
            .find(|m| m.os_rect.contains_point(os_x, os_y))
            .or_else(|| nearest_monitor_os(&mons, os_x, os_y))?;
        Some(mon.os_to_canvas(os_x, os_y))
    }

    /// Map global canvas coordinates into OS coordinates on `id`.
    pub fn canvas_to_os(&self, id: &DeviceId, cx: f64, cy: f64) -> Option<(i32, i32)> {
        let mons = self.global_monitors_for(id);
        let mon = mons
            .iter()
            .find(|m| m.canvas_rect.contains_f64(cx, cy))
            .or_else(|| nearest_monitor_canvas(&mons, cx, cy))?;
        Some(mon.canvas_to_os(cx, cy))
    }

    /// Snap a dragged device's proposed `(x, y)` magnetically to nearby device
    /// boundaries, returning the snapped `(x, y)`.
    pub fn snap_placement(&self, moving_id: &DeviceId, proposed_x: i32, proposed_y: i32, threshold: i32) -> (i32, i32) {
        let Some(moving_bounds) = self.device_bounds(moving_id) else {
            return (proposed_x, proposed_y);
        };
        let mw = moving_bounds.width as i32;
        let mh = moving_bounds.height as i32;
        let mut best_x = proposed_x;
        let mut best_dx = threshold + 1;
        let mut best_y = proposed_y;
        let mut best_dy = threshold + 1;

        for other in &self.devices {
            if &other.device_id == moving_id {
                continue;
            }
            let Some(ob) = self.device_bounds(&other.device_id) else {
                continue;
            };
            // Horizontal edge snaps: moving.right -> other.left, moving.left -> other.right,
            // plus flush top/bottom/left/right alignment.
            let x_candidates = [
                ob.x - mw,
                ob.right(),
                ob.x,
                ob.right() - mw,
            ];
            for cand_x in x_candidates {
                let d = (proposed_x - cand_x).abs();
                if d < best_dx {
                    best_dx = d;
                    best_x = cand_x;
                }
            }
            let y_candidates = [
                ob.y - mh,
                ob.bottom(),
                ob.y,
                ob.bottom() - mh,
            ];
            for cand_y in y_candidates {
                let d = (proposed_y - cand_y).abs();
                if d < best_dy {
                    best_dy = d;
                    best_y = cand_y;
                }
            }
        }
        (best_x, best_y)
    }

    /// Advance the pointer when the controller is driving a **remote** device
    /// via relative hardware deltas `(dx, dy)`.
    pub fn step_remote(
        &self,
        state: &mut PointerRoutingState,
        dx: i32,
        dy: i32,
        now_ms: u64,
        sens: &EdgeSensitivity,
    ) -> Option<StepOutcome> {
        let mons = self.global_monitors_for(&state.focused_device);
        if mons.is_empty() {
            return None;
        }
        let scale = sens.pointer_sensitivity.clamp(0.1, 5.0);
        let total_dx = state.rem_x + (dx as f64) * scale;
        let total_dy = state.rem_y + (dy as f64) * scale;
        let step_x = total_dx.trunc();
        let step_y = total_dy.trunc();
        state.rem_x = total_dx - step_x;
        state.rem_y = total_dy - step_y;

        let cur_mon = mons
            .iter()
            .find(|m| m.canvas_rect.contains_f64(state.canvas_x, state.canvas_y))
            .or_else(|| nearest_monitor_canvas(&mons, state.canvas_x, state.canvas_y))?;

        let next_x = state.canvas_x + step_x;
        let next_y = state.canvas_y + step_y;

        // Check if next position lands inside any monitor of the same device.
        if let Some(next_mon) = mons.iter().find(|m| m.canvas_rect.contains_f64(next_x, next_y)) {
            state.canvas_x = next_x;
            state.canvas_y = next_y;
            state.resistance.reset();
            let (os_x, os_y) = next_mon.canvas_to_os(next_x, next_y);
            return Some(StepOutcome::Stay { os_x, os_y });
        }

        // Determine which edge of `cur_mon` was crossed and by how many pixels.
        let Some((edge, push_px)) = detect_exit_edge(&cur_mon.canvas_rect, next_x, next_y) else {
            state.canvas_x = next_x.clamp(
                cur_mon.canvas_rect.x as f64,
                (cur_mon.canvas_rect.right() - 1) as f64,
            );
            state.canvas_y = next_y.clamp(
                cur_mon.canvas_rect.y as f64,
                (cur_mon.canvas_rect.bottom() - 1) as f64,
            );
            let (os_x, os_y) = cur_mon.canvas_to_os(state.canvas_x, state.canvas_y);
            return Some(StepOutcome::Stay { os_x, os_y });
        };

        // Check if we can cross to a neighbouring device across `edge`.
        let clamped_x = next_x.clamp(
            cur_mon.canvas_rect.x as f64,
            (cur_mon.canvas_rect.right() - 1) as f64,
        );
        let clamped_y = next_y.clamp(
            cur_mon.canvas_rect.y as f64,
            (cur_mon.canvas_rect.bottom() - 1) as f64,
        );

        if let Some((target_dev, target_mon, entry_cx, entry_cy)) = self.find_crossing_target(
            &state.focused_device,
            &cur_mon.canvas_rect,
            edge,
            clamped_x,
            clamped_y,
            sens,
        ) {
            if state
                .resistance
                .push(&state.focused_device, edge, push_px, now_ms, sens)
            {
                let (os_x, os_y) = target_mon.canvas_to_os(entry_cx, entry_cy);
                state.focused_device = target_dev.clone();
                state.canvas_x = entry_cx;
                state.canvas_y = entry_cy;
                state.rem_x = 0.0;
                state.rem_y = 0.0;
                return Some(StepOutcome::Crossed {
                    target_device: target_dev,
                    os_x,
                    os_y,
                    canvas_x: entry_cx,
                    canvas_y: entry_cy,
                });
            }
        } else {
            state.resistance.reset();
        }

        // Clamp onto the current monitor edge.
        state.canvas_x = clamped_x;
        state.canvas_y = clamped_y;
        let (os_x, os_y) = cur_mon.canvas_to_os(clamped_x, clamped_y);
        Some(StepOutcome::Stay { os_x, os_y })
    }

    /// Check whether a **local** OS pointer at `(os_x, os_y)` pushing outward by
    /// `(dx, dy)` should transition onto a neighbouring remote device.
    pub fn check_local_edge_crossing(
        &self,
        local_id: &DeviceId,
        os_x: i32,
        os_y: i32,
        dx: i32,
        dy: i32,
        now_ms: u64,
        tracker: &mut EdgeResistanceTracker,
        sens: &EdgeSensitivity,
    ) -> Option<StepOutcome> {
        let mons = self.global_monitors_for(local_id);
        let cur_mon = mons
            .iter()
            .find(|m| m.os_rect.contains_point(os_x, os_y))
            .or_else(|| nearest_monitor_os(&mons, os_x, os_y))?;

        let (cx, cy) = cur_mon.os_to_canvas(os_x, os_y);

        // Determine if the pointer sits on an outer boundary and moves further out.
        let at_left = os_x <= cur_mon.os_rect.x + 1 && dx < 0;
        let at_right = os_x >= cur_mon.os_rect.right().saturating_sub(2) && dx > 0;
        let at_top = os_y <= cur_mon.os_rect.y + 1 && dy < 0;
        let at_bottom = os_y >= cur_mon.os_rect.bottom().saturating_sub(2) && dy > 0;

        let candidate = if at_left && (-dx >= dy.abs()) {
            Some((Edge::Left, (-dx) as f64))
        } else if at_right && (dx >= dy.abs()) {
            Some((Edge::Right, dx as f64))
        } else if at_top && (-dy >= dx.abs()) {
            Some((Edge::Top, (-dy) as f64))
        } else if at_bottom && (dy >= dx.abs()) {
            Some((Edge::Bottom, dy as f64))
        } else {
            None
        };

        let Some((edge, push_px)) = candidate else {
            tracker.reset();
            return None;
        };

        // Ensure another local monitor doesn't sit immediately across this edge.
        let probe_x = match edge {
            Edge::Left => cur_mon.canvas_rect.x as f64 - 2.0,
            Edge::Right => cur_mon.canvas_rect.right() as f64 + 1.0,
            _ => cx,
        };
        let probe_y = match edge {
            Edge::Top => cur_mon.canvas_rect.y as f64 - 2.0,
            Edge::Bottom => cur_mon.canvas_rect.bottom() as f64 + 1.0,
            _ => cy,
        };
        if mons.iter().any(|m| m.canvas_rect.contains_f64(probe_x, probe_y)) {
            tracker.reset();
            return None;
        }

        let (target_dev, target_mon, entry_cx, entry_cy) =
            self.find_crossing_target(local_id, &cur_mon.canvas_rect, edge, cx, cy, sens)?;

        if tracker.push(local_id, edge, push_px, now_ms, sens) {
            let (target_os_x, target_os_y) = target_mon.canvas_to_os(entry_cx, entry_cy);
            Some(StepOutcome::Crossed {
                target_device: target_dev,
                os_x: target_os_x,
                os_y: target_os_y,
                canvas_x: entry_cx,
                canvas_y: entry_cy,
            })
        } else {
            None
        }
    }

    fn find_crossing_target(
        &self,
        from_id: &DeviceId,
        from_rect: &Rect,
        edge: Edge,
        cx: f64,
        cy: f64,
        sens: &EdgeSensitivity,
    ) -> Option<(DeviceId, GlobalMonitor, f64, f64)> {
        let from_dev = self.device(from_id)?;
        if !from_dev.placement.enabled_edges.allows(edge) {
            return None;
        }
        if is_in_corner_deadzone(from_rect, edge, cx, cy, sens.corner_deadzone_px) {
            return None;
        }

        let tol = sens.adjacency_tolerance_px.max(1);
        let mut best: Option<(i32, DeviceId, GlobalMonitor, f64, f64)> = None;

        for dev in &self.devices {
            if !dev.enabled || &dev.device_id == from_id {
                continue;
            }
            if !dev.placement.enabled_edges.allows(edge.opposite()) {
                continue;
            }
            for mon in self.global_monitors_for(&dev.device_id) {
                let mr = mon.canvas_rect;
                let gap = match edge {
                    Edge::Right => (mr.x - from_rect.right()).abs(),
                    Edge::Left => (from_rect.x - mr.right()).abs(),
                    Edge::Bottom => (mr.y - from_rect.bottom()).abs(),
                    Edge::Top => (from_rect.y - mr.bottom()).abs(),
                };
                if gap > tol {
                    continue;
                }

                // Compute perpendicular coordinate alignment on target monitor.
                let (entry_x, entry_y) = match (edge, self.boundary_mode) {
                    (Edge::Right, BoundaryMode::Overlapping) => {
                        if cy < mr.y as f64 || cy >= mr.bottom() as f64 {
                            continue;
                        }
                        ((mr.x + 2).min(mr.right() - 1) as f64, cy)
                    }
                    (Edge::Left, BoundaryMode::Overlapping) => {
                        if cy < mr.y as f64 || cy >= mr.bottom() as f64 {
                            continue;
                        }
                        ((mr.right() - 3).max(mr.x) as f64, cy)
                    }
                    (Edge::Bottom, BoundaryMode::Overlapping) => {
                        if cx < mr.x as f64 || cx >= mr.right() as f64 {
                            continue;
                        }
                        (cx, (mr.y + 2).min(mr.bottom() - 1) as f64)
                    }
                    (Edge::Top, BoundaryMode::Overlapping) => {
                        if cx < mr.x as f64 || cx >= mr.right() as f64 {
                            continue;
                        }
                        (cx, (mr.bottom() - 3).max(mr.y) as f64)
                    }
                    (Edge::Right, BoundaryMode::Proportional) => {
                        let frac = span_frac(cy, from_rect.y, from_rect.height);
                        let ey = mr.y as f64 + frac * (mr.height.saturating_sub(1)) as f64;
                        ((mr.x + 2).min(mr.right() - 1) as f64, ey)
                    }
                    (Edge::Left, BoundaryMode::Proportional) => {
                        let frac = span_frac(cy, from_rect.y, from_rect.height);
                        let ey = mr.y as f64 + frac * (mr.height.saturating_sub(1)) as f64;
                        ((mr.right() - 3).max(mr.x) as f64, ey)
                    }
                    (Edge::Bottom, BoundaryMode::Proportional) => {
                        let frac = span_frac(cx, from_rect.x, from_rect.width);
                        let ex = mr.x as f64 + frac * (mr.width.saturating_sub(1)) as f64;
                        (ex, (mr.y + 2).min(mr.bottom() - 1) as f64)
                    }
                    (Edge::Top, BoundaryMode::Proportional) => {
                        let frac = span_frac(cx, from_rect.x, from_rect.width);
                        let ex = mr.x as f64 + frac * (mr.width.saturating_sub(1)) as f64;
                        (ex, (mr.bottom() - 3).max(mr.y) as f64)
                    }
                };

                let replace = match &best {
                    None => true,
                    Some((best_gap, best_id, _, _, _)) => {
                        gap < *best_gap || (gap == *best_gap && dev.device_id < *best_id)
                    }
                };
                if replace {
                    best = Some((gap, dev.device_id.clone(), mon, entry_x, entry_y));
                }
            }
        }

        best.map(|(_, id, mon, x, y)| (id, mon, x, y))
    }
}

/// Normalize a device's OS monitors into gap-free logical canvas rects starting at `(0, 0)`.
fn normalize_device_monitors(monitors: &[Monitor]) -> Vec<(Rect, Rect, f64)> {
    if monitors.is_empty() {
        return Vec::new();
    }
    if monitors.len() == 1 {
        let m = &monitors[0];
        let (lw, lh) = m.logical_size();
        return vec![(
            Rect::new(0, 0, lw, lh),
            m.os_rect(),
            m.scale_factor.max(0.5),
        )];
    }

    // If OS coordinates already match logical sizes (e.g. macOS or uniform 1.0x Linux/Win),
    // preserve exact relative offsets directly.
    let all_logical_os = monitors.iter().all(|m| {
        let (lw, lh) = m.logical_size();
        (m.width as i32 - lw as i32).abs() <= 2 && (m.height as i32 - lh as i32).abs() <= 2
    });
    if all_logical_os {
        let min_x = monitors.iter().map(|m| m.x).min().unwrap_or(0);
        let min_y = monitors.iter().map(|m| m.y).min().unwrap_or(0);
        return monitors
            .iter()
            .map(|m| {
                let (lw, lh) = m.logical_size();
                (
                    Rect::new(m.x - min_x, m.y - min_y, lw, lh),
                    m.os_rect(),
                    m.scale_factor.max(0.5),
                )
            })
            .collect();
    }

    // Mixed-DPI multi-monitor (Windows / Linux): place primary monitor first,
    // then attach adjacent monitors flush along shared OS edges to avoid scaling gaps.
    let primary_idx = monitors
        .iter()
        .position(|m| m.is_primary)
        .unwrap_or(0);
    let mut placed: HashMap<usize, Rect> = HashMap::new();
    let (plw, plh) = monitors[primary_idx].logical_size();
    placed.insert(primary_idx, Rect::new(0, 0, plw, plh));

    // Iteratively snap monitors sharing an OS edge with an already-placed monitor.
    let mut progress = true;
    while progress && placed.len() < monitors.len() {
        progress = false;
        for i in 0..monitors.len() {
            if placed.contains_key(&i) {
                continue;
            }
            let mi = &monitors[i];
            let (lw, lh) = mi.logical_size();
            let mut found = None;
            for (&j, &rj) in &placed {
                let mj = &monitors[j];
                if (mi.x - mj.os_rect().right()).abs() <= 4 {
                    let y_frac = span_frac(mi.y as f64, mj.y, mj.height);
                    let cy = rj.y + (y_frac * rj.height as f64).round() as i32;
                    found = Some(Rect::new(rj.right(), cy, lw, lh));
                    break;
                } else if (mi.os_rect().right() - mj.x).abs() <= 4 {
                    let y_frac = span_frac(mi.y as f64, mj.y, mj.height);
                    let cy = rj.y + (y_frac * rj.height as f64).round() as i32;
                    found = Some(Rect::new(rj.x - lw as i32, cy, lw, lh));
                    break;
                } else if (mi.y - mj.os_rect().bottom()).abs() <= 4 {
                    let x_frac = span_frac(mi.x as f64, mj.x, mj.width);
                    let cx = rj.x + (x_frac * rj.width as f64).round() as i32;
                    found = Some(Rect::new(cx, rj.bottom(), lw, lh));
                    break;
                } else if (mi.os_rect().bottom() - mj.y).abs() <= 4 {
                    let x_frac = span_frac(mi.x as f64, mj.x, mj.width);
                    let cx = rj.x + (x_frac * rj.width as f64).round() as i32;
                    found = Some(Rect::new(cx, rj.y - lh as i32, lw, lh));
                    break;
                }
            }
            if let Some(rect) = found {
                placed.insert(i, rect);
                progress = true;
            }
        }
    }

    // Fallback for any disconnected monitors: scale relative to primary scale.
    let base_scale = monitors[primary_idx].scale_factor.max(0.5);
    let base_ox = monitors[primary_idx].x;
    let base_oy = monitors[primary_idx].y;
    for (i, m) in monitors.iter().enumerate() {
        placed.entry(i).or_insert_with(|| {
            let (lw, lh) = m.logical_size();
            let cx = ((m.x - base_ox) as f64 / base_scale).round() as i32;
            let cy = ((m.y - base_oy) as f64 / base_scale).round() as i32;
            Rect::new(cx, cy, lw, lh)
        });
    }

    let min_x = placed.values().map(|r| r.x).min().unwrap_or(0);
    let min_y = placed.values().map(|r| r.y).min().unwrap_or(0);
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let r = placed[&i];
            (
                Rect::new(r.x - min_x, r.y - min_y, r.width, r.height),
                m.os_rect(),
                m.scale_factor.max(0.5),
            )
        })
        .collect()
}

fn bounding_rect(mut rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    let first = rects.next()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_r = first.right();
    let mut max_b = first.bottom();
    for r in rects {
        min_x = min_x.min(r.x);
        min_y = min_y.min(r.y);
        max_r = max_r.max(r.right());
        max_b = max_b.max(r.bottom());
    }
    Some(Rect::new(
        min_x,
        min_y,
        (max_r - min_x).max(1) as u32,
        (max_b - min_y).max(1) as u32,
    ))
}

fn detect_exit_edge(rect: &Rect, nx: f64, ny: f64) -> Option<(Edge, f64)> {
    let over_left = if nx < rect.x as f64 {
        rect.x as f64 - nx
    } else {
        0.0
    };
    let over_right = if nx >= rect.right() as f64 {
        nx - (rect.right() as f64 - 1.0)
    } else {
        0.0
    };
    let over_top = if ny < rect.y as f64 {
        rect.y as f64 - ny
    } else {
        0.0
    };
    let over_bottom = if ny >= rect.bottom() as f64 {
        ny - (rect.bottom() as f64 - 1.0)
    } else {
        0.0
    };

    let candidates = [
        (Edge::Left, over_left),
        (Edge::Right, over_right),
        (Edge::Top, over_top),
        (Edge::Bottom, over_bottom),
    ];
    candidates
        .into_iter()
        .filter(|(_, d)| *d > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

fn is_in_corner_deadzone(rect: &Rect, edge: Edge, cx: f64, cy: f64, deadzone: u32) -> bool {
    if deadzone == 0 {
        return false;
    }
    let dz = deadzone as f64;
    match edge {
        Edge::Left | Edge::Right => {
            if rect.height as f64 <= dz * 2.5 {
                return false;
            }
            cy < rect.y as f64 + dz || cy > rect.bottom() as f64 - 1.0 - dz
        }
        Edge::Top | Edge::Bottom => {
            if rect.width as f64 <= dz * 2.5 {
                return false;
            }
            cx < rect.x as f64 + dz || cx > rect.right() as f64 - 1.0 - dz
        }
    }
}

fn span_frac(val: f64, origin: i32, len: u32) -> f64 {
    if len <= 1 {
        0.0
    } else {
        ((val - origin as f64) / (len as f64 - 1.0)).clamp(0.0, 1.0)
    }
}

fn nearest_monitor_canvas<'a>(mons: &'a [GlobalMonitor], cx: f64, cy: f64) -> Option<&'a GlobalMonitor> {
    mons.iter().min_by(|a, b| {
        dist_sq_rect_f64(&a.canvas_rect, cx, cy).total_cmp(&dist_sq_rect_f64(&b.canvas_rect, cx, cy))
    })
}

fn nearest_monitor_os<'a>(mons: &'a [GlobalMonitor], ox: i32, oy: i32) -> Option<&'a GlobalMonitor> {
    mons.iter().min_by(|a, b| {
        dist_sq_rect_f64(&a.os_rect, ox as f64, oy as f64)
            .total_cmp(&dist_sq_rect_f64(&b.os_rect, ox as f64, oy as f64))
    })
}

fn dist_sq_rect_f64(r: &Rect, x: f64, y: f64) -> f64 {
    let dx = if x < r.x as f64 {
        r.x as f64 - x
    } else if x >= r.right() as f64 {
        x - (r.right() as f64 - 1.0)
    } else {
        0.0
    };
    let dy = if y < r.y as f64 {
        r.y as f64 - y
    } else if y >= r.bottom() as f64 {
        y - (r.bottom() as f64 - 1.0)
    } else {
        0.0
    };
    dx * dx + dy * dy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_screen_layout() -> Layout {
        Layout::new(vec![
            Screen {
                name: "lefty".into(),
                position: Position::Left,
                width: 1920,
                height: 1080,
            },
            Screen {
                name: "mac".into(),
                position: Position::Center,
                width: 2560,
                height: 1440,
            },
            Screen {
                name: "righty".into(),
                position: Position::Right,
                width: 1280,
                height: 720,
            },
        ])
    }

    #[test]
    fn neighbours_follow_horizontal_order() {
        let l = three_screen_layout();
        assert_eq!(
            l.neighbour(Position::Center, Edge::Right).unwrap().name,
            "righty"
        );
        assert_eq!(
            l.neighbour(Position::Center, Edge::Left).unwrap().name,
            "lefty"
        );
        assert_eq!(
            l.neighbour(Position::Right, Edge::Left).unwrap().name,
            "mac"
        );
        assert_eq!(
            l.neighbour(Position::Left, Edge::Right).unwrap().name,
            "mac"
        );
        assert!(l.neighbour(Position::Right, Edge::Right).is_none());
        assert!(l.neighbour(Position::Left, Edge::Left).is_none());
        assert!(l.neighbour(Position::Center, Edge::Top).is_none());
    }

    #[test]
    fn crossing_right_enters_left_edge_preserving_height() {
        let l = three_screen_layout();
        let (pos, x, y) = l.cross(Position::Center, Edge::Right, 0.5).unwrap();
        assert_eq!(pos, Position::Right);
        assert_eq!(x, 0);
        assert_eq!(y, (0.5 * 719.0f64).round() as i32);
    }

    #[test]
    fn crossing_left_enters_right_edge() {
        let l = three_screen_layout();
        let (pos, x, y) = l.cross(Position::Center, Edge::Left, 0.0).unwrap();
        assert_eq!(pos, Position::Left);
        assert_eq!(x, 1919);
        assert_eq!(y, 0);
    }

    #[test]
    fn crossing_with_no_neighbour_is_none() {
        let l = three_screen_layout();
        assert!(l.cross(Position::Right, Edge::Right, 0.5).is_none());
    }

    fn mon(id: &str, x: i32, y: i32, w: u32, h: u32, scale: f64, primary: bool) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            x,
            y,
            width: w,
            height: h,
            physical_width: (w as f64 * scale).round() as u32,
            physical_height: (h as f64 * scale).round() as u32,
            scale_factor: scale,
            is_primary: primary,
        }
    }

    fn win_mon(id: &str, x: i32, y: i32, phys_w: u32, phys_h: u32, scale: f64, primary: bool) -> Monitor {
        Monitor {
            id: id.into(),
            name: id.into(),
            x,
            y,
            width: phys_w,
            height: phys_h,
            physical_width: phys_w,
            physical_height: phys_h,
            scale_factor: scale,
            is_primary: primary,
        }
    }

    #[test]
    fn multi_monitor_dpi_normalization_eliminates_gaps() {
        let topo = ClusterTopology::new(
            vec![DeviceTopology {
                device_id: DeviceId::from("win"),
                name: "win".into(),
                monitors: vec![
                    win_mon("4k", 0, 0, 3840, 2160, 2.0, true),
                    win_mon("1080p", 3840, 0, 1920, 1080, 1.0, false),
                ],
                placement: DevicePlacement {
                    device_id: DeviceId::from("win"),
                    x: 0,
                    y: 0,
                    enabled_edges: EdgeMask::default(),
                },
                enabled: true,
            }],
            BoundaryMode::Overlapping,
        );
        let mons = topo.global_monitors_for(&DeviceId::from("win"));
        assert_eq!(mons.len(), 2);
        assert_eq!(mons[0].canvas_rect, Rect::new(0, 0, 1920, 1080));
        assert_eq!(mons[1].canvas_rect, Rect::new(1920, 0, 1920, 1080));
    }

    #[test]
    fn remote_step_crosses_horizontally_and_vertically() {
        let mac = DeviceId::from("mac");
        let win = DeviceId::from("win");
        let linux = DeviceId::from("linux");
        let topo = ClusterTopology::new(
            vec![
                DeviceTopology {
                    device_id: mac.clone(),
                    name: "mac".into(),
                    monitors: vec![mon("m1", 0, 0, 1440, 900, 2.0, true)],
                    placement: DevicePlacement {
                        device_id: mac.clone(),
                        x: 0,
                        y: 0,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
                DeviceTopology {
                    device_id: win.clone(),
                    name: "win".into(),
                    monitors: vec![win_mon("w1", 0, 0, 3840, 2160, 2.0, true)],
                    placement: DevicePlacement {
                        device_id: win.clone(),
                        x: 1440,
                        y: 0,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
                DeviceTopology {
                    device_id: linux.clone(),
                    name: "linux".into(),
                    monitors: vec![win_mon("l1", 0, 0, 1920, 1080, 1.0, true)],
                    placement: DevicePlacement {
                        device_id: linux.clone(),
                        x: 1440,
                        y: -1080,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
            ],
            BoundaryMode::Overlapping,
        );

        let sens = EdgeSensitivity::default();
        // Start near top of Windows screen and push up onto Linux above it.
        let mut st = PointerRoutingState::new(win.clone(), 2000.0, 2.0);
        let out = topo.step_remote(&mut st, 0, -5, 100, &sens).unwrap();
        match out {
            StepOutcome::Crossed {
                target_device,
                os_x,
                os_y,
                ..
            } => {
                assert_eq!(target_device, linux);
                assert_eq!(os_x, 560); // 2000 - 1440
                assert!(os_y >= 1075);
            }
            other => panic!("expected vertical cross, got {other:?}"),
        }

        // Now move left from Windows onto Mac.
        let mut st2 = PointerRoutingState::new(win.clone(), 1442.0, 450.0);
        let out2 = topo.step_remote(&mut st2, -5, 0, 200, &sens).unwrap();
        match out2 {
            StepOutcome::Crossed {
                target_device,
                os_x,
                os_y,
                ..
            } => {
                assert_eq!(target_device, mac);
                assert!(os_x >= 1435);
                assert_eq!(os_y, 450);
            }
            other => panic!("expected horizontal cross back to mac, got {other:?}"),
        }
    }

    #[test]
    fn edge_resistance_requires_barrier_and_dwell() {
        let mac = DeviceId::from("mac");
        let win = DeviceId::from("win");
        let topo = ClusterTopology::new(
            vec![
                DeviceTopology {
                    device_id: mac.clone(),
                    name: "mac".into(),
                    monitors: vec![mon("m1", 0, 0, 1440, 900, 1.0, true)],
                    placement: DevicePlacement {
                        device_id: mac.clone(),
                        x: 0,
                        y: 0,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
                DeviceTopology {
                    device_id: win.clone(),
                    name: "win".into(),
                    monitors: vec![mon("w1", 0, 0, 1920, 1080, 1.0, true)],
                    placement: DevicePlacement {
                        device_id: win.clone(),
                        x: 1440,
                        y: 0,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
            ],
            BoundaryMode::Overlapping,
        );
        let sens = EdgeSensitivity {
            pointer_sensitivity: 1.0,
            edge_resistance_ms: 120,
            edge_barrier_px: 15,
            corner_deadzone_px: 16,
            adjacency_tolerance_px: 20,
        };
        let mut tracker = EdgeResistanceTracker::default();
        // First push at t=1000: not enough distance or dwell.
        assert!(topo
            .check_local_edge_crossing(&mac, 1439, 450, 8, 0, 1000, &mut tracker, &sens)
            .is_none());
        // Second push at t=1050 (total push 18 >= 15, but elapsed 50 < 120): still blocked.
        assert!(topo
            .check_local_edge_crossing(&mac, 1439, 450, 10, 0, 1050, &mut tracker, &sens)
            .is_none());
        // Third push at t=1150 (elapsed 150 >= 120, push >= 15): crosses!
        let crossed = topo.check_local_edge_crossing(
            &mac,
            1439,
            450,
            4,
            0,
            1150,
            &mut tracker,
            &sens,
        );
        assert!(matches!(
            crossed,
            Some(StepOutcome::Crossed { target_device, .. }) if target_device == win
        ));
    }

    #[test]
    fn snap_placement_snaps_to_adjacent_edge() {
        let mac = DeviceId::from("mac");
        let win = DeviceId::from("win");
        let topo = ClusterTopology::new(
            vec![
                DeviceTopology {
                    device_id: mac.clone(),
                    name: "mac".into(),
                    monitors: vec![mon("m1", 0, 0, 1440, 900, 1.0, true)],
                    placement: DevicePlacement {
                        device_id: mac.clone(),
                        x: 0,
                        y: 0,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
                DeviceTopology {
                    device_id: win.clone(),
                    name: "win".into(),
                    monitors: vec![mon("w1", 0, 0, 1920, 1080, 1.0, true)],
                    placement: DevicePlacement {
                        device_id: win.clone(),
                        x: 1500,
                        y: 50,
                        enabled_edges: EdgeMask::default(),
                    },
                    enabled: true,
                },
            ],
            BoundaryMode::Overlapping,
        );
        let (sx, sy) = topo.snap_placement(&win, 1452, 10, 24);
        assert_eq!((sx, sy), (1440, 0));
    }
}
