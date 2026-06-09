//! Persisted application settings: a small JSON document under the platform
//! config directory. Loading is best-effort — a missing or unreadable file
//! yields defaults so the app always starts.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Site to load on startup (e.g. "KEAX"). None = built-in default.
    pub startup_site: Option<String>,
    /// Favorite site ids, in user order.
    pub favorites: Vec<String>,
    /// Live auto-refresh poll interval (seconds).
    pub polling_interval_seconds: u64,
    /// Number of saved pane-layout slots.
    pub saved_layout_slots: usize,
    /// Selected color table per family label (family.label() -> table name).
    pub palette_by_family: BTreeMap<String, String>,
    /// Multi-pane grid layout pane count from the last session (1, 2 or 4).
    pub grid_pane_count: usize,
    /// Product hotkeys: number-row key ("0"-"9") -> product label (e.g.
    /// "REF", "VEL", "SRV", "RHO", "ZDR", "SW", "CREF", "ET", "VIL", "VILD",
    /// "PHI", "KDP", "AzShr", "Div"). Edit in config.json to customize.
    pub product_hotkeys: BTreeMap<String, String>,
}

/// Default number-row bindings (the classic analyst loadout).
pub fn default_product_hotkeys() -> BTreeMap<String, String> {
    [
        ("1", "REF"),
        ("2", "VEL"),
        ("3", "SRV"),
        ("4", "RHO"),
        ("5", "ZDR"),
        ("6", "SW"),
        ("7", "CREF"),
        ("8", "ET"),
        ("9", "VIL"),
        ("0", "VILD"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            startup_site: None,
            favorites: Vec::new(),
            polling_interval_seconds: 60,
            saved_layout_slots: 8,
            palette_by_family: BTreeMap::new(),
            grid_pane_count: 1,
            product_hotkeys: default_product_hotkeys(),
        }
    }
}

impl AppSettings {
    /// Platform config file path: `%APPDATA%\bowecho\config.json` on
    /// Windows, `$XDG_CONFIG_HOME`/`~/.config/...` on Linux,
    /// `~/Library/Application Support/...` on macOS.
    pub fn config_path() -> Option<PathBuf> {
        let dir = config_dir()?;
        Some(dir.join("bowecho").join("config.json"))
    }

    /// Load settings from `config_path()`, falling back to defaults on any
    /// missing-file / parse error.
    pub fn load() -> Self {
        Self::config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::from_json(&text))
            .unwrap_or_default()
    }

    /// Persist to `config_path()`, creating the parent directory. Returns an
    /// error string on failure (callers may log and ignore).
    pub fn save(&self) -> Result<(), String> {
        let path = Self::config_path().ok_or_else(|| "no config directory".to_owned())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, self.to_json()).map_err(|e| e.to_string())
    }

    pub fn from_json(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn add_favorite(&mut self, site: &str) {
        if !self.favorites.iter().any(|s| s.eq_ignore_ascii_case(site)) {
            self.favorites.push(site.to_ascii_uppercase());
        }
    }
}

fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(PathBuf::from)
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_eight_layout_slots() {
        assert_eq!(AppSettings::default().saved_layout_slots, 8);
    }

    #[test]
    fn json_round_trips_all_fields() {
        let mut s = AppSettings::default();
        s.startup_site = Some("KEAX".to_owned());
        s.add_favorite("ktwx");
        s.add_favorite("KTWX"); // dedup, case-insensitive
        s.polling_interval_seconds = 30;
        s.palette_by_family.insert(
            "Velocity / SRV".to_owned(),
            "Analyst Velocity HD".to_owned(),
        );
        let back = AppSettings::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(back.favorites, vec!["KTWX".to_owned()]);
    }

    #[test]
    fn unknown_or_missing_fields_fall_back_to_default() {
        let s = AppSettings::from_json(r#"{ "startup_site": "KDMX", "bogus": 1 }"#);
        assert_eq!(s.startup_site.as_deref(), Some("KDMX"));
        assert_eq!(s.polling_interval_seconds, 60);
        assert_eq!(s.saved_layout_slots, 8);
    }

    #[test]
    fn malformed_json_yields_default() {
        assert_eq!(
            AppSettings::from_json("not json {{"),
            AppSettings::default()
        );
    }
}
