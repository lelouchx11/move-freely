//! Shared wire protocol and 2D multi-monitor layout math for `move freely`.
//!
//! This crate is OS-agnostic and side-effect free: it defines the messages
//! exchanged between peer devices, length-prefixed framing for both JSON
//! control frames and raw binary bulk-transfer chunks, and the pure 2D
//! multi-monitor geometry that decides when the pointer crosses a screen edge.

pub mod frame;
pub mod layout;
pub mod message;

pub use frame::{
    read_frame, read_wire_frame, write_frame, write_wire_frame, BinaryChunk, FrameError, WireFrame,
    MAX_FRAME_LEN,
};
pub use layout::{
    BoundaryMode, ClusterTopology, DevicePlacement, DeviceTopology, Edge, EdgeMask,
    EdgeResistanceTracker, EdgeSensitivity, Layout, Monitor, PointerRoutingState, Position, Rect,
    Screen, StepOutcome,
};
pub use message::{
    validate_rel_path, ArchKind, Button, ChannelRole, DeviceId, KeyCode, Message, ModifierFlags,
    OsKind, PeerCapabilities, SharedFileEntry, PROTOCOL_VERSION,
};
