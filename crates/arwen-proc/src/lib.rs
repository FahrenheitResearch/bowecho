// SPDX-License-Identifier: Apache-2.0

//! Process supervision for ArWen Studio's gpuwm boundary.
//!
//! - [`process`] — sealed-environment child ownership (adopted verbatim
//!   from rusty-weather rw-sim; Windows Job Objects, POSIX process groups,
//!   env_clear + explicit map).
//! - [`gpu_lock`] — the GPU-UUID advisory lock protocol shared with gpuwm
//!   (Studio only probes it).
//! - [`launcher`] — the ONE command builder every launch path uses, the
//!   [`launcher::ContractSource`] fixture/live switch, and run spawning
//!   with explicit ownership (die-with-Studio vs survive-Studio).
//! - [`registry`] — the plan-hash-keyed run registry (plan bytes,
//!   events.jsonl, launch record).
//! - [`tail`] — incremental JSONL tailing of events.jsonl: the ONE event
//!   source for live runs, reattach, and fixture replay alike.
//! - [`replay`] — the fixture-run core: replays a fixture event stream to
//!   stdout and maintains a `run-progress.json` heartbeat, so the full
//!   run/detach/reattach path is exercisable before the live CLI lands.

pub mod gpu_lock;
pub mod launcher;
pub mod process;
pub mod registry;
pub mod replay;
pub mod tail;

pub use launcher::{ContractSource, GeneratedConfig, LauncherSpec, RunOwnership};
pub use registry::{LaunchRecord, RunRegistry};
pub use tail::JsonlTail;
