//! Scroll mode's Windows shell (#54). The key logic and the motion model
//! are platform-free in [`crate::scroll`]; this module owns the OS side.
//!
//! - [`hook`]: the dedicated keyboard and mouse hook thread.
//! - [`areas`]: finding scrollable areas with UI Automation.
//! - [`engine`]: the worker that turns motion into tagged wheel input and
//!   runs jumps.
//! - [`indicator`]: the click-through outline and digit badges.
//! - [`session`]: one session, driven from the main loop.

pub mod areas;
pub mod engine;
pub mod hook;
pub mod indicator;
pub mod session;
