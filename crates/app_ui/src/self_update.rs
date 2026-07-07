//! Self-update: the passive release check plus the Windows in-app updater
//! (download → SHA-256 → Authenticode → swap → relaunch).
//!
//! v0.29.4 phase-1 extraction #1 (docs/main-decomposition-plan.md): every
//! body moved VERBATIM from main.rs — the only edits are `use` lines and
//! the pub(crate) promotions listed in the extraction commit message.
//! `brand.rs` keeps sole ownership of repo-URL parsing
//! (`update_check_api_url`, `CANONICAL_REPO_URL`, …); this module calls it.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use eframe::egui;
use ui_core::worker_slot::{SlotMessage, StreamState};

use crate::{SECURITY_SIGNATURE_STATUS_TEXT, SECURITY_UNSIGNED_BUILD_TEXT, ViewerApp, brand};

/// Fetch the latest configured GitHub release tag and return it iff it is
/// newer than the running build. Invalid/non-GitHub repository URLs disable
/// the check before this helper is called; network/parse errors stay silent.
fn fetch_newer_release_tag(api_url: &str) -> Option<String> {
    let body = data_source::fetch_text(api_url).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    newer_release_tag(tag, env!("CARGO_PKG_VERSION"))
}

/// Some(trimmed tag) iff the release tag is strictly newer than the
/// current version; None on equal, older, or unparseable input.
fn newer_release_tag(tag_name: &str, current_version: &str) -> Option<String> {
    let remote = parse_semver_triple(tag_name)?;
    let current = parse_semver_triple(current_version)?;
    (remote > current).then(|| tag_name.trim().to_owned())
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

/// Parse "v1.2.3" / "1.2.3" into (major, minor, patch). Tolerates the
/// release-tag leading 'v'/'V', missing components ("v0.9" → (0, 9, 0)),
/// and a pre-release/build suffix ("v0.9.0-rc1" parses as (0, 9, 0) —
/// numeric triple only, good enough for "is there a newer release").
/// Anything non-numeric is None: the update check then stays silent.
fn parse_semver_triple(version: &str) -> Option<(u64, u64, u64)> {
    let trimmed = version.trim().trim_start_matches(['v', 'V']);
    let core = trimmed.split(['-', '+']).next()?;
    if core.is_empty() {
        return None;
    }
    let mut parts = core.split('.');
    let mut triple = [0_u64; 3];
    for slot in &mut triple {
        match parts.next() {
            Some(part) => *slot = part.parse().ok()?,
            None => break,
        }
    }
    if parts.next().is_some() {
        // Four or more dotted components — not a semver triple.
        return None;
    }
    Some((triple[0], triple[1], triple[2]))
}

// ---------------------------------------------------------------------------
// In-app updater (Windows release builds only — docs/SIGNING.md)
//
// User clicks Install update → background thread downloads the SAME release
// asset variant this binary was built as, plus its `.sha256` → the download
// must match that checksum AND carry a valid Authenticode signature
// (WinVerifyTrust) → the running exe is parked as `.old`, the verified file
// renamed into its place, and the app restarts itself after a clean
// shutdown. Any failed check deletes the download and reports the reason.
// Nothing ever installs without the click.

/// One event from the update worker to the UI.
pub(crate) enum SelfUpdateEvent {
    Progress { received: u64, total: Option<u64> },
    Verifying,
    SwapComplete,
    Failed(String),
}

/// The install worker streams progress into a [`StreamSlot`]; the slot stays
/// busy until the outcome event lands (or the worker vanishes — see
/// [`ViewerApp::poll_self_update`]'s honest-failure fallback).
impl SlotMessage for SelfUpdateEvent {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            SelfUpdateEvent::SwapComplete | SelfUpdateEvent::Failed(_)
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

/// Set once by the update worker after a successful swap: the path to spawn
/// (the same path this process started from — the file now holds the new
/// release). Read by `main` after the event loop exits.
static SELF_UPDATE_RELAUNCH: OnceLock<PathBuf> = OnceLock::new();

/// Build-time variant self-identification. The release workflow bakes the
/// exact asset name this binary ships as (`BOWECHO_UPDATE_ASSET`, e.g.
/// "bowecho-windows-x64-v3.exe") into the Windows builds, so the updater can
/// only ever download the variant it is already running. Local/dev builds
/// bake nothing and CI passes an empty value to non-Windows targets — both
/// yield `None`, which keeps the in-app installer hidden (deliberate safety
/// property: an unbaked binary cannot guess its own variant). macOS/Linux
/// stay browser-only for now, hence the explicit platform gate.
fn self_update_asset(baked: Option<&'static str>, is_windows: bool) -> Option<&'static str> {
    if !is_windows {
        return None;
    }
    baked.map(str::trim).filter(|name| !name.is_empty())
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
        (_, SelfUpdateEvent::SwapComplete) => SelfUpdatePhase::Restarting,
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
            Some("Verifying download (SHA-256 checksum + Authenticode signature)".to_owned())
        }
        SelfUpdatePhase::Restarting => Some("Update installed — restarting".to_owned()),
        SelfUpdatePhase::Failed(reason) => Some(format!("Update failed: {reason}")),
    }
}

/// `bowecho.exe` → `bowecho.exe.old`: where the running executable is parked
/// during the swap (renaming a running exe is legal on Windows; deleting it
/// is not).
fn update_backup_path(exe: &Path) -> PathBuf {
    sibling_with_suffix(exe, ".old")
}

/// `bowecho.exe` → `bowecho.exe.update`: the download target, deliberately
/// next to the executable so the final rename never crosses volumes.
fn update_download_path(exe: &Path) -> PathBuf {
    sibling_with_suffix(exe, ".update")
}

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
fn stale_update_artifacts(exe: &Path) -> [PathBuf; 2] {
    [update_backup_path(exe), update_download_path(exe)]
}

/// Best-effort startup sweep of update leftovers. The `.old` file is
/// usually still locked on the first post-update launch (the pre-update
/// process is exiting) — removal failures stay silent and retry on the
/// next launch.
pub(crate) fn cleanup_stale_update_artifacts() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    for path in stale_update_artifacts(&exe) {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Spawn the freshly installed executable with the original CLI args. Runs
/// only after `eframe::run_native` returned, so `on_exit` persistence
/// (workspace layout, sounding state) completed before the new process
/// starts reading state.
pub(crate) fn relaunch_after_self_update() {
    let Some(exe) = SELF_UPDATE_RELAUNCH.get() else {
        return;
    };
    if let Err(err) = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()
    {
        eprintln!("Failed to relaunch {} after update: {err}", exe.display());
    }
}

/// Client for the one-shot updater download: generous total timeout for a
/// ~50 MB asset on a slow link (the 8 s/45 s data_source budgets are sized
/// for radar polling, not installers); rustls like every other outbound
/// call.
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

/// Download → hash → checksum gate → signature gate → swap. Every failure
/// is a reason string for the UI, and no failure path leaves a partial
/// download behind.
fn run_self_update_worker(
    asset_url: &str,
    exe: &Path,
    events: &mpsc::Sender<SelfUpdateEvent>,
    ctx: &egui::Context,
) -> Result<(), String> {
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
    )
}

/// Stream a release asset to `destination`, hashing the exact bytes written.
/// Returns the lowercase SHA-256 hex. `progress` fires with
/// `(received, total)`; total comes from Content-Length when present.
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

/// Explicit platform gate: in-app updates are Windows-only for now
/// (macOS/Linux keep the browser flow), so off-Windows verification
/// refuses everything. Unreachable in practice — [`self_update_asset`]
/// already returns `None` off-Windows, so the installer UI never appears.
#[cfg(not(windows))]
fn verify_authenticode_signature(_path: &Path) -> Result<(), String> {
    Err("in-app updates are Windows-only".to_owned())
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
    /// new tag, verify it (SHA-256 against the release's `.sha256`, then
    /// Authenticode via WinVerifyTrust), swap it into place, and restart.
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
            // The worker body is §9-protected ("logic untouched"): it keeps
            // its own sender/ctx signature and progress-throttled repaints.
            match run_self_update_worker(&asset_url, &exe, tx.sender(), tx.ctx()) {
                Ok(()) => {
                    // Relaunch happens in `main` after the event loop exits
                    // (see `relaunch_after_self_update`); the UI only needs
                    // to close the viewport when it sees this event.
                    let _ = SELF_UPDATE_RELAUNCH.set(exe);
                    let _ = tx.send(SelfUpdateEvent::SwapComplete);
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
            if matches!(event, SelfUpdateEvent::SwapComplete) {
                // The verified binary is already in place on disk;
                // shut down cleanly so `on_exit` persistence runs,
                // then `main` spawns the new executable.
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
            // Windows AND updates come from the canonical repo (Brand-Kit
            // repo overrides keep the browser flow — see
            // self_update_repo_allowed). Everyone else gets the button below.
            let update_asset =
                self_update_asset(option_env!("BOWECHO_UPDATE_ASSET"), cfg!(windows))
                    .filter(|_| self_update_repo_allowed(&self.app_settings.brand.repo_url));
            if let (Some(tag), Some(asset)) = (self.update_available.clone(), update_asset) {
                let installing = self.self_update_rx.in_flight()
                    || matches!(self.self_update_phase, SelfUpdatePhase::Restarting);
                if ui
                    .add_enabled(!installing, egui::Button::new("Install update"))
                    .on_hover_text(format!(
                        "Download {asset} from the {tag} release, verify its SHA-256 checksum \
                         and Authenticode signature, then restart {display_name}"
                    ))
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
        ui.strong("Windows Defender / SmartScreen");
        ui.weak(SECURITY_UNSIGNED_BUILD_TEXT);
        ui.weak(SECURITY_SIGNATURE_STATUS_TEXT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver_triple_handles_release_tags() {
        assert_eq!(parse_semver_triple("v0.8.2"), Some((0, 8, 2)));
        assert_eq!(parse_semver_triple("V1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver_triple("0.8.2"), Some((0, 8, 2)));
        assert_eq!(parse_semver_triple(" v0.9 "), Some((0, 9, 0)));
        assert_eq!(parse_semver_triple("v2"), Some((2, 0, 0)));
        assert_eq!(parse_semver_triple("v0.9.0-rc1"), Some((0, 9, 0)));
        assert_eq!(parse_semver_triple("v0.9.0+build5"), Some((0, 9, 0)));
        assert_eq!(parse_semver_triple(""), None);
        assert_eq!(parse_semver_triple("v"), None);
        assert_eq!(parse_semver_triple("latest"), None);
        assert_eq!(parse_semver_triple("v0.8.2.1"), None);
        assert_eq!(parse_semver_triple("v0..2"), None);
    }

    #[test]
    fn newer_release_tag_compares_numerically() {
        let some = |tag: &str| Some(tag.to_owned());
        assert_eq!(newer_release_tag("v0.9.0", "0.8.2"), some("v0.9.0"));
        // Numeric compare, not lexicographic: "10" > "2".
        assert_eq!(newer_release_tag("v0.8.10", "0.8.2"), some("v0.8.10"));
        assert_eq!(newer_release_tag("v1.0.0", "0.9.9"), some("v1.0.0"));
        // Same version, older remote, prerelease of the current version,
        // and junk tags all stay silent.
        assert_eq!(newer_release_tag("v0.8.2", "0.8.2"), None);
        assert_eq!(newer_release_tag("v0.8.1", "0.8.2"), None);
        assert_eq!(newer_release_tag("v0.8.2-rc1", "0.8.2"), None);
        assert_eq!(newer_release_tag("latest", "0.8.2"), None);
    }

    #[test]
    fn self_update_asset_requires_baked_name_and_windows() {
        assert_eq!(
            self_update_asset(Some("bowecho-windows-x64-v3.exe"), true),
            Some("bowecho-windows-x64-v3.exe")
        );
        // Local/dev builds bake nothing → the in-app installer never appears.
        assert_eq!(self_update_asset(None, true), None);
        // CI passes an empty env value to targets without an update asset.
        assert_eq!(self_update_asset(Some(""), true), None);
        assert_eq!(self_update_asset(Some("  "), true), None);
        // macOS/Linux stay browser-only even if a name were baked.
        assert_eq!(self_update_asset(Some("bowecho-linux-x64"), false), None);
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

        let restarting = apply_self_update_event(&verifying, &SelfUpdateEvent::SwapComplete);
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
