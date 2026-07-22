use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use crate::input::InputSet;
use crate::radar;
use crate::watch::WatchPolicy;
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
    Unavailable = 6,
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
    WrfRender,
    WrfWatch,
    WrfVerify,
    Radar,
    RadarInspect,
    RadarRender,
    RadarVerify,
    Satellite,
    SatelliteInspect,
    SatelliteRender,
    SatelliteVerify,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderPreset {
    Severe,
}

impl RenderPreset {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Severe => "severe",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderOptions {
    pub inputs: InputSet,
    pub preset: RenderPreset,
    pub output_directory: PathBuf,
    pub run_manifest: PathBuf,
    pub workers: usize,
    pub variables: Vec<String>,
    pub json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchOptions {
    pub directory: PathBuf,
    pub preset: RenderPreset,
    pub output_directory: PathBuf,
    pub run_manifest: PathBuf,
    pub workers: usize,
    pub variables: Vec<String>,
    pub jsonl: bool,
    pub poll_interval_ms: u64,
    pub readiness: WatchPolicy,
    pub journal: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WrfCommand {
    Inspect { input: PathBuf, json: bool },
    Render(Box<RenderOptions>),
    Watch(Box<WatchOptions>),
    Verify { manifest: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RadarCutSelection {
    /// The lowest elevation cut that actually carries each requested moment.
    Lowest,
    All,
    Explicit(Vec<usize>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RadarRenderOptions {
    pub input: PathBuf,
    pub output_directory: PathBuf,
    /// Native/base moment names. An empty list requests every available base moment.
    pub moments: Vec<String>,
    pub cuts: RadarCutSelection,
    pub width: u32,
    pub height: u32,
    pub json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RadarCommand {
    Inspect { input: PathBuf, json: bool },
    Render(Box<RadarRenderOptions>),
    Verify { manifest: PathBuf },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SatelliteFrameSelection {
    Latest,
    All,
    /// UTC times encoded as HHMM integers.
    Explicit(Vec<u16>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SatelliteRenderOptions {
    pub run_directory: PathBuf,
    pub output_directory: PathBuf,
    pub frames: SatelliteFrameSelection,
    pub ir_enhancement: String,
    pub width: u32,
    pub height: u32,
    pub json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SatelliteCommand {
    Inspect { run_directory: PathBuf, json: bool },
    Render(Box<SatelliteRenderOptions>),
    Verify { manifest: PathBuf },
}

/// Extensible root command.  Future radar, satellite, sounding, and Formula
/// Lab families should be siblings of `Wrf`, not special cases in `main.rs`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CliCommand {
    Help(HelpTopic),
    Version,
    Wrf(WrfCommand),
    Radar(RadarCommand),
    Satellite(SatelliteCommand),
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

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: ExitCode::Unavailable,
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
    concat!(
        "BowEcho desktop weather analysis and local post-processing\n\n",
        "Usage:\n",
        "  bowecho [FILE]\n",
        "  bowecho wrf <COMMAND> [OPTIONS]\n",
        "  bowecho radar <COMMAND> [OPTIONS]\n",
        "  bowecho satellite <COMMAND> [OPTIONS]\n\n",
        "Command families:\n",
        "  wrf       Inspect, render, watch, and verify WRF/GPUWM data\n",
        "  radar     Inspect, render, and verify Level-II radar data\n",
        "  satellite Inspect, render, and verify canonical satellite-store runs\n\n",
        "Global options:\n",
        "  -h, --help       Show this help\n",
        "  -V, --version    Show the BowEcho version\n\n",
        "Without a command family, BowEcho starts the desktop app and optionally opens FILE.\n",
    )
}

pub const fn wrf_help_text() -> &'static str {
    "BowEcho WRF/GPUWM local post-processing\n\n\
Usage:\n  bowecho wrf inspect <FILE-OR-DIRECTORY> --json\n  bowecho wrf render <INPUT>... --preset severe --out <DIRECTORY> --run-manifest <RUN.JSON> [OPTIONS]\n  bowecho wrf watch <DIRECTORY> --preset severe --out <DIRECTORY> --run-manifest <RUN.JSON> [OPTIONS]\n  bowecho wrf verify <ARTIFACT-MANIFEST.JSON>\n\n\
Commands:\n  inspect    Report WRF grid, projection, times, variables, metadata, and source receipts\n  render     Render a deterministic batch through the BowEcho application host\n  watch      Process complete wrfout files idempotently as they arrive\n  verify     Recalculate every source/artifact size and SHA-256 receipt\n\n\
Source wrfout files are read-only. BowEcho never uploads or deletes them.\n"
}

pub const fn radar_help_text() -> &'static str {
    "BowEcho Level-II radar inspection and deterministic rendering\n\n\
Usage:\n  bowecho radar inspect <FILE> --json\n  bowecho radar render <FILE> --out <DIRECTORY> [OPTIONS]\n  bowecho radar verify <ARTIFACT-MANIFEST.JSON>\n\n\
Commands:\n  inspect    Report radar metadata, scans, cuts, and available moments\n  render     Render moments and cuts through BowEcho's production polar renderer\n  verify     Recalculate every source/artifact size and SHA-256 receipt\n\n\
Source radar files are read-only. BowEcho never uploads or deletes them.\n"
}

pub const fn satellite_help_text() -> &'static str {
    "BowEcho satellite rw-store inspection and deterministic rendering\n\n\
Usage:\n  bowecho satellite inspect <RW-STORE-RUN-DIRECTORY> --json\n  bowecho satellite render <RW-STORE-RUN-DIRECTORY> --out <DIRECTORY> [OPTIONS]\n  bowecho satellite verify <ARTIFACT-MANIFEST.JSON>\n\n\
Commands:\n  inspect    Report stored frames, products, grids, times, and source receipts\n  render     Render frames through BowEcho's production native plotter\n  verify     Recalculate every source/artifact size and SHA-256 receipt\n\n\
Source satellite runs are read-only. BowEcho never uploads or deletes them.\n"
}

const WRF_INSPECT_HELP: &str = "Usage: bowecho wrf inspect <FILE-OR-DIRECTORY> --json\n\n\
Recursively inspects deterministic NetCDF/HDF5 candidates. JSON is written to stdout;\n\
diagnostics are written to stderr. Source files are never modified.\n";

const WRF_RENDER_HELP: &str = "Usage: bowecho wrf render <INPUT>... --preset severe --out <DIRECTORY> --run-manifest <RUN.JSON> [OPTIONS]\n\n\
INPUT may be one wrfout, multiple files, a directory, or a quoted safe glob.\n\
Options:\n  --preset severe        Product preset (default: severe)\n  --out <DIRECTORY>      Artifact output root (required)\n  --run-manifest <FILE>  bowecho.wrf.run.v1 manifest (required)\n  --workers <N>          Parallel workers, 1-256 (default: logical CPUs)\n  --variable <NAME>      Also render a generic native variable; repeatable\n  --json                 Write the final artifact manifest to stdout\n  -h, --help             Show this help\n\n\
Use -- before dash-prefixed input paths. Source wrfout files are read-only.\n";

const WRF_WATCH_HELP: &str = "Usage: bowecho wrf watch <DIRECTORY> --preset severe --out <DIRECTORY> --run-manifest <RUN.JSON> [OPTIONS]\n\n\
Options:\n  --preset severe              Product preset (default: severe)\n  --out <DIRECTORY>            Artifact output root (required)\n  --run-manifest <FILE>        bowecho.wrf.run.v1 manifest (required)\n  --workers <N>                Parallel workers, 1-256 (default: logical CPUs)\n  --variable <NAME>            Also render a generic native variable; repeatable\n  --stable-seconds <N>         Unchanged size+mtime window, 1-3600 (default: 30)\n  --poll-seconds <N>           Directory poll interval, 1-3600 (default: 5)\n  --completion-marker <SUFFIX> Explicit producer marker suffix (default: .complete)\n  --journal <FILE>             Atomic resume journal path (default: under output run)\n  --jsonl                      Stream machine-readable events to stdout\n  -h, --help                   Show this help\n\n\
A marker permits an immediate readability attempt but never bypasses the NetCDF proof.\n";

const WRF_VERIFY_HELP: &str = "Usage: bowecho wrf verify <ARTIFACT-MANIFEST.JSON>\n\n\
Recalculates byte counts and SHA-256 hashes for every declared source and artifact.\n\
A machine-readable verification report is written to stdout.\n";

const RADAR_INSPECT_HELP: &str = "Usage: bowecho radar inspect <FILE> --json\n\n\
Reports Level-II volume metadata, scans, cuts, and available base moments. JSON is\n\
written to stdout; diagnostics are written to stderr. The source is never modified.\n";

const RADAR_RENDER_HELP: &str = "Usage: bowecho radar render <FILE> --out <DIRECTORY> [OPTIONS]\n\n\
Options:\n  --out <DIRECTORY>  Artifact output root (required)\n  --moment <NAME>    Product ID; repeatable (default: available REF/VEL/SW/ZDR/RHO/PHI/KDP)\n  --cut-index <N>    Zero-based elevation-cut index, 0-255; repeatable\n  --all-cuts         Render every available elevation cut\n  --width <PX>       Image width, 64-4096 (default: 1200)\n  --height <PX>      Image height, 64-4096 (default: 1200)\n  --json             Write the final artifact manifest to stdout\n  -h, --help         Show this help\n\n\
Without a cut option, each moment uses its lowest compatible cut. --all-cuts and\n\
--cut-index are mutually exclusive. DVEL and CREF are available as explicit derived\n\
product IDs. Use -- before a dash-prefixed path.\n";

const RADAR_VERIFY_HELP: &str = "Usage: bowecho radar verify <ARTIFACT-MANIFEST.JSON> [--json]\n\n\
Recalculates byte counts and SHA-256 hashes for every declared source and artifact.\n\
A machine-readable verification report is written to stdout.\n";

const SATELLITE_INSPECT_HELP: &str = "Usage: bowecho satellite inspect <RW-STORE-RUN-DIRECTORY> --json\n\n\
Reports stored frames, products, grids, times, and source receipts. JSON is written\n\
to stdout; diagnostics are written to stderr. The source run is never modified.\n";

const SATELLITE_RENDER_HELP: &str = "Usage: bowecho satellite render <RW-STORE-RUN-DIRECTORY> --out <DIRECTORY> [OPTIONS]\n\n\
Options:\n  --out <DIRECTORY>       Artifact output root (required)\n  --frame <SELECTOR>      latest, all, or UTC HHMM; repeat HHMM to select several (default: latest)\n  --ir-enhancement <NAME> natural, cimss, bd, avn, funktop, rainbow, or gray (default: cimss)\n  --width <PX>            Image width, 64-4096 (default: 1600)\n  --height <PX>           Image height, 64-4096 (default: 1200)\n  --json                  Write the final artifact manifest to stdout\n  -h, --help              Show this help\n\n\
latest/all cannot be combined with each other or HHMM selectors. Use -- before a\n\
dash-prefixed run-directory path.\n";

const SATELLITE_VERIFY_HELP: &str = "Usage: bowecho satellite verify <ARTIFACT-MANIFEST.JSON> [--json]\n\n\
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
    if first == "radar" {
        return parse_radar(args.collect()).map(Invocation::Cli);
    }
    if first == "satellite" {
        return parse_satellite(args.collect()).map(Invocation::Cli);
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
        Some("render") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::WrfRender))
        }
        Some("watch") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::WrfWatch))
        }
        Some("verify") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::WrfVerify))
        }
        Some("inspect") => parse_inspect(&args[1..]).map(CliCommand::Wrf),
        Some("render") => parse_render(&args[1..]).map(CliCommand::Wrf),
        Some("watch") => parse_watch(&args[1..]).map(CliCommand::Wrf),
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

fn parse_radar(args: Vec<OsString>) -> Result<CliCommand, CliError> {
    let Some(subcommand) = args.first() else {
        return Err(CliError::usage("missing radar command", radar_help_text()));
    };
    if subcommand == "help" || subcommand == "--help" || subcommand == "-h" {
        return Ok(CliCommand::Help(HelpTopic::Radar));
    }
    match subcommand.to_str() {
        Some("inspect") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::RadarInspect))
        }
        Some("render") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::RadarRender))
        }
        Some("verify") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::RadarVerify))
        }
        Some("inspect") => {
            let (input, json) = parse_single_inspect_target(
                &args[1..],
                RADAR_INSPECT_HELP,
                "radar inspect",
                "a radar file",
            )?;
            Ok(CliCommand::Radar(RadarCommand::Inspect { input, json }))
        }
        Some("render") => parse_radar_render(&args[1..]).map(CliCommand::Radar),
        Some("verify") => parse_verify_manifest(&args[1..], RADAR_VERIFY_HELP, "radar")
            .map(|manifest| CliCommand::Radar(RadarCommand::Verify { manifest })),
        Some(other) => Err(CliError::usage(
            format!("unknown radar command '{other}'"),
            radar_help_text(),
        )),
        None => Err(CliError::usage(
            "radar command is not valid Unicode",
            radar_help_text(),
        )),
    }
}

fn parse_satellite(args: Vec<OsString>) -> Result<CliCommand, CliError> {
    let Some(subcommand) = args.first() else {
        return Err(CliError::usage(
            "missing satellite command",
            satellite_help_text(),
        ));
    };
    if subcommand == "help" || subcommand == "--help" || subcommand == "-h" {
        return Ok(CliCommand::Help(HelpTopic::Satellite));
    }
    match subcommand.to_str() {
        Some("inspect") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::SatelliteInspect))
        }
        Some("render") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::SatelliteRender))
        }
        Some("verify") if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") => {
            Ok(CliCommand::Help(HelpTopic::SatelliteVerify))
        }
        Some("inspect") => {
            let (run_directory, json) = parse_single_inspect_target(
                &args[1..],
                SATELLITE_INSPECT_HELP,
                "satellite inspect",
                "an rw-store run directory",
            )?;
            Ok(CliCommand::Satellite(SatelliteCommand::Inspect {
                run_directory,
                json,
            }))
        }
        Some("render") => parse_satellite_render(&args[1..]).map(CliCommand::Satellite),
        Some("verify") => parse_verify_manifest(&args[1..], SATELLITE_VERIFY_HELP, "satellite")
            .map(|manifest| CliCommand::Satellite(SatelliteCommand::Verify { manifest })),
        Some(other) => Err(CliError::usage(
            format!("unknown satellite command '{other}'"),
            satellite_help_text(),
        )),
        None => Err(CliError::usage(
            "satellite command is not valid Unicode",
            satellite_help_text(),
        )),
    }
}

fn parse_single_inspect_target(
    args: &[OsString],
    help: &'static str,
    command_name: &str,
    target_description: &str,
) -> Result<(PathBuf, bool), CliError> {
    let mut target = None;
    let mut json = false;
    let mut positional_only = false;
    for arg in args {
        if !positional_only && arg == "--" {
            positional_only = true;
        } else if !positional_only && arg == "--json" {
            json = true;
        } else if !positional_only && arg.to_string_lossy().starts_with('-') {
            return Err(CliError::usage(
                format!("unknown {command_name} option '{}'", arg.to_string_lossy()),
                help,
            ));
        } else if target.replace(PathBuf::from(arg)).is_some() {
            return Err(CliError::usage(
                format!("{command_name} accepts exactly one target"),
                help,
            ));
        }
    }
    let target = target.ok_or_else(|| {
        CliError::usage(
            format!("{command_name} requires {target_description}"),
            help,
        )
    })?;
    Ok((target, json))
}

fn parse_radar_render(args: &[OsString]) -> Result<RadarCommand, CliError> {
    let mut input = None;
    let mut output_directory = None;
    let mut moments = Vec::new();
    let mut cut_indices = Vec::new();
    let mut all_cuts = false;
    let mut width = None;
    let mut height = None;
    let mut json = false;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        if !positional_only && arg == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only {
            match arg.to_str() {
                Some("--out") => {
                    let value = option_value(args, &mut index, "--out", RADAR_RENDER_HELP)?;
                    set_once(
                        &mut output_directory,
                        PathBuf::from(value),
                        "--out",
                        RADAR_RENDER_HELP,
                    )?;
                }
                Some("--moment") => {
                    let value = option_value(args, &mut index, "--moment", RADAR_RENDER_HELP)?;
                    let moment = parse_radar_moment(value)?;
                    if !moments.contains(&moment) {
                        moments.push(moment);
                    }
                }
                Some("--cut-index") => {
                    let value = option_value(args, &mut index, "--cut-index", RADAR_RENDER_HELP)?;
                    if all_cuts {
                        return Err(CliError::usage(
                            "--all-cuts and --cut-index are mutually exclusive",
                            RADAR_RENDER_HELP,
                        ));
                    }
                    let cut = parse_bounded_u64(value, "--cut-index", 0, 255, RADAR_RENDER_HELP)?;
                    let cut = usize::try_from(cut).map_err(|_| {
                        CliError::usage("--cut-index is too large", RADAR_RENDER_HELP)
                    })?;
                    cut_indices.push(cut);
                }
                Some("--all-cuts") => {
                    if all_cuts {
                        return Err(CliError::usage(
                            "--all-cuts may be supplied only once",
                            RADAR_RENDER_HELP,
                        ));
                    }
                    if !cut_indices.is_empty() {
                        return Err(CliError::usage(
                            "--all-cuts and --cut-index are mutually exclusive",
                            RADAR_RENDER_HELP,
                        ));
                    }
                    all_cuts = true;
                }
                Some("--width") => {
                    let value = option_value(args, &mut index, "--width", RADAR_RENDER_HELP)?;
                    set_once(
                        &mut width,
                        parse_dimension(value, "--width", RADAR_RENDER_HELP)?,
                        "--width",
                        RADAR_RENDER_HELP,
                    )?;
                }
                Some("--height") => {
                    let value = option_value(args, &mut index, "--height", RADAR_RENDER_HELP)?;
                    set_once(
                        &mut height,
                        parse_dimension(value, "--height", RADAR_RENDER_HELP)?,
                        "--height",
                        RADAR_RENDER_HELP,
                    )?;
                }
                Some("--json") => json = true,
                Some(value) if value.starts_with('-') => {
                    return Err(CliError::usage(
                        format!("unknown radar render option '{value}'"),
                        RADAR_RENDER_HELP,
                    ));
                }
                _ => set_once(
                    &mut input,
                    PathBuf::from(arg),
                    "radar input",
                    RADAR_RENDER_HELP,
                )?,
            }
        } else {
            set_once(
                &mut input,
                PathBuf::from(arg),
                "radar input",
                RADAR_RENDER_HELP,
            )?;
        }
        index += 1;
    }

    cut_indices.sort_unstable();
    cut_indices.dedup();
    let cuts = if all_cuts {
        RadarCutSelection::All
    } else if cut_indices.is_empty() {
        RadarCutSelection::Lowest
    } else {
        RadarCutSelection::Explicit(cut_indices)
    };
    Ok(RadarCommand::Render(Box::new(RadarRenderOptions {
        input: input
            .ok_or_else(|| CliError::usage("radar render requires one file", RADAR_RENDER_HELP))?,
        output_directory: output_directory
            .ok_or_else(|| CliError::usage("radar render requires --out", RADAR_RENDER_HELP))?,
        moments,
        cuts,
        width: width.unwrap_or(1_200),
        height: height.unwrap_or(1_200),
        json,
    })))
}

fn parse_satellite_render(args: &[OsString]) -> Result<SatelliteCommand, CliError> {
    let mut run_directory = None;
    let mut output_directory = None;
    let mut frame_keyword = None;
    let mut explicit_frames = Vec::new();
    let mut ir_enhancement = None;
    let mut width = None;
    let mut height = None;
    let mut json = false;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        if !positional_only && arg == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only {
            match arg.to_str() {
                Some("--out") => {
                    let value = option_value(args, &mut index, "--out", SATELLITE_RENDER_HELP)?;
                    set_once(
                        &mut output_directory,
                        PathBuf::from(value),
                        "--out",
                        SATELLITE_RENDER_HELP,
                    )?;
                }
                Some("--frame") => {
                    let value = option_value(args, &mut index, "--frame", SATELLITE_RENDER_HELP)?;
                    let value = value.to_str().ok_or_else(|| {
                        CliError::usage("--frame must be valid Unicode", SATELLITE_RENDER_HELP)
                    })?;
                    match value {
                        "latest" | "all" => {
                            if frame_keyword.is_some() || !explicit_frames.is_empty() {
                                return Err(CliError::usage(
                                    "latest/all cannot be combined with another --frame selector",
                                    SATELLITE_RENDER_HELP,
                                ));
                            }
                            frame_keyword = Some(if value == "latest" {
                                SatelliteFrameSelection::Latest
                            } else {
                                SatelliteFrameSelection::All
                            });
                        }
                        _ => {
                            if frame_keyword.is_some() {
                                return Err(CliError::usage(
                                    "HHMM selectors cannot be combined with latest or all",
                                    SATELLITE_RENDER_HELP,
                                ));
                            }
                            explicit_frames.push(parse_satellite_hhmm(value)?);
                        }
                    }
                }
                Some("--ir-enhancement") => {
                    let value =
                        option_value(args, &mut index, "--ir-enhancement", SATELLITE_RENDER_HELP)?;
                    set_once(
                        &mut ir_enhancement,
                        parse_satellite_ir_enhancement(value)?,
                        "--ir-enhancement",
                        SATELLITE_RENDER_HELP,
                    )?;
                }
                Some("--width") => {
                    let value = option_value(args, &mut index, "--width", SATELLITE_RENDER_HELP)?;
                    set_once(
                        &mut width,
                        parse_dimension(value, "--width", SATELLITE_RENDER_HELP)?,
                        "--width",
                        SATELLITE_RENDER_HELP,
                    )?;
                }
                Some("--height") => {
                    let value = option_value(args, &mut index, "--height", SATELLITE_RENDER_HELP)?;
                    set_once(
                        &mut height,
                        parse_dimension(value, "--height", SATELLITE_RENDER_HELP)?,
                        "--height",
                        SATELLITE_RENDER_HELP,
                    )?;
                }
                Some("--json") => json = true,
                Some(value) if value.starts_with('-') => {
                    return Err(CliError::usage(
                        format!("unknown satellite render option '{value}'"),
                        SATELLITE_RENDER_HELP,
                    ));
                }
                _ => set_once(
                    &mut run_directory,
                    PathBuf::from(arg),
                    "satellite run directory",
                    SATELLITE_RENDER_HELP,
                )?,
            }
        } else {
            set_once(
                &mut run_directory,
                PathBuf::from(arg),
                "satellite run directory",
                SATELLITE_RENDER_HELP,
            )?;
        }
        index += 1;
    }

    explicit_frames.sort_unstable();
    explicit_frames.dedup();
    let frames = match frame_keyword {
        Some(selection) => selection,
        None if explicit_frames.is_empty() => SatelliteFrameSelection::Latest,
        None => SatelliteFrameSelection::Explicit(explicit_frames),
    };
    Ok(SatelliteCommand::Render(Box::new(SatelliteRenderOptions {
        run_directory: run_directory.ok_or_else(|| {
            CliError::usage(
                "satellite render requires one rw-store run directory",
                SATELLITE_RENDER_HELP,
            )
        })?,
        output_directory: output_directory.ok_or_else(|| {
            CliError::usage("satellite render requires --out", SATELLITE_RENDER_HELP)
        })?,
        frames,
        ir_enhancement: ir_enhancement.unwrap_or_else(|| "cimss".to_owned()),
        width: width.unwrap_or(1_600),
        height: height.unwrap_or(1_200),
        json,
    })))
}

fn parse_verify_manifest(
    args: &[OsString],
    help: &'static str,
    family: &str,
) -> Result<PathBuf, CliError> {
    let mut manifest = None;
    let mut positional_only = false;
    for arg in args {
        if !positional_only && arg == "--" {
            positional_only = true;
        } else if !positional_only && arg == "--json" {
            // Accepted for symmetry; verification output is always machine-readable.
        } else if !positional_only && arg.to_string_lossy().starts_with('-') {
            return Err(CliError::usage(
                format!("unknown {family} verify option '{}'", arg.to_string_lossy()),
                help,
            ));
        } else if manifest.replace(PathBuf::from(arg)).is_some() {
            return Err(CliError::usage(
                format!("{family} verify accepts exactly one artifact manifest"),
                help,
            ));
        }
    }
    manifest.ok_or_else(|| {
        CliError::usage(
            format!("{family} verify requires an artifact manifest"),
            help,
        )
    })
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

fn parse_render(args: &[OsString]) -> Result<WrfCommand, CliError> {
    let mut inputs = Vec::new();
    let mut preset = None;
    let mut output_directory = None;
    let mut run_manifest = None;
    let mut workers = None;
    let mut variables = Vec::new();
    let mut json = false;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        if !positional_only && arg == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only {
            match arg.to_str() {
                Some("--preset") => {
                    let value = option_value(args, &mut index, "--preset", WRF_RENDER_HELP)?;
                    set_once(
                        &mut preset,
                        parse_preset(value, WRF_RENDER_HELP)?,
                        "--preset",
                        WRF_RENDER_HELP,
                    )?;
                }
                Some("--out") => {
                    let value = option_value(args, &mut index, "--out", WRF_RENDER_HELP)?;
                    set_once(
                        &mut output_directory,
                        PathBuf::from(value),
                        "--out",
                        WRF_RENDER_HELP,
                    )?;
                }
                Some("--run-manifest") => {
                    let value = option_value(args, &mut index, "--run-manifest", WRF_RENDER_HELP)?;
                    set_once(
                        &mut run_manifest,
                        PathBuf::from(value),
                        "--run-manifest",
                        WRF_RENDER_HELP,
                    )?;
                }
                Some("--workers") => {
                    let value = option_value(args, &mut index, "--workers", WRF_RENDER_HELP)?;
                    set_once(
                        &mut workers,
                        parse_workers(value, WRF_RENDER_HELP)?,
                        "--workers",
                        WRF_RENDER_HELP,
                    )?;
                }
                Some("--variable") => {
                    let value = option_value(args, &mut index, "--variable", WRF_RENDER_HELP)?;
                    let variable = parse_variable(value, WRF_RENDER_HELP)?;
                    if !variables.contains(&variable) {
                        variables.push(variable);
                    }
                }
                Some("--json") => json = true,
                Some(value) if value.starts_with('-') => {
                    return Err(CliError::usage(
                        format!("unknown render option '{value}'"),
                        WRF_RENDER_HELP,
                    ));
                }
                _ => inputs.push(arg.clone()),
            }
        } else {
            inputs.push(arg.clone());
        }
        index += 1;
    }

    let inputs = InputSet::parse(inputs).map_err(|error| CliError {
        help: Some(WRF_RENDER_HELP),
        ..error
    })?;
    let output_directory = output_directory
        .ok_or_else(|| CliError::usage("render requires --out", WRF_RENDER_HELP))?;
    let run_manifest = run_manifest
        .ok_or_else(|| CliError::usage("render requires --run-manifest", WRF_RENDER_HELP))?;
    Ok(WrfCommand::Render(Box::new(RenderOptions {
        inputs,
        preset: preset.unwrap_or(RenderPreset::Severe),
        output_directory,
        run_manifest,
        workers: workers.unwrap_or_else(default_workers),
        variables,
        json,
    })))
}

fn parse_watch(args: &[OsString]) -> Result<WrfCommand, CliError> {
    let mut directory = None;
    let mut preset = None;
    let mut output_directory = None;
    let mut run_manifest = None;
    let mut workers = None;
    let mut variables = Vec::new();
    let mut jsonl = false;
    let mut stable_seconds = None;
    let mut poll_seconds = None;
    let mut completion_marker_suffix = None;
    let mut journal = None;
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        if !positional_only && arg == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if !positional_only {
            match arg.to_str() {
                Some("--preset") => {
                    let value = option_value(args, &mut index, "--preset", WRF_WATCH_HELP)?;
                    set_once(
                        &mut preset,
                        parse_preset(value, WRF_WATCH_HELP)?,
                        "--preset",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--out") => {
                    let value = option_value(args, &mut index, "--out", WRF_WATCH_HELP)?;
                    set_once(
                        &mut output_directory,
                        PathBuf::from(value),
                        "--out",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--run-manifest") => {
                    let value = option_value(args, &mut index, "--run-manifest", WRF_WATCH_HELP)?;
                    set_once(
                        &mut run_manifest,
                        PathBuf::from(value),
                        "--run-manifest",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--workers") => {
                    let value = option_value(args, &mut index, "--workers", WRF_WATCH_HELP)?;
                    set_once(
                        &mut workers,
                        parse_workers(value, WRF_WATCH_HELP)?,
                        "--workers",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--variable") => {
                    let value = option_value(args, &mut index, "--variable", WRF_WATCH_HELP)?;
                    let variable = parse_variable(value, WRF_WATCH_HELP)?;
                    if !variables.contains(&variable) {
                        variables.push(variable);
                    }
                }
                Some("--stable-seconds") => {
                    let value = option_value(args, &mut index, "--stable-seconds", WRF_WATCH_HELP)?;
                    set_once(
                        &mut stable_seconds,
                        parse_bounded_u64(value, "--stable-seconds", 1, 3_600, WRF_WATCH_HELP)?,
                        "--stable-seconds",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--poll-seconds") => {
                    let value = option_value(args, &mut index, "--poll-seconds", WRF_WATCH_HELP)?;
                    set_once(
                        &mut poll_seconds,
                        parse_bounded_u64(value, "--poll-seconds", 1, 3_600, WRF_WATCH_HELP)?,
                        "--poll-seconds",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--completion-marker") => {
                    let value =
                        option_value(args, &mut index, "--completion-marker", WRF_WATCH_HELP)?;
                    let suffix = value
                        .to_str()
                        .ok_or_else(|| {
                            CliError::usage(
                                "--completion-marker must be valid Unicode",
                                WRF_WATCH_HELP,
                            )
                        })?
                        .to_owned();
                    set_once(
                        &mut completion_marker_suffix,
                        suffix,
                        "--completion-marker",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--journal") => {
                    let value = option_value(args, &mut index, "--journal", WRF_WATCH_HELP)?;
                    set_once(
                        &mut journal,
                        PathBuf::from(value),
                        "--journal",
                        WRF_WATCH_HELP,
                    )?;
                }
                Some("--jsonl") => jsonl = true,
                Some(value) if value.starts_with('-') => {
                    return Err(CliError::usage(
                        format!("unknown watch option '{value}'"),
                        WRF_WATCH_HELP,
                    ));
                }
                _ => set_once(
                    &mut directory,
                    PathBuf::from(arg),
                    "watch directory",
                    WRF_WATCH_HELP,
                )?,
            }
        } else {
            set_once(
                &mut directory,
                PathBuf::from(arg),
                "watch directory",
                WRF_WATCH_HELP,
            )?;
        }
        index += 1;
    }

    let readiness = WatchPolicy {
        stable_window_ms: stable_seconds.unwrap_or(30) * 1_000,
        completion_marker_suffix: completion_marker_suffix.unwrap_or_else(|| ".complete".into()),
    };
    readiness
        .validate()
        .map_err(|message| CliError::usage(message, WRF_WATCH_HELP))?;
    Ok(WrfCommand::Watch(Box::new(WatchOptions {
        directory: directory
            .ok_or_else(|| CliError::usage("watch requires one directory", WRF_WATCH_HELP))?,
        preset: preset.unwrap_or(RenderPreset::Severe),
        output_directory: output_directory
            .ok_or_else(|| CliError::usage("watch requires --out", WRF_WATCH_HELP))?,
        run_manifest: run_manifest
            .ok_or_else(|| CliError::usage("watch requires --run-manifest", WRF_WATCH_HELP))?,
        workers: workers.unwrap_or_else(default_workers),
        variables,
        jsonl,
        poll_interval_ms: poll_seconds.unwrap_or(5) * 1_000,
        readiness,
        journal,
    })))
}

fn option_value<'a>(
    args: &'a [OsString],
    index: &mut usize,
    option: &str,
    help: &'static str,
) -> Result<&'a OsString, CliError> {
    *index += 1;
    args.get(*index)
        .ok_or_else(|| CliError::usage(format!("{option} requires a value"), help))
}

fn set_once<T>(
    slot: &mut Option<T>,
    value: T,
    name: &str,
    help: &'static str,
) -> Result<(), CliError> {
    if slot.replace(value).is_some() {
        Err(CliError::usage(
            format!("{name} may be supplied only once"),
            help,
        ))
    } else {
        Ok(())
    }
}

fn parse_preset(value: &OsString, help: &'static str) -> Result<RenderPreset, CliError> {
    match value.to_str() {
        Some("severe") => Ok(RenderPreset::Severe),
        Some(value) => Err(CliError::usage(
            format!("unsupported WRF preset '{value}'; currently supported: severe"),
            help,
        )),
        None => Err(CliError::usage("--preset must be valid Unicode", help)),
    }
}

fn parse_workers(value: &OsString, help: &'static str) -> Result<usize, CliError> {
    let workers = parse_bounded_u64(value, "--workers", 1, 256, help)?;
    usize::try_from(workers).map_err(|_| CliError::usage("--workers is too large", help))
}

fn parse_bounded_u64(
    value: &OsString,
    option: &str,
    minimum: u64,
    maximum: u64,
    help: &'static str,
) -> Result<u64, CliError> {
    let text = value
        .to_str()
        .ok_or_else(|| CliError::usage(format!("{option} must be valid Unicode"), help))?;
    let parsed = text.parse::<u64>().map_err(|_| {
        CliError::usage(
            format!("{option} must be an integer from {minimum} through {maximum}"),
            help,
        )
    })?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(CliError::usage(
            format!("{option} must be from {minimum} through {maximum}"),
            help,
        ));
    }
    Ok(parsed)
}

fn parse_dimension(value: &OsString, option: &str, help: &'static str) -> Result<u32, CliError> {
    let value = parse_bounded_u64(value, option, 64, 4_096, help)?;
    u32::try_from(value).map_err(|_| CliError::usage(format!("{option} is too large"), help))
}

fn parse_radar_moment(value: &OsString) -> Result<String, CliError> {
    let value = value
        .to_str()
        .ok_or_else(|| CliError::usage("--moment must be valid Unicode", RADAR_RENDER_HELP))?
        .trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(CliError::usage(
            "--moment must contain 1-64 ASCII letters, digits, underscores, or hyphens",
            RADAR_RENDER_HELP,
        ));
    }
    Ok(value.to_ascii_uppercase())
}

fn parse_satellite_hhmm(value: &str) -> Result<u16, CliError> {
    if value.len() != 4 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CliError::usage(
            "--frame must be latest, all, or a four-digit UTC HHMM",
            SATELLITE_RENDER_HELP,
        ));
    }
    let hour = value[..2]
        .parse::<u16>()
        .map_err(|_| CliError::usage("invalid --frame hour", SATELLITE_RENDER_HELP))?;
    let minute = value[2..]
        .parse::<u16>()
        .map_err(|_| CliError::usage("invalid --frame minute", SATELLITE_RENDER_HELP))?;
    if hour > 23 || minute > 59 {
        return Err(CliError::usage(
            "--frame HHMM must contain an hour from 00-23 and minute from 00-59",
            SATELLITE_RENDER_HELP,
        ));
    }
    Ok(hour * 100 + minute)
}

fn parse_satellite_ir_enhancement(value: &OsString) -> Result<String, CliError> {
    let value = value
        .to_str()
        .ok_or_else(|| {
            CliError::usage(
                "--ir-enhancement must be valid Unicode",
                SATELLITE_RENDER_HELP,
            )
        })?
        .to_ascii_lowercase();
    match value.as_str() {
        "natural" | "cimss" | "bd" | "avn" | "funktop" | "rainbow" | "gray" => Ok(value),
        _ => Err(CliError::usage(
            "--ir-enhancement must be natural, cimss, bd, avn, funktop, rainbow, or gray",
            SATELLITE_RENDER_HELP,
        )),
    }
}

fn parse_variable(value: &OsString, help: &'static str) -> Result<String, CliError> {
    let value = value
        .to_str()
        .ok_or_else(|| CliError::usage("--variable must be valid Unicode", help))?
        .trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(CliError::usage(
            "--variable must be non-empty, at most 256 bytes, and contain no control characters",
            help,
        ));
    }
    Ok(value.to_owned())
}

fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(256)
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
                HelpTopic::WrfRender => WRF_RENDER_HELP,
                HelpTopic::WrfWatch => WRF_WATCH_HELP,
                HelpTopic::WrfVerify => WRF_VERIFY_HELP,
                HelpTopic::Radar => radar_help_text(),
                HelpTopic::RadarInspect => RADAR_INSPECT_HELP,
                HelpTopic::RadarRender => RADAR_RENDER_HELP,
                HelpTopic::RadarVerify => RADAR_VERIFY_HELP,
                HelpTopic::Satellite => satellite_help_text(),
                HelpTopic::SatelliteInspect => SATELLITE_INSPECT_HELP,
                HelpTopic::SatelliteRender => SATELLITE_RENDER_HELP,
                HelpTopic::SatelliteVerify => SATELLITE_VERIFY_HELP,
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
        CliCommand::Wrf(WrfCommand::Render(_)) => Err(CliError::unavailable(
            "WRF render must be dispatched by the BowEcho application host",
        )),
        CliCommand::Wrf(WrfCommand::Watch(_)) => Err(CliError::unavailable(
            "WRF watch must be dispatched by the BowEcho application host",
        )),
        CliCommand::Radar(command) => radar::execute(&command, context, stdout, stderr),
        CliCommand::Satellite(_) => Err(CliError::unavailable(
            "satellite commands must be dispatched by the BowEcho application host",
        )),
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
    fn parses_render_file_list_glob_and_host_ready_options() {
        let invocation = parse(&[
            "bowecho",
            "wrf",
            "render",
            "wrfout_d01",
            "run/wrfout_d02_*",
            "--preset",
            "severe",
            "--out",
            "artifacts",
            "--run-manifest",
            "run.json",
            "--workers",
            "8",
            "--variable",
            "W_UP_MAX",
            "--variable",
            "T2",
            "--json",
        ])
        .unwrap();
        let Invocation::Cli(CliCommand::Wrf(WrfCommand::Render(options))) = invocation else {
            panic!("expected WRF render command");
        };
        assert_eq!(options.inputs.specs.len(), 2);
        assert_eq!(options.preset, RenderPreset::Severe);
        assert_eq!(options.output_directory, PathBuf::from("artifacts"));
        assert_eq!(options.run_manifest, PathBuf::from("run.json"));
        assert_eq!(options.workers, 8);
        assert_eq!(options.variables, ["W_UP_MAX", "T2"]);
        assert!(options.json);
    }

    #[test]
    fn parses_watch_readiness_resume_and_stream_options() {
        let invocation = parse(&[
            "bowecho",
            "wrf",
            "watch",
            "forecast",
            "--preset",
            "severe",
            "--out",
            "artifacts",
            "--run-manifest",
            "run.json",
            "--workers",
            "4",
            "--variable",
            "QGRAUP",
            "--stable-seconds",
            "45",
            "--poll-seconds",
            "3",
            "--completion-marker",
            ".done",
            "--journal",
            "watch-state.json",
            "--jsonl",
        ])
        .unwrap();
        let Invocation::Cli(CliCommand::Wrf(WrfCommand::Watch(options))) = invocation else {
            panic!("expected WRF watch command");
        };
        assert_eq!(options.directory, PathBuf::from("forecast"));
        assert_eq!(options.output_directory, PathBuf::from("artifacts"));
        assert_eq!(options.run_manifest, PathBuf::from("run.json"));
        assert_eq!(options.workers, 4);
        assert_eq!(options.variables, ["QGRAUP"]);
        assert_eq!(options.poll_interval_ms, 3_000);
        assert_eq!(options.readiness.stable_window_ms, 45_000);
        assert_eq!(options.readiness.completion_marker_suffix, ".done");
        assert_eq!(options.journal, Some(PathBuf::from("watch-state.json")));
        assert!(options.jsonl);
    }

    #[test]
    fn rejects_ambiguous_or_out_of_range_render_and_watch_options() {
        let duplicate_out = parse(&[
            "bowecho",
            "wrf",
            "render",
            "wrfout_d01",
            "--out",
            "a",
            "--out",
            "b",
            "--run-manifest",
            "run.json",
        ])
        .unwrap_err();
        assert_eq!(duplicate_out.code, ExitCode::Usage);

        let zero_workers = parse(&[
            "bowecho",
            "wrf",
            "watch",
            "run",
            "--out",
            "out",
            "--run-manifest",
            "run.json",
            "--workers",
            "0",
        ])
        .unwrap_err();
        assert_eq!(zero_workers.code, ExitCode::Usage);

        let bad_marker = parse(&[
            "bowecho",
            "wrf",
            "watch",
            "run",
            "--out",
            "out",
            "--run-manifest",
            "run.json",
            "--completion-marker",
            "../done",
        ])
        .unwrap_err();
        assert_eq!(bad_marker.code, ExitCode::Usage);
    }

    #[test]
    fn bad_wrf_invocation_has_stable_usage_code() {
        let error = parse(&["bowecho", "wrf", "inspect", "a", "b"]).unwrap_err();
        assert_eq!(error.code, ExitCode::Usage);
        assert!(error.help.is_some());
    }

    #[test]
    fn parses_radar_inspect_and_dash_prefixed_file() {
        assert_eq!(
            parse(&[
                "bowecho",
                "radar",
                "inspect",
                "--json",
                "--",
                "-volume.ar2v"
            ])
            .unwrap(),
            Invocation::Cli(CliCommand::Radar(RadarCommand::Inspect {
                input: PathBuf::from("-volume.ar2v"),
                json: true,
            }))
        );
    }

    #[test]
    fn parses_radar_render_defaults_and_deterministic_selections() {
        assert_eq!(
            parse(&[
                "bowecho",
                "radar",
                "render",
                "volume.ar2v",
                "--out",
                "renders"
            ])
            .unwrap(),
            Invocation::Cli(CliCommand::Radar(RadarCommand::Render(Box::new(
                RadarRenderOptions {
                    input: PathBuf::from("volume.ar2v"),
                    output_directory: PathBuf::from("renders"),
                    moments: Vec::new(),
                    cuts: RadarCutSelection::Lowest,
                    width: 1_200,
                    height: 1_200,
                    json: false,
                }
            ))))
        );

        assert_eq!(
            parse(&[
                "bowecho",
                "radar",
                "render",
                "volume.ar2v",
                "--out",
                "renders",
                "--moment",
                "ref",
                "--moment",
                "ZDR",
                "--moment",
                "REF",
                "--cut-index",
                "3",
                "--cut-index",
                "1",
                "--cut-index",
                "3",
                "--width",
                "2048",
                "--height",
                "1024",
                "--json"
            ])
            .unwrap(),
            Invocation::Cli(CliCommand::Radar(RadarCommand::Render(Box::new(
                RadarRenderOptions {
                    input: PathBuf::from("volume.ar2v"),
                    output_directory: PathBuf::from("renders"),
                    moments: vec!["REF".to_owned(), "ZDR".to_owned()],
                    cuts: RadarCutSelection::Explicit(vec![1, 3]),
                    width: 2_048,
                    height: 1_024,
                    json: true,
                }
            ))))
        );

        let Invocation::Cli(CliCommand::Radar(RadarCommand::Render(options))) = parse(&[
            "bowecho",
            "radar",
            "render",
            "volume.ar2v",
            "--out",
            "renders",
            "--all-cuts",
        ])
        .unwrap() else {
            panic!("expected radar render command");
        };
        assert_eq!(options.cuts, RadarCutSelection::All);
    }

    #[test]
    fn rejects_conflicting_radar_cuts_and_bad_render_dimensions() {
        let cuts = parse(&[
            "bowecho",
            "radar",
            "render",
            "volume.ar2v",
            "--out",
            "renders",
            "--all-cuts",
            "--cut-index",
            "1",
        ])
        .unwrap_err();
        assert_eq!(cuts.code, ExitCode::Usage);

        let dimension = parse(&[
            "bowecho",
            "radar",
            "render",
            "volume.ar2v",
            "--out",
            "renders",
            "--width",
            "63",
        ])
        .unwrap_err();
        assert_eq!(dimension.code, ExitCode::Usage);

        let extra_input = parse(&[
            "bowecho", "radar", "render", "one.ar2v", "two.ar2v", "--out", "renders",
        ])
        .unwrap_err();
        assert_eq!(extra_input.code, ExitCode::Usage);
    }

    #[test]
    fn parses_satellite_inspect_and_render_defaults() {
        assert_eq!(
            parse(&["bowecho", "satellite", "inspect", "run-dir", "--json"]).unwrap(),
            Invocation::Cli(CliCommand::Satellite(SatelliteCommand::Inspect {
                run_directory: PathBuf::from("run-dir"),
                json: true,
            }))
        );
        assert_eq!(
            parse(&[
                "bowecho",
                "satellite",
                "render",
                "run-dir",
                "--out",
                "renders"
            ])
            .unwrap(),
            Invocation::Cli(CliCommand::Satellite(SatelliteCommand::Render(Box::new(
                SatelliteRenderOptions {
                    run_directory: PathBuf::from("run-dir"),
                    output_directory: PathBuf::from("renders"),
                    frames: SatelliteFrameSelection::Latest,
                    ir_enhancement: "cimss".to_owned(),
                    width: 1_600,
                    height: 1_200,
                    json: false,
                }
            ))))
        );
    }

    #[test]
    fn parses_satellite_explicit_frames_and_render_options() {
        assert_eq!(
            parse(&[
                "bowecho",
                "satellite",
                "render",
                "run-dir",
                "--out",
                "renders",
                "--frame",
                "2359",
                "--frame",
                "0015",
                "--frame",
                "0015",
                "--ir-enhancement",
                "AVN",
                "--width",
                "1920",
                "--height",
                "1080",
                "--json"
            ])
            .unwrap(),
            Invocation::Cli(CliCommand::Satellite(SatelliteCommand::Render(Box::new(
                SatelliteRenderOptions {
                    run_directory: PathBuf::from("run-dir"),
                    output_directory: PathBuf::from("renders"),
                    frames: SatelliteFrameSelection::Explicit(vec![15, 2_359]),
                    ir_enhancement: "avn".to_owned(),
                    width: 1_920,
                    height: 1_080,
                    json: true,
                }
            ))))
        );
    }

    #[test]
    fn rejects_invalid_or_mixed_satellite_frame_selectors() {
        for frame in ["900", "2360", "2400"] {
            let error = parse(&[
                "bowecho",
                "satellite",
                "render",
                "run-dir",
                "--out",
                "renders",
                "--frame",
                frame,
            ])
            .unwrap_err();
            assert_eq!(error.code, ExitCode::Usage);
        }

        let mixed = parse(&[
            "bowecho",
            "satellite",
            "render",
            "run-dir",
            "--out",
            "renders",
            "--frame",
            "latest",
            "--frame",
            "1200",
        ])
        .unwrap_err();
        assert_eq!(mixed.code, ExitCode::Usage);

        let enhancement = parse(&[
            "bowecho",
            "satellite",
            "render",
            "run-dir",
            "--out",
            "renders",
            "--ir-enhancement",
            "unknown",
        ])
        .unwrap_err();
        assert_eq!(enhancement.code, ExitCode::Usage);

        let dimension = parse(&[
            "bowecho",
            "satellite",
            "render",
            "run-dir",
            "--out",
            "renders",
            "--height",
            "4097",
        ])
        .unwrap_err();
        assert_eq!(dimension.code, ExitCode::Usage);
    }

    #[test]
    fn parses_radar_and_satellite_verify_contracts() {
        assert_eq!(
            parse(&["bowecho", "radar", "verify", "radar.json", "--json"]).unwrap(),
            Invocation::Cli(CliCommand::Radar(RadarCommand::Verify {
                manifest: PathBuf::from("radar.json"),
            }))
        );
        assert_eq!(
            parse(&["bowecho", "satellite", "verify", "satellite.json", "--json"]).unwrap(),
            Invocation::Cli(CliCommand::Satellite(SatelliteCommand::Verify {
                manifest: PathBuf::from("satellite.json"),
            }))
        );
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
        assert_eq!(
            parse(&["bowecho", "wrf", "render", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::WrfRender))
        );
        assert_eq!(
            parse(&["bowecho", "wrf", "watch", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::WrfWatch))
        );
        assert_eq!(
            parse(&["bowecho", "radar", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::Radar))
        );
        assert_eq!(
            parse(&["bowecho", "radar", "inspect", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::RadarInspect))
        );
        assert_eq!(
            parse(&["bowecho", "radar", "render", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::RadarRender))
        );
        assert_eq!(
            parse(&["bowecho", "radar", "verify", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::RadarVerify))
        );
        assert_eq!(
            parse(&["bowecho", "satellite", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::Satellite))
        );
        assert_eq!(
            parse(&["bowecho", "satellite", "inspect", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::SatelliteInspect))
        );
        assert_eq!(
            parse(&["bowecho", "satellite", "render", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::SatelliteRender))
        );
        assert_eq!(
            parse(&["bowecho", "satellite", "verify", "--help"]).unwrap(),
            Invocation::Cli(CliCommand::Help(HelpTopic::SatelliteVerify))
        );
    }
}
