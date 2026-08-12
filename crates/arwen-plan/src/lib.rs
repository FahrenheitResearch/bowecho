// SPDX-License-Identifier: Apache-2.0

//! The gpuwm run-plan contract, in types.
//!
//! This is the ONLY crate in the workspace that touches gpuwm's process
//! boundary. Everything Studio sends to or reads from `gpuwm run-plan` is
//! defined here:
//!
//! Aligned to the REAL engine seam: gpuwm/runplan.py + docs/run-plan.md
//! @ wt-runplan 795999cc8 (contract strings final). Schema ids:
//! `gpuwm.run-plan.v1` (plan), `gpuwm.run-plan.event.v1` (events),
//! `gpuwm.run-manifest.v1` (manifest), `gpuwm.run-plan.resolved.v1`,
//! `gpuwm.run-plan.estimate.v1`, `gpuwm.run-plan.probe.v1`, plus the two
//! legacy-convention ids the engine references but does not own:
//! `gpuwm.run-progress/v1` and `gpuwm.failure-capsule/v3` (opaque, exact).
//!
//! - [`plan`] — the plan document Studio submits (unknown keys REFUSED
//!   engine-side; Studio serializes only declared keys).
//! - [`config`] — the intent-level config TOML Studio can embed inline
//!   (see the module doc for the open engine request).
//! - [`events`] — the JSONL event envelope, nine tags, dense sequence
//!   from 1 ([`SequenceGate`] refuses gaps like the engine's reader).
//! - [`manifest`] — run-manifest.json, the reattach entry point.
//! - [`heartbeat`] — gpuwm.supervisor's run-progress.json heartbeat +
//!   the status→stage mapping (mirrors runplan.py `_PHASE_STAGES`).
//! - [`queries`] — --probe / --estimate / --resolve reply documents
//!   (null-with-basis fields rendered, never re-derived).
//!
//! Deserialization is tolerant (no `deny_unknown_fields`, `Option` for
//! non-essential fields) so the engine can grow the contract without
//! breaking older Studios; serialization is CLOSED because the engine
//! refuses unknown keys. Studio consumes the engine's schemas and never
//! mints its own event schema.

pub mod config;
pub mod events;
pub mod heartbeat;
pub mod manifest;
pub mod plan;
pub mod queries;

pub use config::StudioConfig;
pub use events::{EVENT_SCHEMA, Resolution, RunEvent, RunEventEnvelope, STAGES, SequenceGate};
pub use heartbeat::{HEARTBEAT_SCHEMA, HeartbeatStage, RunProgress, interpret_heartbeat_status};
pub use manifest::{MANIFEST_SCHEMA, RunManifest};
pub use plan::{FetchSpec, PLAN_SCHEMA, PlanConfig, RunOptions, RunPlan};
pub use queries::{
    DeviceStatus, EstimateReport, ProbeReport, ResolveReport, ResolvedGeometry,
    configuration_lambert_geometry,
};
