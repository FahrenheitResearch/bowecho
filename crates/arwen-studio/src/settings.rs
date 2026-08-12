// SPDX-License-Identifier: Apache-2.0

//! Persisted Studio settings. Swapping fixture → live gpuwm is a CONFIG
//! change here, never a code change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arwen_proc::{ContractSource, LauncherSpec};
use serde::{Deserialize, Serialize};

pub const SETTINGS_SCHEMA: &str = "arwen-studio.settings.v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StudioSettings {
    pub schema: String,
    /// `"fixture"` until the live `gpuwm run-plan` CLI lands.
    pub contract_mode: String,
    /// Fixture directory; `None` = auto-discover (exe-relative, then the
    /// dev workspace).
    #[serde(default)]
    pub fixture_dir: Option<String>,
    /// Fixture replay wall-time compression.
    pub fixture_speed: f64,
    /// Live mode: how to invoke gpuwm.
    pub live_program: String,
    pub live_args_prefix: Vec<String>,
    /// The COMPLETE sealed environment for the live child.
    pub live_env: BTreeMap<String, String>,
    /// Where gpuwm creates run directories.
    pub output_root: String,
    /// A staged WPS_GEOG tree (per-box; flows into the intent's
    /// `--geog-root`). `None` = the engine's default location.
    #[serde(default)]
    pub geog_root: Option<String>,
    /// The built Rust renderer (`rw_wrfbatch`), the repo-mandated render
    /// engine for product PNGs. Flows to the engine as
    /// `GPUWM_RW_WRFBATCH`; when unset the engine falls back to its
    /// matplotlib path (visible in the UI, never silent).
    #[serde(default)]
    pub rust_renderer: Option<String>,
    /// Basemap tile style key ("dark" = vector-only).
    pub tile_style: String,
    /// Favorite render products (catalog names, verbatim) — the
    /// `render_products` run option's Favorites mode.
    #[serde(default)]
    pub favorite_products: Vec<String>,
    /// TRANSIENT CDS key entry (Sys panel, masked). NEVER serialized:
    /// the key's one store is the standard `~/.cdsapirc` the cdsapi
    /// library reads — written by [`save_cds_key`] with visible consent.
    #[serde(skip)]
    pub cds_key_entry: String,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for StudioSettings {
    fn default() -> Self {
        let output_root = local_app_data()
            .join("ArWenStudio")
            .join("forecasts")
            .to_string_lossy()
            .into_owned();
        Self {
            schema: SETTINGS_SCHEMA.into(),
            contract_mode: "fixture".into(),
            fixture_dir: None,
            fixture_speed: 60.0,
            live_program: "gpuwm".into(),
            live_args_prefix: vec!["run-plan".into()],
            live_env: BTreeMap::new(),
            output_root,
            geog_root: None,
            rust_renderer: None,
            tile_style: "satellite".into(),
            favorite_products: Vec::new(),
            cds_key_entry: String::new(),
            extra: Default::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// CDS credentials: the ONE store is the standard ~/.cdsapirc (the file
// cdsapi and gpuwm's own presence check read). The key is never logged,
// never serialized into settings/plan/event JSON.
// ---------------------------------------------------------------------------

/// The real user home (the Studio process's, not the sealed child's).
pub fn real_user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn cds_credentials_path() -> Option<PathBuf> {
    real_user_home().map(|home| home.join(".cdsapirc"))
}

/// Presence only — never read for display (matching the engine's own
/// `cds_credentials_present`: validity is the CDS server's verdict).
pub fn cds_key_present() -> bool {
    cds_credentials_path()
        .map(|path| path.is_file())
        .unwrap_or(false)
}

/// The current CDS API endpoint the cdsapi docs name.
pub const CDS_API_URL: &str = "https://cds.climate.copernicus.eu/api";

/// Write `~/.cdsapirc` (url + key), the documented cdsapi format.
/// Returns the path written, for the consent line — never the key.
pub fn save_cds_key(key: &str) -> Result<PathBuf, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("the key field is empty".into());
    }
    let path = cds_credentials_path()
        .ok_or_else(|| "no user profile directory to write ~/.cdsapirc in".to_string())?;
    std::fs::write(&path, format!("url: {CDS_API_URL}\nkey: {key}\n"))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn settings_path() -> PathBuf {
    local_app_data().join("ArWenStudio").join("settings.json")
}

pub fn tile_cache_dir() -> PathBuf {
    local_app_data().join("ArWenStudio").join("tiles")
}

impl StudioSettings {
    pub fn load_or_default() -> Self {
        let path = settings_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// BowEcho's embedded host starts in live mode, but never pretends an
    /// engine exists. Existing ArWen Studio settings and runtime paths remain
    /// authoritative so users do not lose a configured engine or run history.
    pub fn load_for_bowecho() -> Self {
        let path = settings_path();
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(settings) = serde_json::from_str(&text)
        {
            return settings;
        }

        let mut settings = Self::default();
        settings.contract_mode = "live".into();
        let managed_python = local_app_data()
            .join("ArWenStudio")
            .join("engine")
            .join("venv")
            .join("Scripts")
            .join("python.exe");
        if managed_python.is_file() {
            settings.live_program = managed_python.to_string_lossy().into_owned();
            settings.live_args_prefix = vec![
                "-I".into(),
                "-B".into(),
                "-m".into(),
                "gpuwm.runplan".into(),
            ];
        }
        settings
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create settings dir: {error}"))?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| format!("serialize settings: {error}"))?;
        std::fs::write(&path, text).map_err(|error| format!("write settings: {error}"))
    }

    /// The fixture directory: explicit setting, else exe-relative
    /// `fixtures/`, else the dev workspace `fixtures/`.
    pub fn resolve_fixture_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.fixture_dir {
            let dir = PathBuf::from(dir);
            if fixture_dir_valid(&dir) {
                return Some(dir);
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            for ancestor in exe.ancestors().skip(1).take(5) {
                let candidate = ancestor.join("fixtures");
                if fixture_dir_valid(&candidate) {
                    return Some(candidate);
                }
            }
        }
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
        fixture_dir_valid(&dev).then_some(dev)
    }

    /// Renderer status for the UI: (label, healthy). A fallback is
    /// VISIBLE, never silent.
    pub fn renderer_status(&self) -> (String, bool) {
        match &self.rust_renderer {
            Some(path) if Path::new(path).is_file() => {
                (format!("native rw_wrfbatch ({path})"), true)
            }
            Some(path) => (
                format!("CONFIGURED BUT MISSING: {path} — engine will fall back to matplotlib"),
                false,
            ),
            None => (
                "not configured — engine renders product PNGs via its matplotlib fallback".into(),
                false,
            ),
        }
    }

    /// Build the contract source this Studio talks through.
    pub fn contract_source(&self) -> Result<ContractSource, String> {
        if self.contract_mode == "live" {
            let mut env = self.live_env.clone();
            if let Some(renderer) = &self.rust_renderer {
                env.insert("GPUWM_RW_WRFBATCH".into(), renderer.clone());
            }
            Ok(ContractSource::Live {
                spec: LauncherSpec {
                    program: PathBuf::from(&self.live_program),
                    args_prefix: self.live_args_prefix.clone(),
                    env,
                    // Never mirror credentials or caches into the shared
                    // ProgramData fallback. Each OS user gets a private
                    // engine HOME beneath LocalAppData instead.
                    private_runtime: Some(
                        local_app_data()
                            .join("ArWenStudio")
                            .join("engine")
                            .join("runtime"),
                    ),
                },
            })
        } else {
            let fixture_dir = self.resolve_fixture_dir().ok_or_else(|| {
                "no fixtures directory found (looked exe-relative and in the dev workspace)"
                    .to_string()
            })?;
            Ok(ContractSource::Fixture {
                fixture_dir,
                speed: self.fixture_speed,
                runner: None,
            })
        }
    }
}

fn fixture_dir_valid(dir: &Path) -> bool {
    dir.join("probe.json").is_file() && dir.join("events.jsonl").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_fixture_mode_and_round_trip() {
        let settings = StudioSettings::default();
        assert_eq!(settings.contract_mode, "fixture");
        let text = serde_json::to_string(&settings).unwrap();
        let parsed: StudioSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, settings);
    }

    #[test]
    fn dev_workspace_fixtures_are_discoverable() {
        let settings = StudioSettings::default();
        let dir = settings
            .resolve_fixture_dir()
            .expect("dev fixtures present");
        assert!(dir.join("events.jsonl").is_file());
        assert!(matches!(
            settings.contract_source().unwrap(),
            ContractSource::Fixture { .. }
        ));
    }
}
