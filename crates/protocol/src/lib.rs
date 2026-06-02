//! Shared wire protocol and layout math for `move freely`.
//!
//! This crate is OS-agnostic and side-effect free: it defines the messages
//! exchanged between the server (the machine owning the physical keyboard +
//! mouse) and its clients, the length-prefixed framing used on the wire, and
//! the pure geometry that decides when the pointer crosses a screen edge.

pub mod frame;
pub mod layout;
pub mod message;

pub use frame::{read_frame, write_frame, FrameError};
pub use layout::{Edge, Layout, Position, Screen};
pub use message::{Button, Key, Message, PROTOCOL_VERSION};
