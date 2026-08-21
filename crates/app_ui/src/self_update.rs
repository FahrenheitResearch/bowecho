//! Self-update: the passive release check plus native Windows and macOS
//! installers. Both pin the canonical HTTPS release asset and its SHA-256;
//! Windows then verifies Authenticode and swaps the executable, while macOS
//! verifies the signed app bundle and hands a whole-bundle swap to a private
//! helper after the eframe event loop exits.
//!
//! v0.29.4 phase-1 extraction #1 (docs/main-decomposition-plan.md): every
//! body moved VERBATIM from main.rs — the only edits are `use` lines and
//! the pub(crate) promotions listed in the extraction commit message.
//! `brand.rs` keeps sole ownership of repo-URL parsing
//! (`update_check_api_url`, `CANONICAL_REPO_URL`, …); this module calls it.

use std::path::Path;
#[cfg(any(windows, target_os = "macos", test))]
use std::path::PathBuf;
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use eframe::egui;
use ui_core::worker_slot::{SlotMessage, StreamState};

#[cfg(windows)]
use crate::{SECURITY_SIGNATURE_STATUS_TEXT, SECURITY_UNSIGNED_BUILD_TEXT};
use crate::{ViewerApp, brand};

#[path = "release_version.rs"]
mod release_version;
use release_version::newer_release_tag;

/// Fetch the latest configured GitHub release tag and return it iff it is
/// newer than the running build. Invalid/non-GitHub repository URLs disable
/// the check before this helper is called; network/parse errors stay silent.
fn fetch_newer_release_tag(api_url: &str) -> Option<String> {
    let body = data_source::fetch_text(api_url).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    newer_release_tag(tag, env!("CARGO_PKG_VERSION"))
}

pub(crate) fn security_update_status_label(
    update_available: Option<&str>,
    checking: bool,
) -> String {
    if let Some(tag) = update_available {
        format!("Update available: {tag}")
    } else if checking {
        "Checking official releases".to_owned()
    } else {
        "No newer release detected this launch".to_owned()
    }
}

// ---------------------------------------------------------------------------
// Native in-app updater (Windows and macOS release builds)
//
// User clicks Install update → background thread downloads the SAME release
// asset variant this binary was built as, plus its `.sha256`. Windows requires
// Authenticode and swaps its executable. macOS requires Developer ID,
// Gatekeeper, stable bundle/team identity, then performs a whole-app swap from
// a private sibling stage after clean shutdown. Nothing installs without the
// click; every failed check removes the private download and reports why.

/// One event from the update worker to the UI.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) enum SelfUpdateEvent {
    Progress { received: u64, total: Option<u64> },
    Verifying,
    ReadyToRelaunch,
    Failed(String),
}

/// The install worker streams progress into a [`StreamSlot`]; the slot stays
/// busy until the outcome event lands (or the worker vanishes — see
/// [`ViewerApp::poll_self_update`]'s honest-failure fallback).
impl SlotMessage for SelfUpdateEvent {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            SelfUpdateEvent::ReadyToRelaunch | SelfUpdateEvent::Failed(_)
        )
    }
}

/// UI-visible lifecycle of the in-app update install.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SelfUpdatePhase {
    Idle,
    Downloading { received: u64, total: Option<u64> },
    Verifying,
    Restarting,
    Failed(String),
}

/// Work that may only begin after eframe has exited. Windows has already
/// swapped its executable by this point. macOS deliberately has not touched
/// the running signed bundle: a helper performs the whole-bundle rename once
/// its stdin reaches EOF at process teardown.
enum SelfUpdateRelaunchPlan {
    #[cfg(windows)]
    Windows { executable: PathBuf },
    #[cfg(target_os = "macos")]
    MacOs(MacOsRelaunchPlan),
}

#[cfg(target_os = "macos")]
struct MacOsRelaunchPlan {
    current_app: PathBuf,
    staged_app: PathBuf,
    stage_root: PathBuf,
}

static SELF_UPDATE_RELAUNCH: OnceLock<SelfUpdateRelaunchPlan> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelfUpdatePlatform {
    Windows,
    MacIntel,
    MacAppleSilicon,
    BrowserOnly,
}

/// Build-time variant self-identification. The release workflow bakes the
/// exact asset name this binary ships as (`BOWECHO_UPDATE_ASSET`, e.g.
/// "bowecho-windows-x64-v3.exe") into native-install builds, so the updater can
/// only ever download the variant it is already running. Local/dev builds
/// bake nothing and CI passes an empty value to browser-only targets — both
/// yield `None`, which keeps the in-app installer hidden (deliberate safety
/// property: an unbaked binary cannot guess its own variant). The two macOS
/// architectures accept only their matching release zip; Linux stays
/// browser-only.
fn self_update_asset(
    baked: Option<&'static str>,
    platform: SelfUpdatePlatform,
) -> Option<&'static str> {
    let name = baked.map(str::trim).filter(|name| !name.is_empty())?;
    match platform {
        // Preserve the existing baked Windows variants exactly. Their asset
        // names are selected by the release matrix (baseline, v3, ARM64).
        SelfUpdatePlatform::Windows => Some(name),
        SelfUpdatePlatform::MacIntel if name == "bowecho-macos-intel.zip" => Some(name),
        SelfUpdatePlatform::MacAppleSilicon if name == "bowecho-macos-apple-silicon.zip" => {
            Some(name)
        }
        SelfUpdatePlatform::MacIntel
        | SelfUpdatePlatform::MacAppleSilicon
        | SelfUpdatePlatform::BrowserOnly => None,
    }
}

fn current_self_update_platform() -> SelfUpdatePlatform {
    if cfg!(windows) {
        SelfUpdatePlatform::Windows
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        SelfUpdatePlatform::MacIntel
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        SelfUpdatePlatform::MacAppleSilicon
    } else {
        SelfUpdatePlatform::BrowserOnly
    }
}

/// In-app installs are pinned to the canonical BowEcho repository: the
/// Authenticode gate accepts *any* trusted signer, so a Brand-Kit repo
/// override must not become an executable-replacement channel. Rebranded
/// builds keep the passive check + browser button against their own repo.
/// An EMPTY repo_url is not an override — it is the stock exe wearing
/// brand-kit assets, updating from the canonical feed like everyone else
/// (`brand::effective_repo_url`, shared with the passive check). Comparing
/// parsed owner/name instead of literal strings keeps ".git" and
/// trailing-slash spellings agreeing with the check that offered the
/// update — a literal compare left "update available" with the install
/// button silently absent.
fn self_update_repo_allowed(repo_url: &str) -> bool {
    brand::github_repo_parts(brand::effective_repo_url(repo_url))
        == brand::github_repo_parts(brand::CANONICAL_REPO_URL)
}

/// Release-asset download URL derived from the latest-release API URL that
/// the passive update check already validated (single source of GitHub repo
/// parsing stays in `brand::github_latest_release_api_url`).
fn github_release_asset_url(api_url: &str, tag: &str, asset: &str) -> Option<String> {
    let repo = api_url
        .strip_prefix("https://api.github.com/repos/")?
        .strip_suffix("/releases/latest")?;
    Some(format!(
        "https://github.com/{repo}/releases/download/{tag}/{asset}"
    ))
}

/// Parse a release `.sha256` asset into the lowercase hex digest. CI writes
/// them with `sha256sum`, whose output differs by runner: the Windows runner
/// emits binary mode (`<hex> *<name>`), Linux text mode (`<hex>  <name>`).
/// Accepts both plus a bare digest; anything that is not a 64-char hex first
/// token is `None` (the updater then refuses to install).
#[cfg(any(windows, target_os = "macos", test))]
fn parse_sha256_asset(contents: &str) -> Option<String> {
    let token = contents.split_whitespace().next()?;
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

fn apply_self_update_event(phase: &SelfUpdatePhase, event: &SelfUpdateEvent) -> SelfUpdatePhase {
    match (phase, event) {
        // Terminal states: a stray late event must not resurrect the
        // progress UI after the outcome is decided.
        (SelfUpdatePhase::Restarting, _) => SelfUpdatePhase::Restarting,
        (SelfUpdatePhase::Failed(reason), _) => SelfUpdatePhase::Failed(reason.clone()),
        (_, SelfUpdateEvent::Progress { received, total }) => SelfUpdatePhase::Downloading {
            received: *received,
            total: *total,
        },
        (_, SelfUpdateEvent::Verifying) => SelfUpdatePhase::Verifying,
        (_, SelfUpdateEvent::ReadyToRelaunch) => SelfUpdatePhase::Restarting,
        (_, SelfUpdateEvent::Failed(reason)) => SelfUpdatePhase::Failed(reason.clone()),
    }
}

fn self_update_phase_label(phase: &SelfUpdatePhase) -> Option<String> {
    match phase {
        SelfUpdatePhase::Idle => None,
        SelfUpdatePhase::Downloading { received, total } => Some(match total {
            Some(total) if *total > 0 => format!(
                "Downloading update — {}%",
                (received.saturating_mul(100) / total).min(100)
            ),
            _ => format!(
                "Downloading update — {:.1} MB",
                *received as f64 / (1024.0 * 1024.0)
            ),
        }),
        SelfUpdatePhase::Verifying => {
            let signature = if cfg!(target_os = "macos") {
                "Developer ID signature + Gatekeeper"
            } else {
                "Authenticode signature"
            };
            Some(format!(
                "Verifying download (SHA-256 checksum + {signature})"
            ))
        }
        SelfUpdatePhase::Restarting => Some("Update installed — restarting".to_owned()),
        SelfUpdatePhase::Failed(reason) => Some(format!("Update failed: {reason}")),
    }
}

/// `bowecho.exe` → `bowecho.exe.old`: where the running executable is parked
/// during the swap (renaming a running exe is legal on Windows; deleting it
/// is not).
#[cfg(any(windows, test))]
fn update_backup_path(exe: &Path) -> PathBuf {
    sibling_with_suffix(exe, ".old")
}

/// `bowecho.exe` → `bowecho.exe.update`: the download target, deliberately
/// next to the executable so the final rename never crosses volumes.
#[cfg(any(windows, test))]
fn update_download_path(exe: &Path) -> PathBuf {
    sibling_with_suffix(exe, ".update")
}

#[cfg(any(windows, test))]
fn sibling_with_suffix(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Everything an interrupted or completed update may leave next to the
/// executable: the parked previous binary and a dead partial download.
#[cfg(any(windows, test))]
fn stale_update_artifacts(exe: &Path) -> [PathBuf; 2] {
    [update_backup_path(exe), update_download_path(exe)]
}

#[cfg(any(target_os = "macos", test))]
const MACOS_APP_NAME: &str = "BowEcho.app";
#[cfg(any(target_os = "macos", test))]
const MACOS_EXECUTABLE_NAME: &str = "bowecho";
#[cfg(any(target_os = "macos", test))]
const MACOS_BUNDLE_IDENTIFIER: &str = "research.fahrenheit.bowecho";
#[cfg(any(target_os = "macos", test))]
const MACOS_STAGE_PREFIX: &str = ".bowecho-update-stage-";
#[cfg(any(target_os = "macos", test))]
const MACOS_STAGE_SENTINEL: &str = ".bowecho-private-update-stage-v1";
#[cfg(any(target_os = "macos", test))]
const MACOS_STAGE_SENTINEL_CONTENTS: &str = "BowEcho private update stage v1\n";
#[cfg(any(target_os = "macos", test))]
const MACOS_PREVIOUS_DIRECTORY: &str = "previous";
#[cfg(target_os = "macos")]
const MACOS_HELPER_ARGUMENT: &str = "--bowecho-private-macos-update-helper-v1";

/// Derive the signed app root from the only accepted executable layout:
/// `BowEcho.app/Contents/MacOS/bowecho`. No ancestor search or arbitrary
/// `.app` guess is allowed.
#[cfg(any(target_os = "macos", test))]
fn macos_app_bundle_from_executable(executable: &Path) -> Result<PathBuf, String> {
    if executable.file_name() != Some(std::ffi::OsStr::new(MACOS_EXECUTABLE_NAME)) {
        return Err("the running executable is not BowEcho.app/Contents/MacOS/bowecho".to_owned());
    }
    let macos = executable
        .parent()
        .filter(|path| path.file_name() == Some(std::ffi::OsStr::new("MacOS")))
        .ok_or_else(|| "the running executable is not inside Contents/MacOS".to_owned())?;
    let contents = macos
        .parent()
        .filter(|path| path.file_name() == Some(std::ffi::OsStr::new("Contents")))
        .ok_or_else(|| "the running executable is not inside BowEcho.app/Contents".to_owned())?;
    let app = contents
        .parent()
        .filter(|path| path.file_name() == Some(std::ffi::OsStr::new(MACOS_APP_NAME)))
        .ok_or_else(|| {
            "the running executable is not inside an exact BowEcho.app bundle".to_owned()
        })?;
    Ok(app.to_path_buf())
}

#[cfg(any(target_os = "macos", test))]
fn macos_app_executable(app: &Path) -> PathBuf {
    app.join("Contents")
        .join("MacOS")
        .join(MACOS_EXECUTABLE_NAME)
}

#[cfg(any(target_os = "macos", test))]
fn macos_backup_path(stage: &Path) -> PathBuf {
    stage.join(MACOS_PREVIOUS_DIRECTORY).join(MACOS_APP_NAME)
}

#[cfg(any(target_os = "macos", test))]
fn path_has_app_translocation_component(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == std::ffi::OsStr::new("AppTranslocation"))
}

#[cfg(any(target_os = "macos", test))]
fn has_exact_macos_bundle_layout(app: &Path) -> bool {
    let real_directory = |path: &Path| {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    };
    let real_file = |path: &Path| {
        std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    };
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    real_directory(app)
        && real_directory(&contents)
        && real_directory(&macos)
        && real_file(&contents.join("Info.plist"))
        && real_file(&macos.join(MACOS_EXECUTABLE_NAME))
}

#[cfg(any(target_os = "macos", test))]
fn is_private_macos_stage(stage: &Path) -> bool {
    if !stage
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with(MACOS_STAGE_PREFIX))
    {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(stage) else {
        return false;
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return false;
    }
    let sentinel = stage.join(MACOS_STAGE_SENTINEL);
    let Ok(sentinel_metadata) = std::fs::symlink_metadata(&sentinel) else {
        return false;
    };
    sentinel_metadata.is_file()
        && !sentinel_metadata.file_type().is_symlink()
        && std::fs::read_to_string(sentinel)
            .is_ok_and(|contents| contents == MACOS_STAGE_SENTINEL_CONTENTS)
}

#[cfg(any(target_os = "macos", test))]
fn private_macos_stage_has_valid_previous_app(stage: &Path) -> bool {
    is_private_macos_stage(stage) && has_exact_macos_bundle_layout(&macos_backup_path(stage))
}

/// Startup may discard an interrupted download, but a completed update's
/// last working app remains available until the user explicitly begins the
/// next update.
#[cfg(any(target_os = "macos", test))]
fn private_macos_stage_is_abandoned(stage: &Path) -> bool {
    is_private_macos_stage(stage) && !private_macos_stage_has_valid_previous_app(stage)
}

#[cfg(any(target_os = "macos", test))]
fn remove_private_macos_stage(stage: &Path) -> Result<(), String> {
    if !is_private_macos_stage(stage) {
        return Err("refusing to recursively remove an unmarked update stage".to_owned());
    }
    std::fs::remove_dir_all(stage)
        .map_err(|err| format!("could not remove the private update stage: {err}"))
}

/// Exact extraction contract for release zips: the extraction root contains
/// one directory named `BowEcho.app`, with the expected plist and executable.
/// Rejecting extra top-level entries also rejects nested/double-wrapped zips.
#[cfg(any(target_os = "macos", test))]
fn exact_extracted_macos_app(extraction_root: &Path) -> Result<PathBuf, String> {
    let mut entries = std::fs::read_dir(extraction_root)
        .map_err(|err| format!("could not inspect the extracted update: {err}"))?;
    let entry = entries
        .next()
        .transpose()
        .map_err(|err| format!("could not inspect the extracted update: {err}"))?
        .ok_or_else(|| "the update zip extracted no BowEcho.app".to_owned())?;
    if entries.next().is_some() || entry.file_name() != std::ffi::OsStr::new(MACOS_APP_NAME) {
        return Err("the update zip must contain exactly one top-level BowEcho.app".to_owned());
    }
    let file_type = entry
        .file_type()
        .map_err(|err| format!("could not inspect extracted BowEcho.app: {err}"))?;
    if !file_type.is_dir() || file_type.is_symlink() {
        return Err("the extracted BowEcho.app is not a real directory".to_owned());
    }
    let app = entry.path();
    if !has_exact_macos_bundle_layout(&app) {
        return Err(
            "the extracted app is missing Contents/Info.plist or Contents/MacOS/bowecho".to_owned(),
        );
    }
    Ok(app)
}

/// Rename a complete staged app into place, rolling the old app back if the
/// second rename fails. The helper, not the GUI worker, calls this on macOS.
#[cfg(any(target_os = "macos", test))]
fn swap_macos_app_bundle(
    staged_app: &Path,
    current_app: &Path,
    stage_root: &Path,
) -> Result<PathBuf, String> {
    if !is_private_macos_stage(stage_root) {
        return Err("refusing to place a backup in an unmarked update stage".to_owned());
    }
    if !has_exact_macos_bundle_layout(staged_app) || !has_exact_macos_bundle_layout(current_app) {
        return Err("the current or staged app does not have the exact BowEcho layout".to_owned());
    }
    let backup_app = macos_backup_path(stage_root);
    let previous_directory = backup_app
        .parent()
        .expect("the stage-local backup always has a previous directory");
    if previous_directory.exists() {
        return Err("the private update stage already contains a previous-app area".to_owned());
    }
    std::fs::create_dir(previous_directory)
        .map_err(|err| format!("could not create the private previous-app area: {err}"))?;
    std::fs::rename(current_app, &backup_app)
        .map_err(|err| format!("could not move the current BowEcho.app aside: {err}"))?;
    if let Err(err) = std::fs::rename(staged_app, current_app) {
        return Err(match std::fs::rename(&backup_app, current_app) {
            Ok(()) => format!(
                "could not install the new BowEcho.app ({err}); the previous app was restored"
            ),
            Err(rollback) => format!(
                "could not install the new BowEcho.app ({err}) and restoring the previous app \
                 failed ({rollback}); reinstall BowEcho from the releases page"
            ),
        });
    }
    Ok(backup_app)
}

/// Best-effort startup sweep of update leftovers. Windows retries deletion
/// of the parked executable. macOS removes only sentinel-marked interrupted
/// stages; a stage containing the last working app is deliberately preserved
/// until a later explicit update attempt.
pub(crate) fn cleanup_stale_update_artifacts() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = &exe;

    #[cfg(windows)]
    for path in stale_update_artifacts(&exe) {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }

    #[cfg(target_os = "macos")]
    if let Ok(app) = macos_app_bundle_from_executable(&exe)
        && let Some(parent) = app.parent()
    {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(MACOS_STAGE_PREFIX)
                    && private_macos_stage_is_abandoned(&entry.path())
                {
                    let _ = remove_private_macos_stage(&entry.path());
                }
            }
        }
    }
}

/// Spawn the platform relaunch after `eframe::run_native` returned. Windows
/// starts the already-swapped executable. macOS starts the current signed
/// BowEcho binary in private helper mode and deliberately leaks the stdin
/// writer: EOF therefore arrives only when this process actually terminates,
/// at which point the helper can safely rename the whole app bundle.
pub(crate) fn relaunch_after_self_update() {
    let Some(plan) = SELF_UPDATE_RELAUNCH.get() else {
        return;
    };

    #[cfg(windows)]
    let SelfUpdateRelaunchPlan::Windows { executable } = plan;
    #[cfg(windows)]
    if let Err(err) = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .spawn()
    {
        eprintln!(
            "Failed to relaunch {} after update: {err}",
            executable.display()
        );
    }

    #[cfg(target_os = "macos")]
    let SelfUpdateRelaunchPlan::MacOs(plan) = plan;
    #[cfg(target_os = "macos")]
    {
        use std::process::Stdio;

        let helper = macos_app_executable(&plan.current_app);
        let mut command = std::process::Command::new(&helper);
        command
            .arg(MACOS_HELPER_ARGUMENT)
            .arg(&plan.stage_root)
            .arg(&plan.staged_app)
            .arg("--")
            .args(std::env::args_os().skip(1))
            .stdin(Stdio::piped());
        match command.spawn() {
            Ok(mut child) => {
                // Keep the pipe open until the OS tears this process down.
                // Dropping it here would let the helper race process exit.
                if let Some(stdin) = child.stdin.take() {
                    std::mem::forget(stdin);
                }
            }
            Err(err) => eprintln!(
                "Failed to start the BowEcho macOS update helper {}: {err}",
                helper.display()
            ),
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = plan;
}

#[cfg(target_os = "macos")]
struct MacOsHelperRequest {
    stage_root: PathBuf,
    staged_app: PathBuf,
    original_args: Vec<std::ffi::OsString>,
}

#[cfg(target_os = "macos")]
fn parse_macos_helper_request() -> Result<Option<MacOsHelperRequest>, String> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(MACOS_HELPER_ARGUMENT)) {
        return Ok(None);
    }
    let stage_root = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "macOS update helper is missing its private stage path".to_owned())?;
    let staged_app = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| "macOS update helper is missing its staged app path".to_owned())?;
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--")) {
        return Err("macOS update helper request is malformed".to_owned());
    }
    Ok(Some(MacOsHelperRequest {
        stage_root,
        staged_app,
        original_args: args.collect(),
    }))
}

/// Intercept the private macOS whole-bundle swap mode before logging, settings,
/// or GUI startup. The helper waits for stdin EOF, which the parent arranges
/// to occur only when its process terminates. Off macOS this is always false.
#[cfg(not(target_os = "macos"))]
pub(crate) fn run_macos_update_helper_if_requested() -> bool {
    false
}

#[cfg(target_os = "macos")]
pub(crate) fn run_macos_update_helper_if_requested() -> bool {
    let request = match parse_macos_helper_request() {
        Ok(None) => return false,
        Ok(Some(request)) => request,
        Err(reason) => {
            eprintln!("BowEcho macOS update helper rejected its request: {reason}");
            return true;
        }
    };
    if let Err(reason) = run_macos_update_helper(request) {
        eprintln!("BowEcho macOS update helper failed: {reason}");
    }
    true
}

#[cfg(target_os = "macos")]
fn run_macos_update_helper(request: MacOsHelperRequest) -> Result<(), String> {
    use std::io::Read as _;

    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|err| format!("could not locate the update helper executable: {err}"))?;
    let current_app = macos_app_bundle_from_executable(&executable)?;
    if path_has_app_translocation_component(&current_app) {
        return Err("updates cannot run from macOS App Translocation".to_owned());
    }
    let current_parent = current_app
        .parent()
        .ok_or_else(|| "BowEcho.app has no parent directory".to_owned())?
        .canonicalize()
        .map_err(|err| format!("could not resolve the app directory: {err}"))?;
    let stage_root = request
        .stage_root
        .canonicalize()
        .map_err(|err| format!("could not resolve the private update stage: {err}"))?;
    if stage_root.parent() != Some(current_parent.as_path())
        || !stage_root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(MACOS_STAGE_PREFIX))
    {
        return Err("the private update stage is not a sibling of BowEcho.app".to_owned());
    }
    if !is_private_macos_stage(&stage_root) {
        return Err("the private update stage is missing its trusted sentinel".to_owned());
    }
    let staged_app = request
        .staged_app
        .canonicalize()
        .map_err(|err| format!("could not resolve staged BowEcho.app: {err}"))?;
    let expected_staged_app = stage_root
        .join("extracted")
        .join(MACOS_APP_NAME)
        .canonicalize()
        .map_err(|err| format!("could not resolve the expected staged app: {err}"))?;
    if staged_app != expected_staged_app {
        return Err("the staged app is outside the private extraction directory".to_owned());
    }
    let exact_staged_app = exact_extracted_macos_app(&stage_root.join("extracted"))?
        .canonicalize()
        .map_err(|err| format!("could not resolve extracted BowEcho.app: {err}"))?;
    if exact_staged_app != staged_app {
        return Err("the extracted update does not have the exact BowEcho.app layout".to_owned());
    }

    // The parent deliberately keeps the write end alive until process exit.
    // Read and discard without an allocation until the OS closes that pipe.
    let mut stdin = std::io::stdin().lock();
    let mut byte = [0_u8; 1];
    while stdin
        .read(&mut byte)
        .map_err(|err| format!("could not synchronize with the exiting app: {err}"))?
        != 0
    {}

    // Re-run the trust and architecture gates in the helper immediately
    // before the rename, closing the worker/helper time-of-check gap.
    if let Err(reason) = verify_macos_bundle_pair(&current_app, &staged_app)
        .and_then(|()| verify_macos_bundle_architecture(&staged_app))
    {
        let reopen = open_macos_app(&current_app, &request.original_args);
        return Err(format!(
            "the final update verification failed ({reason}); reopening the unchanged app \
             returned {reopen:?}"
        ));
    }
    let backup_app = match swap_macos_app_bundle(&staged_app, &current_app, &stage_root) {
        Ok(backup_app) => backup_app,
        Err(reason) => {
            let reopen = open_macos_app(&current_app, &request.original_args);
            return Err(format!(
                "the app-bundle swap failed ({reason}); reopening the current app returned \
                 {reopen:?}"
            ));
        }
    };
    if let Err(relaunch) = open_macos_app(&current_app, &request.original_args) {
        rollback_macos_app_after_relaunch_failure(
            &current_app,
            &staged_app,
            &backup_app,
            &request.original_args,
            &relaunch,
        )?;
        return Err(format!(
            "the updated app could not relaunch ({relaunch}); the previous app was restored"
        ));
    }
    // Keep the sentinel-marked stage: it now owns the last known-working app.
    // A later explicit update attempt prunes it immediately before creating a
    // fresh stage, so at most one previous app persists.
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_macos_app(app: &Path, original_args: &[std::ffi::OsString]) -> Result<(), String> {
    let mut open = std::process::Command::new("/usr/bin/open");
    open.arg("-n").arg(app);
    if !original_args.is_empty() {
        open.arg("--args").args(original_args);
    }
    let status = open
        .status()
        .map_err(|err| format!("could not run /usr/bin/open: {err}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("/usr/bin/open exited with {status}"))
}

#[cfg(target_os = "macos")]
fn rollback_macos_app_after_relaunch_failure(
    current_app: &Path,
    staged_app: &Path,
    backup_app: &Path,
    original_args: &[std::ffi::OsString],
    relaunch_reason: &str,
) -> Result<(), String> {
    if !has_exact_macos_bundle_layout(current_app) || !has_exact_macos_bundle_layout(backup_app) {
        return Err(format!(
            "updated app could not relaunch ({relaunch_reason}); rollback was refused because \
             the installed app or backup no longer has the exact BowEcho layout. The previous \
             app remains at {}",
            backup_app.display()
        ));
    }
    std::fs::rename(current_app, staged_app).map_err(|err| {
        format!(
            "updated app could not relaunch ({relaunch_reason}) and could not be moved back to \
             its private stage ({err}); the previous app remains at {}",
            backup_app.display()
        )
    })?;
    if let Err(restore) = std::fs::rename(backup_app, current_app) {
        let reinstall = std::fs::rename(staged_app, current_app);
        return Err(format!(
            "updated app could not relaunch ({relaunch_reason}) and restoring the previous app \
             failed ({restore}); restoring the update in place returned {reinstall:?}. The \
             previous app remains at {}",
            backup_app.display()
        ));
    }
    if let Err(old_relaunch) = open_macos_app(current_app, original_args) {
        return Err(format!(
            "updated app could not relaunch ({relaunch_reason}); the previous app was restored \
             but /usr/bin/open also failed for it ({old_relaunch})"
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct MacOsCodeIdentity {
    identifier: String,
    team_identifier: String,
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_codesign_identity(output: &str) -> Result<MacOsCodeIdentity, String> {
    let field = |prefix: &str| {
        output
            .lines()
            .filter_map(|line| line.trim().strip_prefix(prefix))
            .map(str::trim)
            .find(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let identifier = field("Identifier=")
        .ok_or_else(|| "codesign did not report a signed Identifier".to_owned())?;
    let team_identifier = field("TeamIdentifier=")
        .filter(|team| team != "not set")
        .ok_or_else(|| "codesign did not report a Developer ID TeamIdentifier".to_owned())?;
    Ok(MacOsCodeIdentity {
        identifier,
        team_identifier,
    })
}

#[cfg(target_os = "macos")]
fn command_failure(tool: &str, output: &std::process::Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        format!("{tool} exited with {}", output.status)
    } else {
        format!("{tool} exited with {}: {detail}", output.status)
    }
}

#[cfg(target_os = "macos")]
fn verify_macos_app_signature(app: &Path) -> Result<MacOsCodeIdentity, String> {
    let verification = std::process::Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .output()
        .map_err(|err| format!("could not run codesign verification: {err}"))?;
    if !verification.status.success() {
        return Err(command_failure(
            "codesign --verify --deep --strict",
            &verification,
        ));
    }
    let assessment = std::process::Command::new("/usr/sbin/spctl")
        .args(["--assess", "--type", "execute"])
        .arg(app)
        .output()
        .map_err(|err| format!("could not run Gatekeeper assessment: {err}"))?;
    if !assessment.status.success() {
        return Err(command_failure(
            "spctl --assess --type execute",
            &assessment,
        ));
    }
    let display = std::process::Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=4"])
        .arg(app)
        .output()
        .map_err(|err| format!("could not inspect the app signature: {err}"))?;
    if !display.status.success() {
        return Err(command_failure("codesign --display --verbose=4", &display));
    }
    let mut identity_output = String::from_utf8_lossy(&display.stdout).into_owned();
    identity_output.push_str(&String::from_utf8_lossy(&display.stderr));
    parse_macos_codesign_identity(&identity_output)
}

#[cfg(target_os = "macos")]
fn verify_macos_bundle_pair(current_app: &Path, staged_app: &Path) -> Result<(), String> {
    let current = verify_macos_app_signature(current_app)
        .map_err(|reason| format!("current BowEcho.app failed verification: {reason}"))?;
    let staged = verify_macos_app_signature(staged_app)
        .map_err(|reason| format!("downloaded BowEcho.app failed verification: {reason}"))?;
    if current.identifier != MACOS_BUNDLE_IDENTIFIER || staged.identifier != MACOS_BUNDLE_IDENTIFIER
    {
        return Err(format!(
            "bundle identifier mismatch: expected {MACOS_BUNDLE_IDENTIFIER}, current is {}, \
             downloaded is {}",
            current.identifier, staged.identifier
        ));
    }
    if current.team_identifier != staged.team_identifier {
        return Err(format!(
            "Developer ID team mismatch: current app is signed by {}, downloaded app by {}",
            current.team_identifier, staged.team_identifier
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_macos_bundle_architecture(app: &Path) -> Result<(), String> {
    let lipo = Path::new("/usr/bin/lipo");
    if !lipo.exists() {
        return Ok(());
    }
    let output = std::process::Command::new(lipo)
        .arg("-archs")
        .arg(macos_app_executable(app))
        .output()
        .map_err(|err| format!("could not inspect the downloaded architecture: {err}"))?;
    if !output.status.success() {
        return Err(command_failure("lipo -archs", &output));
    }
    let expected = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => return Err(format!("unsupported macOS architecture {other}")),
    };
    let architectures = String::from_utf8_lossy(&output.stdout);
    if !architectures
        .split_whitespace()
        .any(|arch| arch == expected)
    {
        return Err(format!(
            "downloaded BowEcho.app does not contain the running {expected} architecture"
        ));
    }
    Ok(())
}

/// Client for the one-shot updater download: generous total timeout for a
/// ~50 MB asset on a slow link (the 8 s/45 s data_source budgets are sized
/// for radar polling, not installers); rustls like every other outbound
/// call.
#[cfg(any(windows, target_os = "macos"))]
fn self_update_http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "bowecho/",
            env!("CARGO_PKG_VERSION"),
            " (in-app updater)"
        ))
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(15 * 60))
        .build()
        .map_err(|err| format!("could not build HTTP client: {err}"))
}

/// An explicit update click retires every prior updater-owned stage before a
/// new one is allocated. Startup never calls this: it preserves the most
/// recent working bundle for rollback, while this boundary keeps retention
/// to at most one old app across successful updates.
#[cfg(any(target_os = "macos", test))]
fn prune_prior_private_macos_stages(parent: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(parent)
        .map_err(|err| format!("could not inspect prior macOS update stages: {err}"))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("could not inspect an update stage: {err}"))?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(MACOS_STAGE_PREFIX)
            && is_private_macos_stage(&entry.path())
        {
            remove_private_macos_stage(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn create_private_macos_stage(current_app: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    if current_app.file_name() != Some(std::ffi::OsStr::new(MACOS_APP_NAME)) {
        return Err("the running app is not an exact BowEcho.app bundle".to_owned());
    }
    if path_has_app_translocation_component(current_app) {
        return Err(
            "updates cannot run from macOS App Translocation; move BowEcho.app to Applications \
             and reopen it"
                .to_owned(),
        );
    }
    let parent = current_app
        .parent()
        .ok_or_else(|| "BowEcho.app has no parent directory".to_owned())?;
    prune_prior_private_macos_stages(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0_u8..32 {
        let stage = parent.join(format!(
            "{MACOS_STAGE_PREFIX}{}-{nonce:x}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&stage) {
            Ok(()) => {
                std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700)).map_err(
                    |err| {
                        let _ = std::fs::remove_dir(&stage);
                        format!("could not make the private update stage private: {err}")
                    },
                )?;
                let sentinel = stage.join(MACOS_STAGE_SENTINEL);
                let sentinel_result = (|| -> Result<(), std::io::Error> {
                    use std::io::Write as _;

                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&sentinel)?;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                    file.write_all(MACOS_STAGE_SENTINEL_CONTENTS.as_bytes())?;
                    file.sync_all()
                })();
                if let Err(err) = sentinel_result {
                    let _ = std::fs::remove_file(&sentinel);
                    let _ = std::fs::remove_dir(&stage);
                    return Err(format!(
                        "could not mark the private update stage safely: {err}"
                    ));
                }
                return Ok(stage);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(format!(
                    "BowEcho.app's parent directory is not writable; move the app to a location \
                     you can update ({err})"
                ));
            }
        }
    }
    Err("could not reserve a unique private update stage".to_owned())
}

#[cfg(target_os = "macos")]
fn extract_macos_release_zip(zip: &Path, extraction_root: &Path) -> Result<(), String> {
    std::fs::create_dir(extraction_root)
        .map_err(|err| format!("could not create the update extraction directory: {err}"))?;
    let output = std::process::Command::new("/usr/bin/ditto")
        .args(["-x", "-k"])
        .arg(zip)
        .arg(extraction_root)
        .output()
        .map_err(|err| format!("could not extract the macOS update with ditto: {err}"))?;
    if !output.status.success() {
        return Err(command_failure("ditto -x -k", &output));
    }
    Ok(())
}

/// Download → hash → checksum gate → platform signature gate. Windows
/// preserves the established executable swap before returning its relaunch
/// plan. macOS returns an untouched, verified sibling stage; its helper swaps
/// only after the GUI process has exited.
#[cfg(windows)]
fn run_self_update_worker(
    asset_url: &str,
    exe: &Path,
    events: &mpsc::Sender<SelfUpdateEvent>,
    ctx: &egui::Context,
) -> Result<SelfUpdateRelaunchPlan, String> {
    let client = self_update_http_client()?;
    let expected_sha256 = client
        .get(format!("{asset_url}.sha256"))
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.text())
        .map_err(|err| format!("could not fetch the release checksum: {err}"))?;
    let expected_sha256 = parse_sha256_asset(&expected_sha256);
    let downloaded = update_download_path(exe);
    let mut last_step_sent = u64::MAX;
    let actual_sha256 =
        download_update_asset(&client, asset_url, &downloaded, |received, total| {
            // Throttle to whole-percent (or per-MiB) steps — the 64 KiB read
            // loop would otherwise spam ~800 repaints per download.
            let step = match total {
                Some(total) if total > 0 => received * 100 / total,
                _ => received / (1024 * 1024),
            };
            if step != last_step_sent {
                last_step_sent = step;
                let _ = events.send(SelfUpdateEvent::Progress { received, total });
                ctx.request_repaint();
            }
        })
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&downloaded);
        })?;
    let _ = events.send(SelfUpdateEvent::Verifying);
    ctx.request_repaint();
    finalize_verified_update(
        &downloaded,
        exe,
        expected_sha256.as_deref(),
        &actual_sha256,
        verify_authenticode_signature,
        swap_running_executable,
    )?;
    Ok(SelfUpdateRelaunchPlan::Windows {
        executable: exe.to_path_buf(),
    })
}

#[cfg(target_os = "macos")]
fn run_self_update_worker(
    asset_url: &str,
    exe: &Path,
    events: &mpsc::Sender<SelfUpdateEvent>,
    ctx: &egui::Context,
) -> Result<SelfUpdateRelaunchPlan, String> {
    let executable = exe
        .canonicalize()
        .map_err(|err| format!("could not resolve the running executable: {err}"))?;
    let current_app = macos_app_bundle_from_executable(&executable)?;
    let stage_root = create_private_macos_stage(&current_app)?;
    let result = (|| {
        let client = self_update_http_client()?;
        let expected_sha256 = client
            .get(format!("{asset_url}.sha256"))
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.text())
            .map_err(|err| format!("could not fetch the release checksum: {err}"))?;
        let expected_sha256 = parse_sha256_asset(&expected_sha256)
            .ok_or_else(|| "the release's .sha256 file is missing or malformed".to_owned())?;
        let downloaded_zip = stage_root.join("update.zip");
        let mut last_step_sent = u64::MAX;
        let actual_sha256 =
            download_update_asset(&client, asset_url, &downloaded_zip, |received, total| {
                let step = match total {
                    Some(total) if total > 0 => received * 100 / total,
                    _ => received / (1024 * 1024),
                };
                if step != last_step_sent {
                    last_step_sent = step;
                    let _ = events.send(SelfUpdateEvent::Progress { received, total });
                    ctx.request_repaint();
                }
            })?;
        let _ = events.send(SelfUpdateEvent::Verifying);
        ctx.request_repaint();
        if !expected_sha256.eq_ignore_ascii_case(&actual_sha256) {
            return Err(format!(
                "SHA-256 mismatch: release lists {expected_sha256}, download hashed to \
                 {actual_sha256}"
            ));
        }
        let extraction_root = stage_root.join("extracted");
        extract_macos_release_zip(&downloaded_zip, &extraction_root)?;
        let staged_app = exact_extracted_macos_app(&extraction_root)?;
        verify_macos_bundle_pair(&current_app, &staged_app)?;
        verify_macos_bundle_architecture(&staged_app)?;
        let _ = std::fs::remove_file(&downloaded_zip);
        Ok(SelfUpdateRelaunchPlan::MacOs(MacOsRelaunchPlan {
            current_app,
            staged_app,
            stage_root: stage_root.clone(),
        }))
    })();
    if result.is_err() {
        let _ = remove_private_macos_stage(&stage_root);
    }
    result
}

#[cfg(not(any(windows, target_os = "macos")))]
fn run_self_update_worker(
    _asset_url: &str,
    _exe: &Path,
    _events: &mpsc::Sender<SelfUpdateEvent>,
    _ctx: &egui::Context,
) -> Result<SelfUpdateRelaunchPlan, String> {
    Err("in-app updates are available on Windows and macOS".to_owned())
}

/// Stream a release asset to `destination`, hashing the exact bytes written.
/// Returns the lowercase SHA-256 hex. `progress` fires with
/// `(received, total)`; total comes from Content-Length when present.
#[cfg(any(windows, target_os = "macos"))]
fn download_update_asset(
    client: &reqwest::blocking::Client,
    url: &str,
    destination: &Path,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<String, String> {
    use sha2::Digest as _;
    use std::io::{Read as _, Write as _};
    let mut response = client
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| format!("download failed: {err}"))?;
    let total = response.content_length();
    let mut file = std::fs::File::create(destination)
        .map_err(|err| format!("could not create {}: {err}", destination.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut received = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|err| format!("download interrupted: {err}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|err| format!("could not write {}: {err}", destination.display()))?;
        hasher.update(&buffer[..read]);
        received += read as u64;
        progress(received, total);
    }
    file.flush()
        .map_err(|err| format!("could not finish writing {}: {err}", destination.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// The two verification gates plus the swap, with the platform pieces
/// injected so tests can drive every branch without WinVerifyTrust or a
/// live executable swap. The downloaded file is deleted on ANY failure —
/// an unverified binary must never linger next to the real one.
#[cfg(any(windows, test))]
fn finalize_verified_update(
    downloaded: &Path,
    exe: &Path,
    expected_sha256: Option<&str>,
    actual_sha256: &str,
    verify_signature: impl FnOnce(&Path) -> Result<(), String>,
    swap: impl FnOnce(&Path, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let fail = |reason: String| -> Result<(), String> {
        let _ = std::fs::remove_file(downloaded);
        Err(reason)
    };
    let Some(expected) = expected_sha256 else {
        return fail("the release's .sha256 file is missing or malformed".to_owned());
    };
    if !expected.eq_ignore_ascii_case(actual_sha256) {
        return fail(format!(
            "SHA-256 mismatch: release lists {expected}, download hashed to {actual_sha256}"
        ));
    }
    if let Err(reason) = verify_signature(downloaded) {
        return fail(format!("signature verification failed: {reason}"));
    }
    swap(downloaded, exe).or_else(fail)
}

/// The swap: park the running executable as `.old` (renaming a running exe
/// is legal on Windows), then rename the verified download into its place.
/// Both paths share a directory, so neither rename crosses volumes. If the
/// second rename fails the original is renamed back.
#[cfg(windows)]
fn swap_running_executable(downloaded: &Path, exe: &Path) -> Result<(), String> {
    let backup = update_backup_path(exe);
    if backup.exists() {
        std::fs::remove_file(&backup)
            .map_err(|err| format!("could not clear the previous update backup: {err}"))?;
    }
    std::fs::rename(exe, &backup)
        .map_err(|err| format!("could not move the running executable aside: {err}"))?;
    if let Err(err) = std::fs::rename(downloaded, exe) {
        return Err(match std::fs::rename(&backup, exe) {
            Ok(()) => {
                format!(
                    "could not install the update ({err}); the previous executable was restored"
                )
            }
            Err(rollback) => format!(
                "could not install the update ({err}) and restoring the previous executable \
                 failed ({rollback}); re-download from the releases page"
            ),
        });
    }
    Ok(())
}

/// Authenticode gate: WinVerifyTrust with the generic verify-v2 policy —
/// the same trust decision PowerShell's `Get-AuthenticodeSignature` makes
/// (embedded signature present, file hash intact, chain to a trusted root),
/// with all UI suppressed. Revocation is deliberately NOT fetched over the
/// network here (`WTD_REVOKE_NONE` + cache-only retrieval): the SHA-256 pin
/// against the release page is the primary integrity gate, and a mid-update
/// revocation-server hiccup must not strand the user; see docs/SIGNING.md.
#[cfg(windows)]
fn verify_authenticode_signature(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE,
        WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
    };

    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide_path.as_ptr(),
        hFile: std::ptr::null_mut(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let mut trust_data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: std::ptr::null_mut(),
        pSIPClientData: std::ptr::null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        hWVTStateData: std::ptr::null_mut(),
        pwszURLReference: std::ptr::null_mut(),
        dwProvFlags: WTD_REVOCATION_CHECK_NONE | WTD_CACHE_ONLY_URL_RETRIEVAL,
        dwUIContext: 0,
        pSignatureSettings: std::ptr::null_mut(),
    };
    let status = unsafe {
        WinVerifyTrust(
            INVALID_HANDLE_VALUE,
            &mut action,
            (&raw mut trust_data).cast(),
        )
    };
    // Always release the verifier's state handle, whatever the verdict.
    trust_data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            INVALID_HANDLE_VALUE,
            &mut action,
            (&raw mut trust_data).cast(),
        );
    }
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "WinVerifyTrust rejected the file (0x{:08X})",
            status as u32
        ))
    }
}

impl ViewerApp {
    /// One release-version check per launch, on a background thread: never
    /// blocks the UI, and every failure (offline, rate-limited, bad JSON) is
    /// silent — offline users must see nothing.
    pub(crate) fn start_update_check(&mut self, ctx: &egui::Context) {
        if self.update_check_rx.in_flight() {
            return;
        }
        let Some(api_url) = brand::update_check_api_url(&self.app_settings.brand.repo_url) else {
            self.update_available = None;
            return;
        };
        self.update_check_rx.spawn(ctx, move |tx| {
            let _ = tx.send(fetch_newer_release_tag(&api_url));
        });
    }

    /// User clicked Install update: download the same-variant asset for the
    /// new tag, verify it against the release's `.sha256` and the platform
    /// trust service, prepare its native swap, and restart.
    /// Everything runs off the UI thread; [`Self::poll_self_update`] applies
    /// the worker's progress events. Never called without an explicit click.
    fn start_self_update(&mut self, ctx: &egui::Context, tag: &str, asset: &str) {
        if self.self_update_rx.in_flight() {
            return;
        }
        // Belt and braces with the button gate: exe replacement only ever
        // downloads from the canonical repo (self_update_repo_allowed).
        if !self_update_repo_allowed(&self.app_settings.brand.repo_url) {
            return;
        }
        let Some(api_url) = brand::update_check_api_url(&self.app_settings.brand.repo_url) else {
            return;
        };
        let Some(asset_url) = github_release_asset_url(&api_url, tag, asset) else {
            return;
        };
        // Resolve the running executable BEFORE any rename can confuse the
        // answer; every later path (download target, backup, relaunch)
        // derives from this one value.
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                self.self_update_phase = SelfUpdatePhase::Failed(format!(
                    "could not locate the running executable: {err}"
                ));
                return;
            }
        };
        self.self_update_phase = SelfUpdatePhase::Downloading {
            received: 0,
            total: None,
        };
        self.self_update_rx.spawn(ctx, move |tx| {
            match run_self_update_worker(&asset_url, &exe, tx.sender(), tx.ctx()) {
                Ok(plan) => {
                    // Relaunch happens in `main` after the event loop exits
                    // (see `relaunch_after_self_update`). On macOS the plan
                    // intentionally retains the verified sibling stage until
                    // the post-exit helper can swap the entire app bundle.
                    let _ = SELF_UPDATE_RELAUNCH.set(plan);
                    let _ = tx.send(SelfUpdateEvent::ReadyToRelaunch);
                }
                Err(reason) => {
                    let _ = tx.send(SelfUpdateEvent::Failed(reason));
                }
            }
        });
    }

    pub(crate) fn poll_self_update(&mut self, ctx: &egui::Context) {
        // No budget: install events are tiny and the stream ends at the
        // first terminal event, exactly like the pre-slot drain loop.
        let (events, state) = self.self_update_rx.drain(Duration::MAX);
        for event in events {
            if matches!(event, SelfUpdateEvent::ReadyToRelaunch) {
                // Shut down cleanly so `on_exit` persistence runs, then main
                // executes the platform relaunch plan.
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.self_update_phase = apply_self_update_event(&self.self_update_phase, &event);
        }
        // Worker vanished without a terminal event (panic):
        // report honestly instead of showing progress forever.
        if state == StreamState::Disconnected
            && matches!(
                self.self_update_phase,
                SelfUpdatePhase::Downloading { .. } | SelfUpdatePhase::Verifying
            )
        {
            self.self_update_phase =
                SelfUpdatePhase::Failed("update stopped unexpectedly".to_owned());
        }
    }

    pub(crate) fn security_updates_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let display_name = self.app_settings.brand.resolved_display_name().to_owned();
        let releases_url = brand::releases_page_url(&self.app_settings.brand);
        let release_check_available =
            brand::update_check_api_url(&self.app_settings.brand.repo_url).is_some();
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        if release_check_available {
            ui.weak(security_update_status_label(
                self.update_available.as_deref(),
                self.update_check_rx.in_flight(),
            ));
        } else {
            ui.weak("Automatic update checks are not configured for this brand.");
        }
        ui.horizontal_wrapped(|ui| {
            // In-app install: only when a newer release exists AND this
            // binary knows which release asset it is (baked by the release
            // workflow — local/dev builds never self-update) AND we are on
            // Windows/macOS AND updates come from the canonical repo (Brand-Kit
            // repo overrides keep the browser flow — see
            // self_update_repo_allowed). Everyone else gets the button below.
            let update_asset = self_update_asset(
                option_env!("BOWECHO_UPDATE_ASSET"),
                current_self_update_platform(),
            )
            .filter(|_| self_update_repo_allowed(&self.app_settings.brand.repo_url));
            if let (Some(tag), Some(asset)) = (self.update_available.clone(), update_asset) {
                let installing = self.self_update_rx.in_flight()
                    || matches!(self.self_update_phase, SelfUpdatePhase::Restarting);
                if ui
                    .add_enabled(!installing, egui::Button::new("Install update"))
                    .on_hover_text(if cfg!(target_os = "macos") {
                        format!(
                            "Download {asset} from the {tag} release, verify its SHA-256, \
                             Developer ID signature, and Gatekeeper assessment, then restart \
                             {display_name}"
                        )
                    } else {
                        format!(
                            "Download {asset} from the {tag} release, verify its SHA-256 checksum \
                             and Authenticode signature, then restart {display_name}"
                        )
                    })
                    .clicked()
                {
                    self.start_self_update(ctx, &tag, asset);
                }
            }
            if ui
                .add_enabled(releases_url.is_some(), egui::Button::new("Open releases"))
                .on_hover_text(format!("Open the configured {display_name} releases page"))
                .clicked()
                && let Some(releases_url) = &releases_url
            {
                ctx.open_url(egui::OpenUrl::new_tab(releases_url));
            }
            let checking = self.update_check_rx.in_flight();
            if ui
                .add_enabled(
                    release_check_available && !checking,
                    egui::Button::new("Check now"),
                )
                .on_hover_text("Re-run the background release check")
                .clicked()
            {
                self.start_update_check(ctx);
                self.status = format!("Checking {display_name} releases");
            }
            if checking || self.self_update_rx.in_flight() {
                ui.spinner();
            }
        });
        if let Some(label) = self_update_phase_label(&self.self_update_phase) {
            if matches!(self.self_update_phase, SelfUpdatePhase::Failed(_)) {
                ui.colored_label(ui.visuals().warn_fg_color, label);
            } else {
                ui.weak(label);
            }
        }
        ui.separator();
        // Explainer prose folds into a collapsed kit disclosure
        // (ui-refresh plan: prose never renders inline-expanded).
        #[cfg(windows)]
        crate::panel_kit::about(
            ui,
            "settings_smartscreen_about",
            "Windows Defender / SmartScreen",
            &[SECURITY_UNSIGNED_BUILD_TEXT, SECURITY_SIGNATURE_STATUS_TEXT],
        );
        #[cfg(target_os = "macos")]
        crate::panel_kit::about(
            ui,
            "settings_macos_update_security_about",
            "macOS update security",
            &[
                "Official macOS releases are Developer ID-signed, notarized, and assessed by \
                 Gatekeeper before an in-app update can install.",
                "The updater pins the canonical GitHub asset and its SHA-256, requires the \
                 stable BowEcho bundle identifier and the same signing team as the running app, \
                 then swaps the whole app bundle only after BowEcho exits.",
            ],
        );
        #[cfg(not(any(windows, target_os = "macos")))]
        crate::panel_kit::about(
            ui,
            "settings_release_security_about",
            "Release security",
            &["Linux opens the canonical releases page; in-app installation is not enabled."],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_update_asset_requires_a_baked_platform_variant() {
        assert_eq!(
            self_update_asset(
                Some("bowecho-windows-x64-v3.exe"),
                SelfUpdatePlatform::Windows
            ),
            Some("bowecho-windows-x64-v3.exe")
        );
        // Local/dev builds bake nothing → the in-app installer never appears.
        assert_eq!(self_update_asset(None, SelfUpdatePlatform::Windows), None);
        // CI passes an empty env value to targets without an update asset.
        assert_eq!(
            self_update_asset(Some(""), SelfUpdatePlatform::Windows),
            None
        );
        assert_eq!(
            self_update_asset(Some("  "), SelfUpdatePlatform::Windows),
            None
        );
        assert_eq!(
            self_update_asset(
                Some("bowecho-macos-intel.zip"),
                SelfUpdatePlatform::MacIntel
            ),
            Some("bowecho-macos-intel.zip")
        );
        assert_eq!(
            self_update_asset(
                Some("bowecho-macos-apple-silicon.zip"),
                SelfUpdatePlatform::MacAppleSilicon
            ),
            Some("bowecho-macos-apple-silicon.zip")
        );
        assert_eq!(
            self_update_asset(
                Some("bowecho-macos-apple-silicon.zip"),
                SelfUpdatePlatform::MacIntel
            ),
            None
        );
        // Linux stays browser-only even if an asset name were accidentally baked.
        assert_eq!(
            self_update_asset(Some("bowecho-linux-x64"), SelfUpdatePlatform::BrowserOnly),
            None
        );
    }

    #[test]
    fn self_update_is_pinned_to_the_canonical_repo() {
        // The Authenticode gate accepts any trusted signer, so exe
        // replacement must never follow a Brand-Kit repo override.
        assert!(self_update_repo_allowed(
            "https://github.com/FahrenheitResearch/bowecho"
        ));
        assert!(self_update_repo_allowed(
            "https://github.com/FahrenheitResearch/bowecho/"
        ));
        assert!(!self_update_repo_allowed(
            "https://github.com/someone-else/bowecho-fork"
        ));
        // Empty = no override: the stock exe with brand-kit assets installs
        // from the canonical feed (the passive check falls back there too).
        assert!(self_update_repo_allowed(""));
        assert!(self_update_repo_allowed("  "));
        // ".git" is the same canonical repo: the passive check parses it
        // (and offers the update), so the install gate must agree — a
        // literal string compare left the install button silently absent.
        let git_suffixed = "https://github.com/FahrenheitResearch/bowecho.git";
        assert!(self_update_repo_allowed(git_suffixed));
        assert_eq!(
            brand::update_check_api_url(git_suffixed),
            brand::update_check_api_url(""),
            "check and install must resolve the same feed"
        );
        // A fork stays rejected however it is spelled.
        assert!(!self_update_repo_allowed(
            "https://github.com/someone-else/bowecho-fork.git"
        ));
    }

    #[test]
    fn github_release_asset_url_derives_from_latest_release_api_url() {
        assert_eq!(
            github_release_asset_url(
                "https://api.github.com/repos/FahrenheitResearch/bowecho/releases/latest",
                "v0.28.2",
                "bowecho-windows-x64.exe"
            )
            .as_deref(),
            Some(
                "https://github.com/FahrenheitResearch/bowecho/releases/download/v0.28.2/bowecho-windows-x64.exe"
            )
        );
        assert_eq!(
            github_release_asset_url("https://example.org/feed.json", "v1.0.0", "app.exe"),
            None
        );
    }

    #[test]
    fn parse_sha256_asset_accepts_every_ci_checksum_format() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        // Windows runner sha256sum writes binary mode: "<hex> *<name>".
        assert_eq!(
            parse_sha256_asset(&format!("{hex} *bowecho-windows-x64.exe\n")).as_deref(),
            Some(hex)
        );
        // Linux runner writes text mode: "<hex>  <name>".
        assert_eq!(
            parse_sha256_asset(&format!("{hex}  bowecho-linux-x64\n")).as_deref(),
            Some(hex)
        );
        // Bare digest works; uppercase folds to lowercase for comparison.
        assert_eq!(
            parse_sha256_asset(&hex.to_ascii_uppercase()).as_deref(),
            Some(hex)
        );
        assert_eq!(parse_sha256_asset(""), None);
        assert_eq!(parse_sha256_asset("not-a-hash *file.exe"), None);
        assert_eq!(parse_sha256_asset(&hex[..63]), None);
    }

    #[test]
    fn self_update_phase_transitions_and_labels_are_honest() {
        let idle = SelfUpdatePhase::Idle;
        assert_eq!(self_update_phase_label(&idle), None);

        let downloading = apply_self_update_event(
            &idle,
            &SelfUpdateEvent::Progress {
                received: 25 * 1024 * 1024,
                total: Some(50 * 1024 * 1024),
            },
        );
        assert_eq!(
            self_update_phase_label(&downloading).as_deref(),
            Some("Downloading update — 50%")
        );
        // No Content-Length → byte progress, never a fake percent.
        let sizeless = apply_self_update_event(
            &downloading,
            &SelfUpdateEvent::Progress {
                received: 3 * 1024 * 1024 + 512 * 1024,
                total: None,
            },
        );
        assert_eq!(
            self_update_phase_label(&sizeless).as_deref(),
            Some("Downloading update — 3.5 MB")
        );

        let verifying = apply_self_update_event(&sizeless, &SelfUpdateEvent::Verifying);
        assert_eq!(verifying, SelfUpdatePhase::Verifying);

        let restarting = apply_self_update_event(&verifying, &SelfUpdateEvent::ReadyToRelaunch);
        assert_eq!(restarting, SelfUpdatePhase::Restarting);
        // Terminal: no late event may resurrect the progress UI.
        assert_eq!(
            apply_self_update_event(&restarting, &SelfUpdateEvent::Verifying),
            SelfUpdatePhase::Restarting
        );

        let failed =
            apply_self_update_event(&verifying, &SelfUpdateEvent::Failed("checksum".to_owned()));
        assert_eq!(
            self_update_phase_label(&failed).as_deref(),
            Some("Update failed: checksum")
        );
        assert_eq!(
            apply_self_update_event(
                &failed,
                &SelfUpdateEvent::Progress {
                    received: 1,
                    total: None
                }
            ),
            failed
        );
    }

    #[test]
    fn update_artifact_paths_sit_next_to_the_executable() {
        let exe = Path::new("C:/apps/BowEcho/bowecho.exe");
        assert_eq!(
            update_backup_path(exe),
            Path::new("C:/apps/BowEcho/bowecho.exe.old")
        );
        assert_eq!(
            update_download_path(exe),
            Path::new("C:/apps/BowEcho/bowecho.exe.update")
        );
        // The startup sweep must target exactly the swap leftovers — the
        // parked previous binary and a dead partial download.
        assert_eq!(
            stale_update_artifacts(exe),
            [
                PathBuf::from("C:/apps/BowEcho/bowecho.exe.old"),
                PathBuf::from("C:/apps/BowEcho/bowecho.exe.update"),
            ]
        );
    }

    #[test]
    fn macos_bundle_derivation_accepts_only_the_exact_app_layout() {
        let executable = Path::new("C:/Applications/BowEcho.app/Contents/MacOS/bowecho");
        let app = Path::new("C:/Applications/BowEcho.app");
        assert_eq!(macos_app_bundle_from_executable(executable).unwrap(), app);
        let stage = Path::new("C:/Applications/.bowecho-update-stage-test");
        assert_eq!(
            macos_backup_path(stage),
            stage.join("previous").join(MACOS_APP_NAME)
        );
        assert!(
            macos_app_bundle_from_executable(Path::new(
                "C:/Applications/Renamed.app/Contents/MacOS/bowecho"
            ))
            .is_err()
        );
        assert!(
            macos_app_bundle_from_executable(Path::new(
                "C:/Applications/BowEcho.app/Contents/Helpers/bowecho"
            ))
            .is_err()
        );
        assert!(
            macos_app_bundle_from_executable(Path::new(
                "C:/Applications/BowEcho.app/Contents/MacOS/not-bowecho"
            ))
            .is_err()
        );
    }

    #[test]
    fn app_translocation_is_rejected_by_component_not_substring() {
        assert!(path_has_app_translocation_component(Path::new(
            "C:/private/var/AppTranslocation/xyz/d/BowEcho.app"
        )));
        assert!(!path_has_app_translocation_component(Path::new(
            "C:/Applications/AppTranslocation Notes/BowEcho.app"
        )));
    }

    #[test]
    fn macos_codesign_identity_requires_identifier_and_real_team() {
        let identity = parse_macos_codesign_identity(
            "Executable=/Applications/BowEcho.app/Contents/MacOS/bowecho\n\
             Identifier=research.fahrenheit.bowecho\n\
             TeamIdentifier=ABCDE12345\n",
        )
        .unwrap();
        assert_eq!(identity.identifier, MACOS_BUNDLE_IDENTIFIER);
        assert_eq!(identity.team_identifier, "ABCDE12345");
        assert!(parse_macos_codesign_identity("Identifier=research.fahrenheit.bowecho").is_err());
        assert!(
            parse_macos_codesign_identity(
                "Identifier=research.fahrenheit.bowecho\nTeamIdentifier=not set\n"
            )
            .is_err()
        );
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bowecho-selfupdate-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch directory");
        path
    }

    fn write_fake_macos_app(app: &Path, marker: &str) {
        let macos = app.join("Contents").join("MacOS");
        std::fs::create_dir_all(&macos).expect("create fake app layout");
        std::fs::write(app.join("Contents").join("Info.plist"), b"plist")
            .expect("write fake plist");
        std::fs::write(macos.join(MACOS_EXECUTABLE_NAME), marker.as_bytes())
            .expect("write fake executable");
    }

    fn write_fake_private_stage(stage: &Path) {
        std::fs::create_dir_all(stage).expect("create fake private stage");
        std::fs::write(
            stage.join(MACOS_STAGE_SENTINEL),
            MACOS_STAGE_SENTINEL_CONTENTS,
        )
        .expect("write private stage sentinel");
    }

    #[test]
    fn extracted_macos_release_requires_one_exact_bowecho_app() {
        let root = scratch_directory("mac-extraction");
        let app = root.join(MACOS_APP_NAME);
        write_fake_macos_app(&app, "new");
        assert_eq!(exact_extracted_macos_app(&root).unwrap(), app);
        std::fs::write(root.join("unexpected.txt"), b"extra").unwrap();
        assert!(exact_extracted_macos_app(&root).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn private_stage_removal_requires_exact_sentinel() {
        let parent = scratch_directory("stage-sentinel");
        let stage = parent.join(format!("{MACOS_STAGE_PREFIX}test"));
        std::fs::create_dir(&stage).unwrap();
        assert!(!is_private_macos_stage(&stage));
        assert!(remove_private_macos_stage(&stage).is_err());
        assert!(stage.exists(), "unmarked directory must not be removed");
        std::fs::write(
            stage.join(MACOS_STAGE_SENTINEL),
            MACOS_STAGE_SENTINEL_CONTENTS,
        )
        .unwrap();
        assert!(is_private_macos_stage(&stage));
        remove_private_macos_stage(&stage).unwrap();
        assert!(!stage.exists());
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn startup_cleanup_predicate_preserves_a_valid_previous_app() {
        let parent = scratch_directory("stage-preserve-backup");
        let stage = parent.join(format!("{MACOS_STAGE_PREFIX}preserve"));
        write_fake_private_stage(&stage);
        assert!(private_macos_stage_is_abandoned(&stage));
        write_fake_macos_app(&macos_backup_path(&stage), "previous");
        assert!(private_macos_stage_has_valid_previous_app(&stage));
        assert!(
            !private_macos_stage_is_abandoned(&stage),
            "startup cleanup must preserve the last working app"
        );
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn explicit_update_prunes_only_exact_sentinel_stages() {
        let parent = scratch_directory("stage-explicit-prune");
        let owned = parent.join(format!("{MACOS_STAGE_PREFIX}owned"));
        write_fake_private_stage(&owned);
        write_fake_macos_app(&macos_backup_path(&owned), "previous");
        let unmarked = parent.join(format!("{MACOS_STAGE_PREFIX}unmarked"));
        std::fs::create_dir(&unmarked).unwrap();
        std::fs::write(unmarked.join("keep"), b"not updater-owned").unwrap();
        prune_prior_private_macos_stages(&parent).unwrap();
        assert!(!owned.exists(), "explicit update retires its prior backup");
        assert!(
            unmarked.join("keep").exists(),
            "a prefix without the exact sentinel is never removable"
        );
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn macos_bundle_swap_is_whole_bundle_and_rolls_back() {
        let root = scratch_directory("mac-swap");
        let current = root.join(MACOS_APP_NAME);
        let stage = root.join(format!("{MACOS_STAGE_PREFIX}first"));
        write_fake_private_stage(&stage);
        let staged = stage.join("extracted").join(MACOS_APP_NAME);
        write_fake_macos_app(&current, "old");
        write_fake_macos_app(&staged, "new");
        let backup = swap_macos_app_bundle(&staged, &current, &stage).unwrap();
        assert_eq!(
            std::fs::read(macos_app_executable(&current)).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(macos_app_executable(&backup)).unwrap(),
            b"old"
        );

        // Force the second rename to fail; the old bundle must return to its
        // original path.
        std::fs::remove_dir_all(&current).unwrap();
        std::fs::rename(&backup, &current).unwrap();
        let stage_two = root.join(format!("{MACOS_STAGE_PREFIX}second"));
        write_fake_private_stage(&stage_two);
        // Put the staged stand-in inside the current bundle. Moving current
        // aside moves that nested path too, forcing the install rename to
        // fail after the backup rename and exercising the real rollback.
        let nested_staged = current.join("nested").join(MACOS_APP_NAME);
        write_fake_macos_app(&nested_staged, "nested-new");
        let outcome = swap_macos_app_bundle(&nested_staged, &current, &stage_two);
        assert!(outcome.unwrap_err().contains("previous app was restored"));
        assert!(has_exact_macos_bundle_layout(&current));
        assert!(!macos_backup_path(&stage_two).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn macos_bundle_swap_never_deletes_an_existing_stage_backup() {
        let root = scratch_directory("mac-existing-backup");
        let current = root.join(MACOS_APP_NAME);
        let stage = root.join(format!("{MACOS_STAGE_PREFIX}occupied"));
        write_fake_private_stage(&stage);
        let staged = stage.join("extracted").join(MACOS_APP_NAME);
        let backup = macos_backup_path(&stage);
        write_fake_macos_app(&current, "old");
        write_fake_macos_app(&staged, "new");
        write_fake_macos_app(&backup, "do-not-delete");
        let outcome = swap_macos_app_bundle(&staged, &current, &stage);
        assert!(outcome.unwrap_err().contains("already contains"));
        assert_eq!(
            std::fs::read(macos_app_executable(&backup)).unwrap(),
            b"do-not-delete"
        );
        assert!(current.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Scratch file for the finalize tests: a real on-disk download stand-in
    /// so the "deleted on failure" property is tested against the file
    /// system, not a mock.
    fn scratch_download(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bowecho-selfupdate-test-{}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, b"downloaded bytes").expect("write scratch download");
        path
    }

    #[test]
    fn finalize_verified_update_deletes_download_on_checksum_mismatch() {
        let downloaded = scratch_download("checksum-mismatch");
        let outcome = finalize_verified_update(
            &downloaded,
            Path::new("unused.exe"),
            Some("aaaa"),
            "bbbb",
            |_| panic!("signature must not be checked after a checksum mismatch"),
            |_, _| panic!("swap must not run after a checksum mismatch"),
        );
        assert!(outcome.unwrap_err().contains("SHA-256 mismatch"));
        assert!(!downloaded.exists(), "failed download must be deleted");
    }

    #[test]
    fn finalize_verified_update_requires_a_parseable_release_checksum() {
        let downloaded = scratch_download("missing-checksum");
        let outcome = finalize_verified_update(
            &downloaded,
            Path::new("unused.exe"),
            None,
            "abcd",
            |_| panic!("signature must not be checked without a release checksum"),
            |_, _| panic!("swap must not run without a release checksum"),
        );
        assert!(outcome.unwrap_err().contains(".sha256"));
        assert!(!downloaded.exists());
    }

    #[test]
    fn finalize_verified_update_deletes_download_when_signature_fails() {
        let downloaded = scratch_download("bad-signature");
        let outcome = finalize_verified_update(
            &downloaded,
            Path::new("unused.exe"),
            Some("ABCD"),
            "abcd", // checksum comparison is case-insensitive
            |_| Err("WinVerifyTrust rejected the file (0x800B0100)".to_owned()),
            |_, _| panic!("swap must not run after a signature failure"),
        );
        let reason = outcome.unwrap_err();
        assert!(reason.contains("signature verification failed"));
        assert!(reason.contains("0x800B0100"));
        assert!(!downloaded.exists());
    }

    #[test]
    fn finalize_verified_update_deletes_download_when_swap_fails() {
        let downloaded = scratch_download("swap-failure");
        let outcome = finalize_verified_update(
            &downloaded,
            Path::new("unused.exe"),
            Some("abcd"),
            "abcd",
            |_| Ok(()),
            |_, _| Err("could not move the running executable aside: locked".to_owned()),
        );
        assert!(outcome.unwrap_err().contains("executable aside"));
        assert!(!downloaded.exists());
    }

    #[test]
    fn finalize_verified_update_swaps_only_after_both_gates_pass() {
        let downloaded = scratch_download("success");
        let verified = std::cell::Cell::new(false);
        let swapped = std::cell::Cell::new(false);
        let exe = Path::new("C:/apps/bowecho.exe");
        let outcome = finalize_verified_update(
            &downloaded,
            exe,
            Some("abcd"),
            "abcd",
            |path| {
                assert_eq!(path, downloaded, "signature check runs on the download");
                verified.set(true);
                Ok(())
            },
            |from, to| {
                assert!(verified.get(), "swap must come after the signature gate");
                assert_eq!((from, to), (downloaded.as_path(), exe));
                swapped.set(true);
                Ok(())
            },
        );
        assert_eq!(outcome, Ok(()));
        assert!(swapped.get());
        assert!(
            downloaded.exists(),
            "on success the swap owns the file; finalize must not delete it"
        );
        let _ = std::fs::remove_file(&downloaded);
    }

    #[cfg(windows)]
    #[test]
    fn winverifytrust_rejects_a_missing_file() {
        let missing = std::env::temp_dir().join("bowecho-selfupdate-no-such-file.exe");
        assert!(!missing.exists());
        assert!(verify_authenticode_signature(&missing).is_err());
    }

    /// Manual, developer-machine-only check of the real WinVerifyTrust
    /// wrapper against a real Authenticode-signed release exe and a real
    /// unsigned build:
    /// `BOWECHO_TEST_SIGNED_EXE=<signed> BOWECHO_TEST_UNSIGNED_EXE=<unsigned>
    ///  cargo test -p app_ui --bin bowecho -- --ignored winverifytrust_wrapper`
    #[cfg(windows)]
    #[test]
    #[ignore = "needs local exes via BOWECHO_TEST_SIGNED_EXE / BOWECHO_TEST_UNSIGNED_EXE"]
    fn winverifytrust_wrapper_accepts_signed_and_rejects_unsigned() {
        let signed = std::env::var("BOWECHO_TEST_SIGNED_EXE")
            .expect("set BOWECHO_TEST_SIGNED_EXE to an Authenticode-signed exe");
        let unsigned = std::env::var("BOWECHO_TEST_UNSIGNED_EXE")
            .expect("set BOWECHO_TEST_UNSIGNED_EXE to an unsigned exe");
        assert_eq!(verify_authenticode_signature(Path::new(&signed)), Ok(()));
        let rejection = verify_authenticode_signature(Path::new(&unsigned));
        let reason = rejection.expect_err("an unsigned exe must be rejected");
        assert!(
            reason.contains("WinVerifyTrust rejected"),
            "unexpected rejection reason: {reason}"
        );
    }
}
