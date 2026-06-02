//! Screen layout and edge-crossing geometry.
//!
//! v1 arranges up to three screens in a single horizontal row: a `Center`
//! screen (the server's own display) with an optional `Left` and `Right`
//! neighbour. All of this is pure data + math so it can be unit-tested without
//! any OS involvement.

use serde::{Deserialize, Serialize};

/// Where a screen sits relative to the server's own (`Center`) display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Left,
    Center,
    Right,
}

impl Position {
    /// Left-to-right ordering used for adjacency: Left=-1, Center=0, Right=1.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// A single screen in the layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub name: String,
    pub position: Position,
    pub width: u32,
    pub height: u32,
}

/// The full set of screens, indexed by position.
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

    /// The screen immediately adjacent to `from` across `edge`, if one exists.
    ///
    /// Only horizontal crossings (`Left`/`Right`) connect screens in v1; the
    /// vertical edges have no neighbour.
    pub fn neighbour(&self, from: Position, edge: Edge) -> Option<&Screen> {
        let target_order = match edge {
            Edge::Right => from.order() + 1,
            Edge::Left => from.order() - 1,
            Edge::Top | Edge::Bottom => return None,
        };
        let target = Position::from_order(target_order)?;
        self.screen(target)
    }

    /// Resolve a pointer crossing.
    ///
    /// `frac` is the normalized position (0.0..=1.0) along the edge the pointer
    /// exited — for a horizontal crossing this is the vertical fraction. Returns
    /// the destination screen plus the entry pixel coordinates on it, preserving
    /// the fraction so the pointer lands at the matching height.
    pub fn cross(&self, from: Position, edge: Edge, frac: f64) -> Option<(Position, i32, i32)> {
        let dest = self.neighbour(from, edge)?;
        let frac = frac.clamp(0.0, 1.0);
        let (x, y) = match edge {
            // Exit right edge of A -> enter left edge (x=0) of B.
            Edge::Right => (0, (frac * dest.height.saturating_sub(1) as f64).round() as i32),
            // Exit left edge of A -> enter right edge of B.
            Edge::Left => (
                dest.width.saturating_sub(1) as i32,
                (frac * dest.height.saturating_sub(1) as f64).round() as i32,
            ),
            Edge::Top | Edge::Bottom => return None,
        };
        Some((dest.position, x, y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_screen_layout() -> Layout {
        Layout::new(vec![
            Screen { name: "lefty".into(), position: Position::Left, width: 1920, height: 1080 },
            Screen { name: "mac".into(), position: Position::Center, width: 2560, height: 1440 },
            Screen { name: "righty".into(), position: Position::Right, width: 1280, height: 720 },
        ])
    }

    #[test]
    fn neighbours_follow_horizontal_order() {
        let l = three_screen_layout();
        assert_eq!(l.neighbour(Position::Center, Edge::Right).unwrap().name, "righty");
        assert_eq!(l.neighbour(Position::Center, Edge::Left).unwrap().name, "lefty");
        assert_eq!(l.neighbour(Position::Right, Edge::Left).unwrap().name, "mac");
        assert_eq!(l.neighbour(Position::Left, Edge::Right).unwrap().name, "mac");
        // Outermost edges have no neighbour.
        assert!(l.neighbour(Position::Right, Edge::Right).is_none());
        assert!(l.neighbour(Position::Left, Edge::Left).is_none());
        // Vertical edges never connect in v1.
        assert!(l.neighbour(Position::Center, Edge::Top).is_none());
    }

    #[test]
    fn crossing_right_enters_left_edge_preserving_height() {
        let l = three_screen_layout();
        // Exit center's right edge halfway down -> enter righty at x=0, ~middle.
        let (pos, x, y) = l.cross(Position::Center, Edge::Right, 0.5).unwrap();
        assert_eq!(pos, Position::Right);
        assert_eq!(x, 0);
        assert_eq!(y, ((0.5 * 719.0) as f64).round() as i32); // 360
    }

    #[test]
    fn crossing_left_enters_right_edge() {
        let l = three_screen_layout();
        let (pos, x, y) = l.cross(Position::Center, Edge::Left, 0.0).unwrap();
        assert_eq!(pos, Position::Left);
        assert_eq!(x, 1919); // lefty.width - 1
        assert_eq!(y, 0);
    }

    #[test]
    fn crossing_with_no_neighbour_is_none() {
        let l = three_screen_layout();
        assert!(l.cross(Position::Right, Edge::Right, 0.5).is_none());
    }
}
