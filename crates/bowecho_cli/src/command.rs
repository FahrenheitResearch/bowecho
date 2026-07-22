use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use crate::wrf;

/// Process exit codes shared by all BowEcho command families.
///
/// Keep these stable: forecast runners may use them to distinguish bad
/// invocation, inaccessible inputs, unreadable science data, and receipt
/// mismatches without scraping diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ExitCode {
    Success = 0,
    Usage = 2,
    Input = 3,
    Data = 4,
    Verification = 5,
    Internal = 10,
}

impl ExitCode {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeContext {
    pub bowecho_version: String,
    pub bowecho_commit: String,
}

impl RuntimeContext {
    pub fn new(version: impl Into<String>, commit: impl Into<String>) -> Self {
        Self {
            bowecho_version: version.into(),
            bowecho_commit: commit.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelpTopic {
    Root,
    Wrf,
    WrfInspect,
    WrfVerify,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WrfCommand {
    Inspect { input: PathBuf, json: bool },
    Verify { manifest: PathBuf },
}

/// Extensible root command.  Future radar, satellite, sounding, and Formula
/// Lab families should be siblings of `Wrf`, not special cases in `main.rs`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliCommand {
    Help(HelpTopic),
    Version,
    Wrf(WrfCommand),
}

/// Startup routing preserves BowEcho's existing `bowecho <file>` GUI path.
/// Only an exact command-family prefix is intercepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Invocation {
    Gui { input_path: Option<PathBuf> },
    Cli(CliCommand),
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CliError {
    pub code: ExitCode,
    pub message: String,
    pub help: Option<&'static str>,
}

impl CliError {
    pub fn usage(message: impl Into<String>, help: &'static str) -> Self {
        Self {
            code: ExitCode::Usage,
            message: message.into(),
            help: Some(help),
        }
    }

    pub fn input(message: impl Into<String>) -> Self {
        Self {
            code: ExitCode::Input,
            message: message.into(),
            help: None,
        }
    }

    pub fn data(message: impl Into<String>) -> Self {
        Self {
            code: ExitCode::Data,
            message: message.into(),
            help: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: ExitCode::Internal,
            message: message.into(),
            help: None,
        }
    }
}

pub const fn help_text() -> &'static str {
    "BowEcho desktop weather analysis and local post-processing\n\n\
Usage:\n  bowecho [FILE]\n  bowecho wrf <COMMAND> [OPTIONS]\n\n\
Command families:\n  wrf       Inspect and verify WRF/GPUWM data and BowEcho artifacts\n\n\
Global options:\n  -h, --help       Show this help\n  -V, --version    Show the BowEcho version\n\n\
Without a command family, BowEcho starts the desktop app and optionally opens FILE.\n"
}

pub const fn wrf_help_text() -> &'static str {
    "BowEcho WRF/GPUWM local post-processing\n\n\
Usage:\n  bowecho wrf inspect <FILE-OR-DIRECTORY> --json\n  bowecho wrf verify <ARTIFACT-MANIFEST.JSON>\n\n\
Commands:\n  inspect    Report WRF grid, projection, times, variables, metadata, and source receipts\n  verify     Recalculate every source/artifact size and SHA-256 receipt\n\n\
Source wrfout files are read-only. BowEcho never uploads or deletes them.\n"
}

const WRF_INSPECT_HELP: &str = "Usage: bowecho wrf inspect <FILE-OR-DIRECTORY> --json\n\n\
Recursively inspects deterministic NetCDF/HDF5 candidates. JSON is written to stdout;\n\
diagnostics are written to stderr. Source files are never modified.\n";

const WRF_VERIFY_HELP: &str = "Usage: bowecho wrf verify <ARTIFACT-MANIFEST.JSON>\n\n\
Recalculates byte counts and SHA-256 hashes for every declared source and artifact.\n\
A machine-readable verification report is written to stdout.\n";

pub fn parse_invocation<I>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(first) = args.next() else {
        return Ok(Invocation::Gui { input_path: None });
    };

    if first == "wrf" {
        return parse_wrf(args.collect()).map(Invocation::Cli);
    }
    if first == "help" || first == "--help" || first == "-h" {
        return Ok(Invocation::Cli(CliCommand::Help(HelpTopic::Root)));
    }
    if first == "version" || first == "--version" || first == "-V" {
        return Ok(Invocation::Cli(CliCommand::Version));
    }

    // Existing behavior: argv[1] is an optional file for the GUI. Additional
    // arguments have historically been ignored and remain untouched here.
    Ok(Invocation::Gui {
        input_path: Some(PathBuf::from(first)),
    })
}

fn parse_wrf(args: Vec<OsString>) -> Result<CliCommand, CliError> {
    let Some(subcommand) = args.first() else {
        return Err(CliError::usage("missing WRF command", wrf_help_text()));
    };
    if subcommand == "help" || subcommand == "--help" || subcommand == "-h" {
        return Ok(CliCommand::Help(HelpTopic::Wrf));
    }
    match subcommand.to_str() {
        Some("inspect") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::WrfInspect))
        }
        Some("verify") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::WrfVerify))
        }
        Some("inspect") => parse_inspect(&args[1..]).map(CliCommand::Wrf),
        Some("verify") => parse_verify(&args[1..]).map(CliCommand::Wrf),
        Some(other) => Err(CliError::usage(
            format!("unknown WRF command '{other}'"),
            wrf_help_text(),
        )),
        None => Err(CliError::usage(
            "WRF command is not valid Unicode",
            wrf_help_text(),
        )),
    }
}

fn parse_inspect(args: &[OsString]) -> Result<WrfCommand, CliError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Err(CliError::usage("", WRF_INSPECT_HELP));
    }
    let mut input = None;
    let mut json = false;
    let mut positional_only = false;
    for arg in args {
        if !positional_only && arg == "--" {
            positional_only = true;
        } else if !positional_only && arg == "--json" {
            json = true;
        } else if !positional_only && arg.to_string_lossy().starts_with('-') {
            return Err(CliError::usage(
                format!("unknown inspect option '{}'", arg.to_string_lossy()),
                WRF_INSPECT_HELP,
            ));
        } else if input.replace(PathBuf::from(arg)).is_some() {
            return Err(CliError::usage(
                "inspect accepts exactly one file or directory",
                WRF_INSPECT_HELP,
            ));
        }
    }
    let input = input
        .ok_or_else(|| CliError::usage("inspect requires a file or directory", WRF_INSPECT_HELP))?;
    Ok(WrfCommand::Inspect { input, json })
}

fn parse_verify(args: &[OsString]) -> Result<WrfCommand, CliError> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Err(CliError::usage("", WRF_VERIFY_HELP));
    }
    let mut manifest = None;
    let mut positional_only = false;
    for arg in args {
        if !positional_only && arg == "--" {
            positional_only = true;
        } else if !positional_only && arg == "--json" {
            // Accepted for symmetry; verify is always machine-readable.
        } else if !positional_only && arg.to_string_lossy().starts_with('-') {
            return Err(CliError::usage(
                format!("unknown verify option '{}'", arg.to_string_lossy()),
                WRF_VERIFY_HELP,
            ));
        } else if manifest.replace(PathBuf::from(arg)).is_some() {
            return Err(CliError::usage(
                "verify accepts exactly one artifact manifest",
                WRF_VERIFY_HELP,
            ));
        }
    }
    let manifest = manifest
        .ok_or_else(|| CliError::usage("verify requires an artifact manifest", WRF_VERIFY_HELP))?;
    Ok(WrfCommand::Verify { manifest })
}

/// Execute a headless command without initializing GUI or GPU state.
pub fn execute(
    command: CliCommand,
    context: &RuntimeContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<ExitCode, CliError> {
    match command {
        CliCommand::Help(topic) => {
            let text = match topic {
                HelpTopic::Root => help_text(),
                HelpTopic::Wrf => wrf_help_text(),
                HelpTopic::WrfInspect => WRF_INSPECT_HELP,
                HelpTopic::WrfVerify => WRF_VERIFY_HELP,
            };
            stdout
                .write_all(text.as_bytes())
                .map_err(|error| CliError::internal(format!("write stdout: {error}")))?;
            Ok(ExitCode::Success)
        }
        CliCommand::Version => {
            writeln!(
                stdout,
                "BowEcho {} ({})",
                context.bowecho_version, context.bowecho_commit
            )
            .map_err(|error| CliError::internal(format!("write stdout: {error}")))?;
            Ok(ExitCode::Success)
        }
        CliCommand::Wrf(WrfCommand::Inspect { input, json: _ }) => {
            let (report, code) = wrf::inspect(&input, context)?;
            serde_json::to_writer_pretty(&mut *stdout, &report)
                .map_err(|error| CliError::internal(format!("serialize inspection: {error}")))?;
            writeln!(stdout)
                .map_err(|error| CliError::internal(format!("write stdout: {error}")))?;
            writeln!(
                stderr,
                "BowEcho WRF inspect: {} file(s), status {:?}",
                report.files.len(),
                report.status
            )
            .map_err(|error| CliError::internal(format!("write stderr: {error}")))?;
            for file in report.files.iter().filter(|file| !file.failures.is_empty()) {
                for failure in &file.failures {
                    writeln!(stderr, "{}: {failure}", file.path.display())
                        .map_err(|error| CliError::internal(format!("write stderr: {error}")))?;
                }
            }
            Ok(code)
        }
        CliCommand::Wrf(WrfCommand::Verify { manifest }) => {
            let (report, code) = wrf::verify(&manifest)?;
            serde_json::to_writer_pretty(&mut *stdout, &report)
                .map_err(|error| CliError::internal(format!("serialize verification: {error}")))?;
            writeln!(stdout)
                .map_err(|error| CliError::internal(format!("write stdout: {error}")))?;
            writeln!(
                stderr,
                "BowEcho WRF verify: {} receipt(s), verified={}",
                report.receipts.len(),
                report.verified
            )
            .map_err(|error| CliError::internal(format!("write stderr: {error}")))?;
            Ok(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, CliError> {
        parse_invocation(args.iter().map(OsString::from))
    }

    #[test]
    fn no_args_and_file_keep_gui_behavior() {
        assert_eq!(
            parse(&["bowecho"]).unwrap(),
            Invocation::Gui { input_path: None }
        );
        assert_eq!(
            parse(&["bowecho", "sample.ar2v"]).unwrap(),
            Invocation::Gui {
                input_path: Some(PathBuf::from("sample.ar2v"))
            }
        );
    }

    #[test]
    fn parses_exact_wrf_inspect_prefix_and_options_in_either_order() {
        let expected = Invocation::Cli(CliCommand::Wrf(WrfCommand::Inspect {
            input: PathBuf::from("run"),
            json: true,
        }));
        assert_eq!(
            parse(&["bowecho", "wrf", "inspect", "run", "--json"]).unwrap(),
            expected
        );
        assert_eq!(
            parse(&["bowecho", "wrf", "inspect", "--json", "run"]).unwrap(),
            expected
        );
    }

    #[test]
    fn supports_dash_prefixed_paths_after_separator() {
        assert_eq!(
            parse(&["bowecho", "wrf", "inspect", "--json", "--", "-run.nc"]).unwrap(),
            Invocation::Cli(CliCommand::Wrf(WrfCommand::Inspect {
                input: PathBuf::from("-run.nc"),
                json: true,
            }))
        );
    }

    #[test]
    fn bad_wrf_invocation_has_stable_usage_code() {
        let error = parse(&["bowecho", "wrf", "inspect", "a", "b"]).unwrap_err();
        assert_eq!(error.code, ExitCode::Usage);
        assert!(error.help.is_some());
    }

    #[test]
    fn all_help_forms_are_successful_commands() {
        assert_eq!(
            parse(&["bowecho", "wrf", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::Wrf))
        );
        assert_eq!(
            parse(&["bowecho", "wrf", "inspect", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::WrfInspect))
        );
    }
}
