//! Pony Town Anti-AFK - Rust port.
//! Reference implementation: the Python app in `../app/`.
//!
//! Modules:
//! - `cdp`       DevTools Protocol client (port of `app/engine.py`)
//! - `keepalive` randomized human-like keep-alive engine (port of `app/keepalive.py`)
//! - `settings`  persisted options
//! - `util`      small helpers
//! - `app`       iced GUI: top settings bar, live web view, single log bar

pub mod app;
pub mod cdp;
pub mod keepalive;
pub mod settings;
pub mod util;
