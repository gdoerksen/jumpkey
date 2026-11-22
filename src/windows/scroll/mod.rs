//! Scroll mode's Windows shell (#54). The key logic and the motion model
//! are platform-free in [`crate::scroll`]; this module owns the OS side.
//!
//! - [`hook`]: the dedicated keyboard and mouse hook thread.
//!
//! Area discovery, the scroll engine and the indicator come with the
//! feature PR (roadmap step 5, part 2).

pub mod areas;
pub mod engine;
pub mod hook;
pub mod indicator;
pub mod session;
