//! # sharprs
//!
//! Rust port of SHARPpy's `sharptab` -- the Sounding/Hodograph Analysis and
//! Research Program thermodynamic and kinematic sounding analysis library.
//!
//! ## Module dependency graph (mirrors SHARPpy's sharptab)
//!
//! ```text
//! constants          (leaf -- no deps)
//!   +-> utils        (constants)
//!         +-> thermo  (constants, utils)
//!               +-> interp   (constants, thermo, profile types)
//!                     +-> winds   (constants, utils, interp)
//!                           +-> params  (constants, utils, thermo, interp, winds)
//!                                 +-> fire        (constants, thermo, interp, profile)
//!                                 +-> watch_type  (constants, thermo, interp, params)
//!                                 +-> profile     (constants, thermo, utils)
//! ```
//!
//! ## Design decisions
//!
//! - **`f64` everywhere** -- meteorological precision demands it; no f32.
//! - **`f64::NAN` for missing data** -- replaces SHARPpy's masked arrays.
//!   Helpers like `profile::is_valid()` make it easy to skip NaN entries.
//! - **`Result<T, SharpError>`** for fallible operations (interpolation
//!   off-grid, insufficient data, etc.). Pure arithmetic helpers return `f64`.
//! - **Profile is a flat struct with `Vec<f64>` columns** -- cache-friendly,
//!   easy to serialize, mirrors the masked-array paradigm without NumPy.
//! - **No heap allocations in leaf functions** (constants, thermo, utils).
//! - **`#[cfg(feature = "python")]`** gates PyO3 + numpy bindings.
//! - **`#[cfg(feature = "wasm")]`** gates wasm-bindgen bindings.

pub mod constants;
pub mod error;
pub mod fire;
pub mod interp;
pub mod params;
pub mod profile;
#[cfg(feature = "python")]
pub mod python;
pub mod render;
pub mod thermo;
pub mod utils;
pub mod watch_type;
pub mod winds;

// Re-export the most commonly used types at crate root.
pub use constants::MISSING;
pub use error::SharpError;
pub use profile::Profile;
