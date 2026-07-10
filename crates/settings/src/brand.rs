//! Persisted application identity and Brand Kit presets.
//!
//! This module is UI-toolkit agnostic. Runtime egui/image adaptation lives in
//! `app_ui::brand`; settings only owns the source-of-truth document, validation,
//! and backwards-compatible defaults.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const BRAND_KIT_SCHEMA: u32 = 1;
pub const DEFAULT_STORAGE_NAMESPACE: &str = "bowecho";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrandPreset {
    #[default]
    BowEcho,
    GenericBrandedApp,
    Custom,
}

impl BrandPreset {
    pub const BUILT_INS: [Self; 2] = [Self::BowEcho, Self::GenericBrandedApp];

    pub fn label(self) -> &'static str {
        match self {
            Self::BowEcho => "BowEcho default",
            Self::GenericBrandedApp => "Generic branded app",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareLayout {
    #[default]
    Original,
    Landscape16x9,
    Square1x1,
    Portrait9x16,
}

impl ShareLayout {
    pub const ALL: [Self; 4] = [
        Self::Original,
        Self::Landscape16x9,
        Self::Square1x1,
        Self::Portrait9x16,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::Landscape16x9 => "16:9",
            Self::Square1x1 => "1:1",
            Self::Portrait9x16 => "9:16",
        }
    }

    pub fn ratio(self) -> Option<(usize, usize)> {
        match self {
            Self::Original => None,
            Self::Landscape16x9 => Some((16, 9)),
            Self::Square1x1 => Some((1, 1)),
            Self::Portrait9x16 => Some((9, 16)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrandPalette {
    pub primary: String,
    pub accent: String,
    pub danger: String,
    pub warning: String,
    pub success: String,
    pub surface: String,
    pub surface_alt: String,
    pub text: String,
    pub muted_text: String,
    pub outline: String,
}

impl Default for BrandPalette {
    fn default() -> Self {
        Self {
            primary: "#30587E".to_owned(),
            accent: "#78AAD2".to_owned(),
            danger: "#F83E52".to_owned(),
            warning: "#F6B739".to_owned(),
            success: "#6EF582".to_owned(),
            surface: "#0E0F11".to_owned(),
            surface_alt: "#16181B".to_owned(),
            text: "#CDD2D8".to_owned(),
            muted_text: "#9AA8B8".to_owned(),
            outline: "#282C32".to_owned(),
        }
    }
}

impl BrandPalette {
    pub fn generic_branded_app() -> Self {
        Self {
            primary: "#2F80ED".to_owned(),
            accent: "#56CCF2".to_owned(),
            danger: "#EB5757".to_owned(),
            warning: "#F2C94C".to_owned(),
            success: "#27AE60".to_owned(),
            surface: "#101418".to_owned(),
            surface_alt: "#182027".to_owned(),
            text: "#F8FAFC".to_owned(),
            muted_text: "#AEB8C2".to_owned(),
            outline: "#34414D".to_owned(),
        }
    }

    pub fn resolved(&self, fallback: &Self) -> ResolvedBrandPalette {
        ResolvedBrandPalette {
            primary: resolved_color(&self.primary, &fallback.primary, [48, 88, 126]),
            accent: resolved_color(&self.accent, &fallback.accent, [120, 170, 210]),
            danger: resolved_color(&self.danger, &fallback.danger, [248, 62, 82]),
            warning: resolved_color(&self.warning, &fallback.warning, [246, 183, 57]),
            success: resolved_color(&self.success, &fallback.success, [110, 245, 130]),
            surface: resolved_color(&self.surface, &fallback.surface, [14, 15, 17]),
            surface_alt: resolved_color(&self.surface_alt, &fallback.surface_alt, [22, 24, 27]),
            text: resolved_color(&self.text, &fallback.text, [205, 210, 216]),
            muted_text: resolved_color(&self.muted_text, &fallback.muted_text, [154, 168, 184]),
            outline: resolved_color(&self.outline, &fallback.outline, [40, 44, 50]),
        }
    }

    fn normalized_with_fallback(&self, fallback: &Self) -> Self {
        fn normalized(value: &str, fallback: &str) -> String {
            normalize_hex_color(value)
                .or_else(|| normalize_hex_color(fallback))
                .unwrap_or_else(|| "#000000".to_owned())
        }

        Self {
            primary: normalized(&self.primary, &fallback.primary),
            accent: normalized(&self.accent, &fallback.accent),
            danger: normalized(&self.danger, &fallback.danger),
            warning: normalized(&self.warning, &fallback.warning),
            success: normalized(&self.success, &fallback.success),
            surface: normalized(&self.surface, &fallback.surface),
            surface_alt: normalized(&self.surface_alt, &fallback.surface_alt),
            text: normalized(&self.text, &fallback.text),
            muted_text: normalized(&self.muted_text, &fallback.muted_text),
            outline: normalized(&self.outline, &fallback.outline),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedBrandPalette {
    pub primary: [u8; 3],
    pub accent: [u8; 3],
    pub danger: [u8; 3],
    pub warning: [u8; 3],
    pub success: [u8; 3],
    pub surface: [u8; 3],
    pub surface_alt: [u8; 3],
    pub text: [u8; 3],
    pub muted_text: [u8; 3],
    pub outline: [u8; 3],
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrandAssets {
    /// Runtime launch icon. PNG is recommended; changing this cannot rewrite
    /// an already-built executable resource.
    pub app_icon_png: Option<String>,
    /// Build-kit hint. `app_ui/build.rs` reads `BOWECHO_APP_ICON_ICO` because a
    /// runtime config file cannot alter the Windows executable resource.
    pub app_icon_ico: Option<String>,
    pub header_logo: Option<String>,
    pub social_watermark: Option<String>,
    pub share_card_background: Option<String>,
}

impl BrandAssets {
    pub fn existing_file(value: &Option<String>) -> Option<PathBuf> {
        existing_path(value.as_deref())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrandFeatureLabels {
    pub radar: String,
    pub map: String,
    pub warnings: String,
    pub evacuation: String,
    pub air_quality: String,
}

impl Default for BrandFeatureLabels {
    fn default() -> Self {
        Self {
            radar: "Radar".to_owned(),
            // Renamed from "Custom"/"Severe" (sidebar UI refresh wave 2).
            // The sidebar remaps those legacy defaults at display time so
            // installs that merely persisted them pick up the rename.
            map: "Map".to_owned(),
            warnings: "Alerts".to_owned(),
            evacuation: "Evacuation".to_owned(),
            air_quality: "Air Quality".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareBranding {
    pub watermark_enabled: bool,
    pub card_enabled: bool,
    pub layout: ShareLayout,
    pub title: String,
    pub subtitle: String,
    pub site_label: String,
    pub source_footer: String,
}

impl Default for ShareBranding {
    fn default() -> Self {
        Self {
            watermark_enabled: false,
            card_enabled: false,
            layout: ShareLayout::Original,
            title: String::new(),
            subtitle: String::new(),
            site_label: String::new(),
            source_footer: String::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrandConfig {
    pub schema: u32,
    pub preset: BrandPreset,
    pub display_name: String,
    pub short_name: String,
    pub organization: String,
    pub tagline: String,
    pub website_url: String,
    pub repo_url: String,
    pub releases_url: String,
    pub support_url: String,
    pub donate_url: String,
    pub contact_url: String,
    pub privacy_url: String,
    pub use_custom_storage_namespace: bool,
    pub storage_namespace: String,
    pub screenshot_filename_prefix: String,
    pub output_folder_label: String,
    pub palette: BrandPalette,
    pub assets: BrandAssets,
    pub features: BrandFeatureLabels,
    pub sharing: ShareBranding,
}

impl Default for BrandConfig {
    fn default() -> Self {
        Self {
            schema: BRAND_KIT_SCHEMA,
            preset: BrandPreset::BowEcho,
            display_name: "BowEcho".to_owned(),
            short_name: "BowEcho".to_owned(),
            organization: "Fahrenheit Research".to_owned(),
            tagline: "Fast NEXRAD Level II radar viewer".to_owned(),
            website_url: "https://github.com/FahrenheitResearch/bowecho".to_owned(),
            repo_url: "https://github.com/FahrenheitResearch/bowecho".to_owned(),
            releases_url: "https://github.com/FahrenheitResearch/bowecho/releases".to_owned(),
            support_url: "https://github.com/FahrenheitResearch/bowecho/issues".to_owned(),
            donate_url: String::new(),
            contact_url: String::new(),
            privacy_url: String::new(),
            use_custom_storage_namespace: false,
            storage_namespace: DEFAULT_STORAGE_NAMESPACE.to_owned(),
            screenshot_filename_prefix: "bowecho".to_owned(),
            output_folder_label: "BowEcho".to_owned(),
            palette: BrandPalette::default(),
            assets: BrandAssets::default(),
            features: BrandFeatureLabels::default(),
            sharing: ShareBranding::default(),
        }
    }
}

impl BrandConfig {
    pub fn preset(preset: BrandPreset) -> Self {
        match preset {
            BrandPreset::BowEcho => Self::default(),
            BrandPreset::Custom => Self {
                preset: BrandPreset::Custom,
                ..Self::default()
            },
            BrandPreset::GenericBrandedApp => Self {
                schema: BRAND_KIT_SCHEMA,
                preset,
                display_name: "Generic Weather App".to_owned(),
                short_name: "Weather App".to_owned(),
                organization: "Your Organization".to_owned(),
                tagline: "Operational weather intelligence".to_owned(),
                website_url: String::new(),
                repo_url: String::new(),
                releases_url: String::new(),
                support_url: String::new(),
                donate_url: String::new(),
                contact_url: String::new(),
                privacy_url: String::new(),
                // The preset never relocates an existing installation. A user
                // or distributor must separately opt in to this namespace.
                use_custom_storage_namespace: false,
                storage_namespace: "branded_weather_app".to_owned(),
                screenshot_filename_prefix: "weather_app".to_owned(),
                output_folder_label: "Generic Weather App".to_owned(),
                palette: BrandPalette::generic_branded_app(),
                assets: BrandAssets::default(),
                features: BrandFeatureLabels {
                    radar: "Radar".to_owned(),
                    map: "Map".to_owned(),
                    warnings: "Alerts".to_owned(),
                    evacuation: "Impacts".to_owned(),
                    air_quality: "Environment".to_owned(),
                },
                sharing: ShareBranding {
                    watermark_enabled: true,
                    card_enabled: true,
                    layout: ShareLayout::Landscape16x9,
                    title: "Generic Weather App".to_owned(),
                    subtitle: "Radar - Alerts - Operational context".to_owned(),
                    site_label: "example.org".to_owned(),
                    source_footer:
                        "Generic branded preset. Configure identity, links, assets, and data attribution before distribution."
                            .to_owned(),
                },
            },
        }
    }

    /// Distribution default used only when no config exists. Existing config
    /// files with no `brand` field always keep the historical BowEcho default.
    pub fn distribution_default() -> Self {
        Self::distribution_default_from(None, None)
    }

    /// Same policy as [`Self::distribution_default`], with values supplied by
    /// the app crate's build script. Runtime environment variables win, which
    /// keeps packaging overrides explicit and testable.
    pub fn distribution_default_from(
        build_brand: Option<&str>,
        build_namespace: Option<&str>,
    ) -> Self {
        let requested = std::env::var("BOWECHO_DEFAULT_BRAND")
            .ok()
            .or_else(|| build_brand.map(str::to_owned))
            .or_else(|| environment_or_build_value("BOWECHO_DEFAULT_BRAND"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let mut brand = match requested.as_str() {
            "generic"
            | "generic_branded_app"
            | "generic-branded-app"
            | "branded"
            | "branded_weather_app"
            | "branded-weather-app" => Self::preset(BrandPreset::GenericBrandedApp),
            _ => Self::default(),
        };
        let namespace = std::env::var("BOWECHO_STORAGE_NAMESPACE")
            .ok()
            .or_else(|| build_namespace.map(str::to_owned))
            .or_else(|| environment_or_build_value("BOWECHO_STORAGE_NAMESPACE"));
        if let Some(namespace) = namespace.as_deref().and_then(sanitize_namespace) {
            brand.storage_namespace = namespace;
            brand.use_custom_storage_namespace = true;
        }
        brand
    }

    pub fn is_default(value: &Self) -> bool {
        value == &Self::default()
    }

    pub fn mark_custom(&mut self) {
        self.preset = BrandPreset::Custom;
    }

    pub fn resolved_display_name(&self) -> &str {
        non_empty_or(&self.display_name, "BowEcho")
    }

    pub fn resolved_short_name(&self) -> &str {
        non_empty_or(&self.short_name, self.resolved_display_name())
    }

    pub fn resolved_tagline(&self) -> &str {
        non_empty_or(&self.tagline, "Fast NEXRAD Level II radar viewer")
    }

    pub fn filename_prefix(&self) -> String {
        sanitize_namespace(&self.screenshot_filename_prefix).unwrap_or_else(|| "bowecho".to_owned())
    }

    pub fn output_folder_name(&self) -> String {
        sanitize_folder_label(&self.output_folder_label).unwrap_or_else(|| "BowEcho".to_owned())
    }

    pub fn effective_storage_namespace(&self) -> Option<String> {
        if !self.use_custom_storage_namespace {
            return None;
        }
        sanitize_namespace(&self.storage_namespace)
            .filter(|namespace| namespace != DEFAULT_STORAGE_NAMESPACE)
    }

    pub fn palette_fallback(&self) -> BrandPalette {
        match self.preset {
            BrandPreset::GenericBrandedApp => BrandPalette::generic_branded_app(),
            BrandPreset::BowEcho | BrandPreset::Custom => BrandPalette::default(),
        }
    }

    pub fn resolved_palette(&self) -> ResolvedBrandPalette {
        self.palette.resolved(&self.palette_fallback())
    }

    pub fn normalized_for_load(mut self) -> Self {
        self.schema = BRAND_KIT_SCHEMA;
        let defaults = Self::preset(match self.preset {
            BrandPreset::Custom => BrandPreset::BowEcho,
            preset => preset,
        });
        if self.display_name.trim().is_empty() {
            self.display_name = defaults.display_name;
        }
        if self.short_name.trim().is_empty() {
            self.short_name = defaults.short_name;
        }
        if self.organization.trim().is_empty() {
            self.organization = defaults.organization;
        }
        if self.tagline.trim().is_empty() {
            self.tagline = defaults.tagline;
        }
        self.palette = self
            .palette
            .normalized_with_fallback(&self.palette_fallback());
        self
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str::<Self>(text)
            .map(Self::normalized_for_load)
            .map_err(|error| error.to_string())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn valid_http_url<'a>(&self, value: &'a str) -> Option<&'a str> {
        let value = value.trim();
        (value.starts_with("https://") || value.starts_with("http://")).then_some(value)
    }

    pub fn is_bowecho_named(&self) -> bool {
        self.resolved_display_name() == "BowEcho"
            && self.resolved_short_name() == "BowEcho"
            && self.filename_prefix() == "bowecho"
    }
}

pub fn parse_hex_color(value: &str) -> Option<[u8; 3]> {
    let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some([
        u8::from_str_radix(&value[0..2], 16).ok()?,
        u8::from_str_radix(&value[2..4], 16).ok()?,
        u8::from_str_radix(&value[4..6], 16).ok()?,
    ])
}

pub fn normalize_hex_color(value: &str) -> Option<String> {
    let [red, green, blue] = parse_hex_color(value)?;
    Some(format!("#{red:02X}{green:02X}{blue:02X}"))
}

pub fn sanitize_namespace(value: &str) -> Option<String> {
    let mut output = String::new();
    let mut previous_separator = false;
    for character in value.trim().chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_separator = false;
        } else if matches!(character, '-' | '_') && !previous_separator && !output.is_empty() {
            output.push(character);
            previous_separator = true;
        }
    }
    while output.ends_with('-') || output.ends_with('_') {
        output.pop();
    }
    (!output.is_empty()).then_some(output)
}

fn environment_or_build_value(key: &str) -> Option<String> {
    std::env::var(key).ok().or_else(|| match key {
        "BOWECHO_DEFAULT_BRAND" => option_env!("BOWECHO_DEFAULT_BRAND").map(str::to_owned),
        "BOWECHO_STORAGE_NAMESPACE" => option_env!("BOWECHO_STORAGE_NAMESPACE").map(str::to_owned),
        _ => None,
    })
}

fn resolved_color(value: &str, fallback: &str, hard_fallback: [u8; 3]) -> [u8; 3] {
    parse_hex_color(value)
        .or_else(|| parse_hex_color(fallback))
        .unwrap_or(hard_fallback)
}

fn existing_path(value: Option<&str>) -> Option<PathBuf> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    path.is_file().then_some(path)
}

fn sanitize_folder_label(value: &str) -> Option<String> {
    let output = value
        .trim()
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    (!output.is_empty() && output != "." && output != "..").then_some(output)
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_default_is_exact_bowecho_identity() {
        let brand = BrandConfig::default();
        assert_eq!(brand.resolved_display_name(), "BowEcho");
        assert_eq!(brand.resolved_short_name(), "BowEcho");
        assert_eq!(brand.filename_prefix(), "bowecho");
        assert_eq!(brand.output_folder_name(), "BowEcho");
        assert_eq!(brand.storage_namespace, DEFAULT_STORAGE_NAMESPACE);
        assert!(!brand.use_custom_storage_namespace);
        assert!(!brand.sharing.watermark_enabled);
        assert!(!brand.sharing.card_enabled);
    }

    #[test]
    fn brand_kit_json_round_trips_all_fields_and_paths() {
        let mut brand = BrandConfig::preset(BrandPreset::GenericBrandedApp);
        brand.preset = BrandPreset::Custom;
        brand.organization = "Example Operations".to_owned();
        brand.repo_url = "https://github.com/example/weather-desktop".to_owned();
        brand.releases_url = "https://github.com/example/weather-desktop/releases".to_owned();
        brand.donate_url = "https://example.test/donate".to_owned();
        brand.contact_url = "mailto:ops@example.test".to_owned();
        brand.privacy_url = "https://example.test/privacy".to_owned();
        brand.use_custom_storage_namespace = true;
        brand.assets = BrandAssets {
            app_icon_png: Some("C:\\brand\\icon.png".to_owned()),
            app_icon_ico: Some("C:\\brand\\icon.ico".to_owned()),
            header_logo: Some("assets/header.png".to_owned()),
            social_watermark: Some("assets/watermark.png".to_owned()),
            share_card_background: Some("assets/share.jpg".to_owned()),
        };
        brand.sharing.layout = ShareLayout::Portrait9x16;

        let json = brand.to_json();
        let loaded = BrandConfig::from_json(&json).expect("valid brand kit");

        assert_eq!(loaded, brand);
        assert_eq!(
            loaded.assets.social_watermark.as_deref(),
            Some("assets/watermark.png")
        );
    }

    #[test]
    fn brand_generic_preset_has_expected_identity_palette_and_assets() {
        let brand = BrandConfig::preset(BrandPreset::GenericBrandedApp);
        assert_eq!(brand.display_name, "Generic Weather App");
        assert_eq!(brand.short_name, "Weather App");
        assert_eq!(brand.tagline, "Operational weather intelligence");
        assert_eq!(brand.filename_prefix(), "weather_app");
        assert_eq!(brand.storage_namespace, "branded_weather_app");
        assert!(!brand.use_custom_storage_namespace);
        assert_eq!(
            brand.palette,
            BrandPalette {
                primary: "#2F80ED".to_owned(),
                accent: "#56CCF2".to_owned(),
                danger: "#EB5757".to_owned(),
                warning: "#F2C94C".to_owned(),
                success: "#27AE60".to_owned(),
                surface: "#101418".to_owned(),
                surface_alt: "#182027".to_owned(),
                text: "#F8FAFC".to_owned(),
                muted_text: "#AEB8C2".to_owned(),
                outline: "#34414D".to_owned(),
            }
        );
        assert_eq!(brand.resolved_palette().primary, [47, 128, 237]);
        assert_eq!(brand.features.map, "Map");
        assert_eq!(brand.features.evacuation, "Impacts");
        assert_eq!(brand.assets, BrandAssets::default());
        assert!(
            brand
                .sharing
                .source_footer
                .contains("Configure identity, links, assets")
        );
    }

    #[test]
    fn brand_invalid_color_and_missing_asset_fall_back_without_panic() {
        let missing = std::env::temp_dir().join(format!(
            "bowecho-brand-missing-{}-icon.png",
            std::process::id()
        ));
        let json = format!(
            r#"{{
                "palette": {{ "primary": "not-a-color" }},
                "assets": {{ "app_icon_png": {:?} }}
            }}"#,
            missing.display().to_string()
        );
        let brand = BrandConfig::from_json(&json).expect("safe fallbacks");

        assert_eq!(brand.palette.primary, BrandPalette::default().primary);
        assert_eq!(brand.resolved_palette().primary, [48, 88, 126]);
        assert!(BrandAssets::existing_file(&brand.assets.app_icon_png).is_none());
    }

    #[test]
    fn brand_path_helpers_do_not_require_assets_to_exist() {
        let mut brand = BrandConfig::default();
        brand.assets.header_logo = Some("relative/partner-logo.png".to_owned());
        let loaded = BrandConfig::from_json(&brand.to_json()).expect("valid path strings");
        assert_eq!(loaded.assets.header_logo, brand.assets.header_logo);
    }
}
