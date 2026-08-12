// SPDX-License-Identifier: Apache-2.0

//! Embeddable ArWen forecast planner and run monitor.
//!
//! BowEcho hosts this library in its WRF workspace while the gpuwm engine
//! remains an isolated child process. The standalone binary is retained for
//! the upstream acceptance matrix and pre-vendor history.

// The vendored snapshot predates several Rust 1.94 style lints. Keep its
// already-tested control flow intact; these are non-correctness suggestions.
#![allow(
    clippy::collapsible_if,
    clippy::doc_lazy_continuation,
    clippy::field_reassign_with_default,
    clippy::match_single_binding,
    clippy::needless_borrow,
    clippy::needless_range_loop,
    clippy::redundant_closure
)]

mod advanced;
pub mod app;
mod draft;
mod inspector;
mod kit;
#[allow(dead_code)] // Standalone smoke/live-fire drivers are retained for the acceptance matrix.
mod livefire;
mod map_pane;
#[cfg(test)]
mod matrix;
mod model_layer;
mod products;
mod run_session;
mod settings;
mod standalone;
mod storm;
mod theme;
mod timeline;

pub use app::StudioApp;
pub use standalone::run_standalone;
