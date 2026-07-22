//! Headless BowEcho command infrastructure.
//!
//! Parsing and machine-readable contracts live here so command families can be
//! reused without initializing eframe.  GUI-owned science pipelines remain in
//! the application host: future `wrf render` and `wrf watch` commands should
//! delegate to BowEcho's existing WRF -> rw-store -> Rusty Weather pipeline,
//! rather than introducing another decoder or renderer in this crate.

pub mod artifact;
mod command;
pub mod fs;
pub mod wrf;

pub use command::{
    CliCommand, CliError, ExitCode, HelpTopic, Invocation, RuntimeContext, WrfCommand, execute,
    help_text, parse_invocation, wrf_help_text,
};
