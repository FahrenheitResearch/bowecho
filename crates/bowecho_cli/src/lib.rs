//! Headless BowEcho command infrastructure.
//!
//! Parsing and machine-readable contracts live here so command families can be
//! reused without initializing eframe. GUI-owned science pipelines remain in
//! the application host: `wrf render` and `wrf watch` delegate to BowEcho's
//! existing WRF -> rw-store -> Rusty Weather pipeline rather than introducing
//! another decoder or renderer in this crate.

pub mod artifact;
mod command;
pub mod fs;
pub mod input;
pub mod paths;
pub mod radar;
pub mod run_manifest;
pub mod watch;
pub mod wrf;

pub use command::{
    CliCommand, CliError, ExitCode, HelpTopic, Invocation, RadarCommand, RadarCutSelection,
    RadarRenderOptions, RenderOptions, RenderPreset, RuntimeContext, SatelliteArchiveRange,
    SatelliteArchiveSelector, SatelliteCommand, SatelliteFetchOptions, SatelliteFrameSelection,
    SatelliteListOptions, SatelliteRenderOptions, WatchOptions, WrfCommand, execute, help_text,
    parse_invocation, radar_help_text, satellite_help_text, wrf_help_text,
};
