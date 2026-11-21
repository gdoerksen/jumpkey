//! Public API for [jumpkey](https://github.com/gdoerksen/jumpkey).
//!
//! `jumpkey` is a system-wide keyboard navigation overlay. Press a leader
//! chord, see hint labels on every interactable control on screen, type the
//! hint, done.
//!
//! The crate ships both:
//!
//! - **A binary** (`jumpkey`) — an end-user tool that registers a global
//!   hotkey, walks the foreground window's accessibility tree, and renders
//!   the hint overlay.
//! - **A library** — the platform-agnostic core types and traits
//!   ([`Action`], [`Backend`], [`Element`], [`HintEngine`]), plus a Windows
//!   backend in [`windows`] (only compiled on `target_os = "windows"`).
//!
//! A Linux backend (X11/Wayland) may land later as a module gated behind
//! its own `cfg`.

#![warn(missing_docs)]

pub mod action;
pub mod alphabet_presets;
pub mod backend;
pub mod cache;
pub mod config;
pub mod hint;
pub mod keyboard;
pub mod layout_order;
pub mod model;
pub mod telemetry;

#[cfg(windows)]
pub mod windows;

pub use action::Action;
pub use backend::Backend;
pub use config::Config;
pub use hint::{ConsecutiveKeys, HintEngine, HintStrategy, DEFAULT_ALPHABET};
pub use keyboard::{Geometry, Layout, LayoutChoice};
pub use layout_order::HintOrder;
pub use model::{Bounds, Element, ElementId, Role};
