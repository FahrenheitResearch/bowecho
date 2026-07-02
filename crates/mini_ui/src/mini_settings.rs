//! MiniSettings — miniDerecho's own settings document (miniderecho-spec
//! §1 B4, §11 M1). Lives at `settings::config_path_for_namespace
//! ("miniderecho")` and NEVER touches `AppSettings::load/save` or
//! BowEcho's `config.json`. Eq-derivable (no float fields — the tilt
//! preference is tenths of a degree) and unknown-key tolerant (forward
//! compatibility: a newer mini's keys must not brick an older one).
//!
//! Module note: named `mini_settings` (not the spec module map's
//! `settings.rs`) so paths to the workspace `settings` crate stay
//! unshadowed inside this bin.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MiniSettings {
    /// Last-used site as a `SiteRef::settings_key()` string.
    pub last_site: Option<String>,
    /// Selected product as a [`crate::products::Product::key`] string.
    pub product: String,
    /// Preferred tilt elevation in tenths of a degree (Eq-safe integer);
    /// resolved against each volume's tilt list by nearest match.
    pub tilt_deg_tenths: Option<i32>,
    /// Basemap tile style as a `ui_core::tiles::TileStyle::key` string.
    pub tile_style: String,
    /// First-run IP geolocation kill-switch (spec §3.2): off ⇒ the
    /// fallback chain skips straight past geolocation.
    pub ip_geolocation: bool,
}

impl Default for MiniSettings {
    fn default() -> Self {
        Self {
            last_site: None,
            product: "ref".to_owned(),
            tilt_deg_tenths: None,
            // Default basemap: satellite imagery reads best dark under the
            // radar (DarkVector is tile-less until the M4 vector crate).
            tile_style: "satellite".to_owned(),
            ip_geolocation: true,
        }
    }
}

impl MiniSettings {
    /// The B4 settings home: `<config>/miniderecho/config.json`. `None`
    /// only when the platform has no config directory at all.
    pub fn config_path() -> Option<PathBuf> {
        settings::config_path_for_namespace("miniderecho")
    }

    pub fn load() -> Self {
        Self::config_path()
            .map(|path| Self::load_from(&path))
            .unwrap_or_default()
    }

    /// Total load: a missing/corrupt document is a fresh default, never a
    /// crash — first-run must stay zero-config.
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(path) = Self::config_path() {
            self.save_to(&path);
        }
    }

    /// Best-effort persist (settings loss degrades to first-run behavior;
    /// it must never take the app down).
    pub fn save_to(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join("miniderecho-settings-tests")
            .join(format!("{name}-{}", std::process::id()))
            .join("config.json")
    }

    #[test]
    fn settings_round_trip_through_disk_is_eq() {
        let path = temp_path("round-trip");
        let settings = MiniSettings {
            last_site: Some("KEAX".to_owned()),
            product: "dvel".to_owned(),
            tilt_deg_tenths: Some(9),
            tile_style: "topo".to_owned(),
            ip_geolocation: false,
        };
        settings.save_to(&path);
        assert_eq!(MiniSettings::load_from(&path), settings);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_keys_and_missing_keys_are_tolerated() {
        // A FUTURE mini's document: extra keys ignored, known keys kept.
        let future: MiniSettings = serde_json::from_str(
            r#"{
                "last_site": "intl:smhi:angelholm",
                "product": "srv",
                "loop_length": 12,
                "warning_families": ["tornado"],
                "tile_style": "streets"
            }"#,
        )
        .expect("unknown keys must not fail the parse");
        assert_eq!(future.last_site.as_deref(), Some("intl:smhi:angelholm"));
        assert_eq!(future.product, "srv");
        assert_eq!(future.tile_style, "streets");
        // Missing keys take defaults.
        assert_eq!(future.tilt_deg_tenths, None);
        assert!(future.ip_geolocation);

        // The empty document is exactly the default settings.
        let empty: MiniSettings = serde_json::from_str("{}").expect("empty document parses");
        assert_eq!(empty, MiniSettings::default());
    }

    #[test]
    fn corrupt_or_missing_documents_load_as_defaults() {
        let path = temp_path("corrupt");
        assert_eq!(MiniSettings::load_from(&path), MiniSettings::default());
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(MiniSettings::load_from(&path), MiniSettings::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// B4: mini's document lives under the miniderecho namespace — the
    /// path is the settings crate's namespaced helper, never BowEcho's
    /// `config.json` root.
    #[test]
    fn config_path_is_the_namespaced_b4_path() {
        assert_eq!(
            MiniSettings::config_path(),
            settings::config_path_for_namespace("miniderecho")
        );
        if let Some(path) = MiniSettings::config_path() {
            let text = path.display().to_string();
            assert!(text.contains("miniderecho"), "{text}");
            assert!(text.ends_with("config.json"), "{text}");
        }
    }
}
