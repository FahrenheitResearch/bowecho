//! Crate-wide error type for fallible sounding operations.

use thiserror::Error;

/// Errors that can occur during sounding analysis.
#[derive(Debug, Error)]
pub enum SharpError {
    /// A required field on the profile was entirely missing / all-None.
    #[error("profile field `{field}` has no valid data")]
    NoData { field: &'static str },

    /// Interpolation target is outside the range of available data.
    #[error("interpolation target {value} is out of range [{lo}, {hi}]")]
    OutOfRange { value: f64, lo: f64, hi: f64 },

    /// A profile has too few levels to perform the requested operation.
    #[error("insufficient levels: need at least {need}, got {got}")]
    InsufficientLevels { need: usize, got: usize },

    /// Iterative solver did not converge.
    #[error("convergence failure in `{routine}` after {iters} iterations")]
    Convergence { routine: &'static str, iters: usize },

    /// A parcel lift found no LFC (e.g., stable profile).
    #[error("no level of free convection found")]
    NoLfc,

    /// Generic invalid input.
    #[error("{0}")]
    InvalidInput(String),
}
