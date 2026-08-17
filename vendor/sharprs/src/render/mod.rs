//! Rendering primitives and sounding image compositor for sharprs.
//!
//! Provides a self-contained RGBA pixel canvas with antialiased drawing,
//! bitmap font text, wind barbs, and PNG output.  The [`compositor`] module
//! assembles all panels into the final SHARPpy-style sounding analysis image.

pub mod canvas;
pub mod compositor;
pub mod hodograph;
pub mod panels;
pub mod param_table;
pub mod parcel_summary;
pub mod skewt;

pub use canvas::{Canvas, ClippedCanvas};
pub use compositor::{compute_all_params, render_full_sounding, ComputedParams};
pub use hodograph::{
    draw_hodograph, hodograph_data_from_profile, render_hodograph, CorfidiVector, HodographData,
    SRWindLayer, StormMotion, WindLevel,
};
pub use parcel_summary::{native_parcel_summaries, NativeParcelFlavor, NativeParcelSummary};
