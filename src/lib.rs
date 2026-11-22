//! Internals of [jumpkey](https://github.com/gdoerksen/jumpkey).
//!
//! `jumpkey` is a system-wide keyboard navigation overlay. Press a leader
//! chord, see hint labels on every interactable control on screen, type the
//! hint, done.
//!
//! This library exists so the binary, benchmarks, examples and tests share
//! one codebase. It is **not a public API**: the crate isn't published and
//! nothing here is stable (architecture review #18).
//!
//! - The **core** (every module except [`windows`]) is platform-free: core
//!   types and traits ([`Action`], [`Backend`], [`Element`], [`HintEngine`]),
//!   config, keyboard model and key events ([`input`]), layout. It must build and pass its tests on
//!   Linux (`cargo script check-linux`, CI).
//! - The **Windows backend** lives in [`windows`], behind `cfg(windows)`.
//!   Another platform would get its own `cfg`-gated module next to it.

#![warn(missing_docs)]

pub mod action;
pub mod alphabet_presets;
pub mod backend;
pub mod cache;
pub mod config;
pub mod hint;
pub mod input;
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
pub use input::{Key, KeyEvent, KeyTracker, Mods, NamedKey};
pub use keyboard::{Geometry, Layout, LayoutChoice};
pub use layout_order::HintOrder;
pub use model::{Bounds, Element, ElementId, Role};
