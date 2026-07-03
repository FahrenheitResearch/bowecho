//! Shared egui-shell library for this workspace's frontends (BowEcho and
//! miniDerecho). Extraction home per docs/miniderecho-spec.md §2; also the
//! CP-1 landing pad for v0.29's `render_service` / `loop_engine` modules
//! (docs/v029-engine-spec.md §12a) — future tenants, `pub` types, app_ui
//! first consumer, mini_ui second.

pub mod geo;
pub mod loop_engine;
pub mod render_service;
pub mod tiles;
pub mod worker_slot;
