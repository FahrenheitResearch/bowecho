//! Versioned, bounded sidebar-layout persistence and the stable section catalog.
//!
//! The settings crate stores this document as opaque JSON.  Keeping the typed
//! schema here lets the UI evolve without coupling `settings` to egui or to the
//! app's section enum, while still preserving unknown section slugs for a newer
//! BowEcho build.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const LAYOUT_VERSION: u16 = 1;
pub(crate) const MAX_CUSTOM_TABS: usize = 8;
pub(crate) const MAX_CUSTOM_SECTIONS: usize = 24;
const MAX_BUILTIN_ENTRIES: usize = 64;
const MAX_TITLE_CHARS: usize = 32;
const MAX_SECTION_SLUG_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum BuiltinTab {
    Radar,
    Map,
    Alerts,
    Data,
    Settings,
}

impl BuiltinTab {
    pub(crate) const ALL: [Self; 5] = [
        Self::Radar,
        Self::Map,
        Self::Alerts,
        Self::Data,
        Self::Settings,
    ];

    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Radar => "radar",
            Self::Map => "map",
            Self::Alerts => "alerts",
            Self::Data => "data",
            Self::Settings => "settings",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Radar => "Radar",
            Self::Map => "Map",
            Self::Alerts => "Alerts",
            Self::Data => "Data",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RadarPreset {
    Classic,
    Compact,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditorTarget {
    Builtin(BuiltinTab),
    Custom(u32),
}

impl Default for EditorTarget {
    fn default() -> Self {
        Self::Builtin(BuiltinTab::Radar)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderScope {
    Builtin,
    Custom(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RenderContext {
    pub section: SectionId,
    pub scope: RenderScope,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SectionDestination {
    Builtin,
    Custom(u32),
    ForcedBuiltin,
}

impl RadarPreset {
    pub(crate) const fn slug(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Compact => "compact",
        }
    }

    pub(crate) fn from_slug(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "compact" | "minimal" | "modern" => Self::Compact,
            _ => Self::Classic,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum SectionId {
    RadarWorkspace,
    RadarPlayback,
    RadarProducts,
    RadarTilts,
    RadarSite,
    RadarAlgorithms,
    RadarTools,
    MapLayers,
    MapAnalysis,
    MapAppearance,
    AlertsControls,
    AlertsCurrent,
    AlertsOutlooks,
    AlertsFeed,
    AlertsLocalFile,
    DataRadarArchive,
    DataTornadoArchive,
    DataPacks,
    DataCommunityCases,
    DataRadarCoverage,
    DataGridComposites,
    DataLiveFeeds,
    DataModelStore,
    DataLocalFiles,
    SettingsDisplay,
    SettingsSidebarLayout,
    SettingsRadarProducts,
    SettingsBrand,
    SettingsSecurityUpdates,
    SettingsHotkeys,
    SettingsAlerts,
    SettingsBackup,
    SettingsCommunityCache,
    SettingsFederation,
    SettingsPublication,
    SettingsPerformance,
    SettingsDebugCases,
    SettingsModel,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SectionSpec {
    pub id: SectionId,
    pub slug: &'static str,
    pub title: &'static str,
    pub tab: BuiltinTab,
    pub default_open: bool,
    /// Existing persistence key.  These never change: retaining them is what
    /// keeps a user's pre-customization open/closed state across the upgrade.
    pub legacy_key: &'static str,
}

macro_rules! section_specs {
    ($(($variant:ident, $slug:literal, $title:literal, $tab:ident, $open:literal, $key:literal)),+ $(,)?) => {
        pub(crate) const SECTION_SPECS: &[SectionSpec] = &[
            $(SectionSpec {
                id: SectionId::$variant,
                slug: $slug,
                title: $title,
                tab: BuiltinTab::$tab,
                default_open: $open,
                legacy_key: $key,
            }),+
        ];
    };
}

section_specs!(
    (
        RadarWorkspace,
        "radar.workspace",
        "Pane layout & independent views",
        Radar,
        true,
        "radar_workspace"
    ),
    (
        RadarPlayback,
        "radar.playback",
        "Playback",
        Radar,
        true,
        "radar_loop"
    ),
    (
        RadarProducts,
        "radar.products",
        "Products",
        Radar,
        true,
        "radar_products"
    ),
    (RadarTilts, "radar.tilts", "Tilt", Radar, true, "radar_tilt"),
    (RadarSite, "radar.site", "Site", Radar, true, "radar_site"),
    (
        RadarAlgorithms,
        "radar.algorithms",
        "Algorithms",
        Radar,
        true,
        "radar_algorithms"
    ),
    (
        RadarTools,
        "radar.tools",
        "Tools",
        Radar,
        true,
        "radar_tools"
    ),
    (
        MapLayers,
        "map.layers",
        "Map layers",
        Map,
        true,
        "customize_map_layers"
    ),
    (
        MapAnalysis,
        "map.analysis",
        "Analysis overlays",
        Map,
        false,
        "customize_analysis_overlays"
    ),
    (
        MapAppearance,
        "map.appearance",
        "Appearance",
        Map,
        false,
        "customize_appearance"
    ),
    (
        AlertsControls,
        "alerts.controls",
        "Alert controls",
        Alerts,
        true,
        "severe_controls"
    ),
    (
        AlertsCurrent,
        "alerts.current",
        "Current alerts",
        Alerts,
        true,
        "severe_current_alerts"
    ),
    (
        AlertsOutlooks,
        "alerts.outlooks",
        "Outlooks",
        Alerts,
        true,
        "severe_spc_outlooks"
    ),
    (
        AlertsFeed,
        "alerts.feed",
        "Warning feed",
        Alerts,
        false,
        "severe_warning_feed"
    ),
    (
        AlertsLocalFile,
        "alerts.local_file",
        "Local file",
        Alerts,
        false,
        "severe_local_file"
    ),
    (
        DataRadarArchive,
        "data.radar_archive",
        "Radar archive",
        Data,
        true,
        "data_archive"
    ),
    (
        DataTornadoArchive,
        "data.tornado_archive",
        "Tornado archive (SPC)",
        Data,
        true,
        "data_event_day"
    ),
    (
        DataPacks,
        "data.packs",
        "Data packs",
        Data,
        true,
        "data_packs"
    ),
    (
        DataCommunityCases,
        "data.community_cases",
        "Community cases",
        Data,
        false,
        "data_community_cases"
    ),
    (
        DataRadarCoverage,
        "data.radar_coverage",
        "Radar coverage",
        Data,
        true,
        "data_radar_coverage"
    ),
    (
        DataGridComposites,
        "data.grid_composites",
        "Grid / Composites",
        Data,
        true,
        "data_grid_composites"
    ),
    (
        DataLiveFeeds,
        "data.live_feeds",
        "Live feeds",
        Data,
        true,
        "data_live_feeds"
    ),
    (
        DataModelStore,
        "data.model_store",
        "Model store",
        Data,
        true,
        "data_model_store"
    ),
    (
        DataLocalFiles,
        "data.local_files",
        "Local files",
        Data,
        true,
        "data_local"
    ),
    (
        SettingsDisplay,
        "settings.display",
        "Display",
        Settings,
        true,
        "settings_display"
    ),
    (
        SettingsSidebarLayout,
        "settings.sidebar_layout",
        "Sidebar & custom tabs",
        Settings,
        false,
        "settings_sidebar_layout"
    ),
    (
        SettingsRadarProducts,
        "settings.radar_products",
        "Radar products",
        Settings,
        false,
        "settings_radar_products"
    ),
    (
        SettingsBrand,
        "settings.brand",
        "App Identity / Brand Kit",
        Settings,
        false,
        "settings_brand"
    ),
    (
        SettingsSecurityUpdates,
        "settings.security_updates",
        "Security & updates",
        Settings,
        false,
        "settings_security_updates"
    ),
    (
        SettingsHotkeys,
        "settings.hotkeys",
        "Hotkeys",
        Settings,
        false,
        "settings_hotkeys"
    ),
    (
        SettingsAlerts,
        "settings.alerts",
        "Alerts",
        Settings,
        false,
        "settings_alerts"
    ),
    (
        SettingsBackup,
        "settings.backup",
        "Settings backup",
        Settings,
        false,
        "settings_backup"
    ),
    (
        SettingsCommunityCache,
        "settings.community_cache",
        "Community Cache",
        Settings,
        false,
        "settings_community_cache"
    ),
    (
        SettingsFederation,
        "settings.federation",
        "Public origin federation",
        Settings,
        false,
        "settings_public_origin_federation"
    ),
    (
        SettingsPublication,
        "settings.publication",
        "Owner generation publication",
        Settings,
        false,
        "settings_generation_publication"
    ),
    (
        SettingsPerformance,
        "settings.performance",
        "Performance",
        Settings,
        false,
        "settings_performance"
    ),
    (
        SettingsDebugCases,
        "settings.debug_cases",
        "Debug cases",
        Settings,
        false,
        "settings_debug_cases"
    ),
    (
        SettingsModel,
        "settings.model",
        "Model",
        Settings,
        false,
        "settings_model"
    ),
);

impl SectionId {
    pub(crate) fn spec(self) -> &'static SectionSpec {
        SECTION_SPECS
            .iter()
            .find(|spec| spec.id == self)
            .expect("every SectionId has registry metadata")
    }

    pub(crate) fn from_slug(slug: &str) -> Option<Self> {
        SECTION_SPECS
            .iter()
            .find(|spec| spec.slug == slug)
            .map(|spec| spec.id)
    }

    pub(crate) fn from_legacy_key(key: &str) -> Option<Self> {
        SECTION_SPECS
            .iter()
            .find(|spec| spec.legacy_key == key)
            .map(|spec| spec.id)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct BuiltinLayout {
    pub order: Vec<String>,
    pub hidden: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CustomTab {
    pub id: u32,
    pub title: String,
    pub sections: Vec<String>,
}

impl Default for CustomTab {
    fn default() -> Self {
        Self {
            id: 1,
            title: "My tab".to_owned(),
            sections: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct LayoutDocument {
    pub version: u16,
    pub radar_preset: String,
    pub builtins: BTreeMap<String, BuiltinLayout>,
    pub custom_tabs: Vec<CustomTab>,
}

impl Default for LayoutDocument {
    fn default() -> Self {
        Self {
            version: LAYOUT_VERSION,
            radar_preset: RadarPreset::Classic.slug().to_owned(),
            builtins: BTreeMap::new(),
            custom_tabs: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedLayout {
    pub document: LayoutDocument,
    pub newer_schema: bool,
}

impl LoadedLayout {
    pub(crate) fn load(value: Option<&serde_json::Value>) -> Self {
        let Some(value) = value else {
            return Self {
                document: LayoutDocument::default(),
                newer_schema: false,
            };
        };
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(u64::from(LAYOUT_VERSION));
        if version > u64::from(LAYOUT_VERSION) {
            return Self {
                document: LayoutDocument::default(),
                newer_schema: true,
            };
        }
        let mut document = serde_json::from_value::<LayoutDocument>(value.clone())
            .unwrap_or_else(|_| LayoutDocument::default());
        document.normalize();
        Self {
            document,
            newer_schema: false,
        }
    }
}

impl LayoutDocument {
    pub(crate) fn radar_preset(&self) -> RadarPreset {
        RadarPreset::from_slug(&self.radar_preset)
    }

    pub(crate) fn set_radar_preset(&mut self, preset: RadarPreset) {
        self.radar_preset = preset.slug().to_owned();
    }

    pub(crate) fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("sidebar layout is serializable")
    }

    pub(crate) fn custom_tab(&self, id: u32) -> Option<&CustomTab> {
        self.custom_tabs.iter().find(|tab| tab.id == id)
    }

    pub(crate) fn next_custom_id(&self) -> u32 {
        let used = self
            .custom_tabs
            .iter()
            .map(|tab| tab.id)
            .filter(|id| *id != 0)
            .collect::<BTreeSet<_>>();
        first_free_custom_id(&used)
    }

    pub(crate) fn create_custom_tab(&mut self) -> Option<u32> {
        if self.custom_tabs.len() >= MAX_CUSTOM_TABS {
            return None;
        }
        let id = self.next_custom_id();
        self.custom_tabs.push(CustomTab {
            id,
            title: format!("My tab {}", self.custom_tabs.len() + 1),
            sections: Vec::new(),
        });
        Some(id)
    }

    pub(crate) fn resolved_builtin(&self, tab: BuiltinTab) -> Vec<SectionId> {
        let defaults = SECTION_SPECS
            .iter()
            .filter(|spec| spec.tab == tab)
            .map(|spec| spec.id)
            .collect::<Vec<_>>();
        let Some(layout) = self.builtins.get(tab.slug()) else {
            return defaults;
        };
        let hidden = layout
            .hidden
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut seen = BTreeSet::new();
        let mut resolved = Vec::new();
        for slug in &layout.order {
            let Some(id) = SectionId::from_slug(slug) else {
                continue;
            };
            let spec = id.spec();
            if spec.tab == tab && !hidden.contains(spec.slug) && seen.insert(id) {
                resolved.push(id);
            }
        }
        // New sections that did not exist when the user customized the tab
        // append in the product's default order instead of vanishing forever.
        for id in defaults {
            let spec = id.spec();
            if !hidden.contains(spec.slug) && seen.insert(id) {
                resolved.push(id);
            }
        }
        resolved
    }

    pub(crate) fn builtin_is_customized(&self, tab: BuiltinTab) -> bool {
        self.builtins
            .get(tab.slug())
            .is_some_and(|layout| !layout.order.is_empty() || !layout.hidden.is_empty())
    }

    pub(crate) fn destination_for(&self, section: SectionId) -> SectionDestination {
        let spec = section.spec();
        if self.resolved_builtin(spec.tab).contains(&section) {
            return SectionDestination::Builtin;
        }
        self.custom_tabs
            .iter()
            .find(|tab| tab.sections.iter().any(|slug| slug == spec.slug))
            .map_or(SectionDestination::ForcedBuiltin, |tab| {
                SectionDestination::Custom(tab.id)
            })
    }

    pub(crate) fn normalize(&mut self) {
        self.version = LAYOUT_VERSION;
        self.radar_preset = self.radar_preset().slug().to_owned();
        let mut used_ids = BTreeSet::new();
        self.custom_tabs.truncate(MAX_CUSTOM_TABS);
        for (index, tab) in self.custom_tabs.iter_mut().enumerate() {
            if tab.id == 0 || !used_ids.insert(tab.id) {
                tab.id = first_free_custom_id(&used_ids);
                used_ids.insert(tab.id);
            }
            tab.title = normalized_title(&tab.title, index + 1);
            normalize_slug_list(&mut tab.sections, MAX_CUSTOM_SECTIONS);
        }
        self.builtins.retain(|key, layout| {
            if !BuiltinTab::ALL.iter().any(|tab| tab.slug() == key) {
                return false;
            }
            normalize_slug_list(&mut layout.order, MAX_BUILTIN_ENTRIES);
            normalize_slug_list(&mut layout.hidden, MAX_BUILTIN_ENTRIES);
            true
        });
    }
}

fn first_free_custom_id(used: &BTreeSet<u32>) -> u32 {
    // At most eight IDs survive normalization, so the pigeonhole principle
    // guarantees a free value in 1..=9. Starting at one also handles a
    // persisted u32::MAX without saturating or looping forever.
    (1..=(MAX_CUSTOM_TABS as u32 + 1))
        .find(|candidate| !used.contains(candidate))
        .expect("bounded custom tabs always leave a free positive id")
}

pub(crate) fn custom_section_key(tab_id: u32, section: SectionId) -> String {
    format!("custom:{tab_id}:{}", section.spec().slug)
}

fn normalized_title(value: &str, index: usize) -> String {
    let trimmed = value.trim();
    let source = if trimmed.is_empty() {
        format!("My tab {index}")
    } else {
        trimmed.to_owned()
    };
    source.chars().take(MAX_TITLE_CHARS).collect()
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SECTION_SLUG_BYTES
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn normalize_slug_list(values: &mut Vec<String>, maximum: usize) {
    let mut seen = BTreeSet::new();
    values.retain(|value| valid_slug(value) && seen.insert(value.clone()));
    values.truncate(maximum);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_is_classic_and_empty() {
        let loaded = LoadedLayout::load(None);
        assert_eq!(loaded.document.radar_preset(), RadarPreset::Classic);
        assert!(loaded.document.custom_tabs.is_empty());
        assert!(!loaded.newer_schema);
    }

    #[test]
    fn classic_radar_registry_order_matches_pre_minimal_layout() {
        assert_eq!(
            LayoutDocument::default().resolved_builtin(BuiltinTab::Radar),
            vec![
                SectionId::RadarWorkspace,
                SectionId::RadarPlayback,
                SectionId::RadarProducts,
                SectionId::RadarTilts,
                SectionId::RadarSite,
                SectionId::RadarAlgorithms,
                SectionId::RadarTools,
            ]
        );
    }

    #[test]
    fn builtin_override_orders_hides_and_appends_unmentioned_defaults() {
        let mut document = LayoutDocument::default();
        document.builtins.insert(
            "radar".to_owned(),
            BuiltinLayout {
                order: vec!["radar.site".to_owned(), "radar.playback".to_owned()],
                hidden: vec!["radar.tools".to_owned()],
            },
        );
        let resolved = document.resolved_builtin(BuiltinTab::Radar);
        assert_eq!(resolved[0], SectionId::RadarSite);
        assert_eq!(resolved[1], SectionId::RadarPlayback);
        assert!(!resolved.contains(&SectionId::RadarTools));
        assert!(resolved.contains(&SectionId::RadarProducts));
    }

    #[test]
    fn custom_state_is_bounded_deduped_and_unknown_sections_survive() {
        let mut document = LayoutDocument::default();
        for id in 0..12_u32 {
            document.custom_tabs.push(CustomTab {
                id,
                title: "  an extremely long custom sidebar tab title that must be trimmed  "
                    .to_owned(),
                sections: vec![
                    "radar.products".to_owned(),
                    "radar.products".to_owned(),
                    "future.experimental".to_owned(),
                    "INVALID SPACE".to_owned(),
                ],
            });
        }
        document.normalize();
        assert_eq!(document.custom_tabs.len(), MAX_CUSTOM_TABS);
        assert!(
            document
                .custom_tabs
                .iter()
                .all(|tab| tab.title.chars().count() <= MAX_TITLE_CHARS)
        );
        assert!(
            document
                .custom_tabs
                .iter()
                .all(|tab| tab.sections == ["radar.products", "future.experimental"])
        );
        let ids = document
            .custom_tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), document.custom_tabs.len());
        assert!(!ids.contains(&0));
    }

    #[test]
    fn newer_schema_is_not_interpreted_or_downgraded() {
        let raw = serde_json::json!({"version": LAYOUT_VERSION + 1, "future": true});
        let loaded = LoadedLayout::load(Some(&raw));
        assert!(loaded.newer_schema);
        assert_eq!(loaded.document, LayoutDocument::default());
    }

    #[test]
    fn registry_slugs_keys_and_ids_are_unique() {
        let mut slugs = BTreeSet::new();
        let mut keys = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for spec in SECTION_SPECS {
            assert!(slugs.insert(spec.slug));
            assert!(keys.insert(spec.legacy_key));
            assert!(ids.insert(spec.id));
            assert!(!spec.title.is_empty());
            assert_eq!(SectionId::from_slug(spec.slug), Some(spec.id));
            assert_eq!(SectionId::from_legacy_key(spec.legacy_key), Some(spec.id));
        }
    }

    #[test]
    fn custom_collapse_keys_are_scoped_by_tab() {
        assert_ne!(
            custom_section_key(1, SectionId::RadarPlayback),
            custom_section_key(2, SectionId::RadarPlayback)
        );
    }

    #[test]
    fn zero_duplicate_and_max_custom_ids_normalize_without_stalling() {
        let mut document = LayoutDocument::default();
        for id in [u32::MAX, u32::MAX, 0, 1, 1, 2, 3, 4] {
            document.custom_tabs.push(CustomTab {
                id,
                title: String::new(),
                sections: Vec::new(),
            });
        }
        document.normalize();
        let ids = document
            .custom_tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), document.custom_tabs.len());
        assert!(!ids.contains(&0));
        assert!(ids.contains(&u32::MAX));
        assert!(document.next_custom_id() > 0);
        assert!(!ids.contains(&document.next_custom_id()));
    }

    #[test]
    fn workflow_destination_prefers_builtin_then_custom_then_one_shot_fallback() {
        let section = SectionId::SettingsSidebarLayout;
        let mut document = LayoutDocument::default();
        assert_eq!(
            document.destination_for(section),
            SectionDestination::Builtin
        );

        document.builtins.insert(
            BuiltinTab::Settings.slug().to_owned(),
            BuiltinLayout {
                order: Vec::new(),
                hidden: vec![section.spec().slug.to_owned()],
            },
        );
        assert_eq!(
            document.destination_for(section),
            SectionDestination::ForcedBuiltin
        );

        document.custom_tabs.push(CustomTab {
            id: 42,
            title: "Operator".to_owned(),
            sections: vec![section.spec().slug.to_owned()],
        });
        assert_eq!(
            document.destination_for(section),
            SectionDestination::Custom(42)
        );
    }
}
