//! Naming contract for synthesized per-level isobaric map fields.
//!
//! Every imported WRF run and every downloaded model hour carries five
//! isobaric `pressure3d` volumes (`temperature_iso`, `dewpoint_iso`, `u_iso`,
//! `v_iso`, `height_iso`; downloaded models substitute `rh_iso` when too few
//! dewpoint levels realize) — but the rw-ui field picker lists only
//! `Surface2D` variables, so none of that upper air was plottable on the map.
//! The Model Data dock now synthesizes per-level picker entries at DISPLAY
//! time (the 7e6fdee pattern: nothing on disk changes, existing stores gain
//! the fields without re-import). This module is the single source of truth
//! for how those synthesized fields are named:
//!
//! * a display LABEL for the picker ("Temperature 850 mb") — always contains
//!   a space, so it can never collide with a real store slug (same
//!   invertibility guarantee `wrf_fields` labels carry, test-enforced);
//! * a store-style SLUG ("temperature_850") used everywhere a real store
//!   variable name would flow (map layers, 🎨 product bindings, the Solar
//!   palette resolver) — the slugs deliberately match the
//!   `temperature_850`-pattern [`crate::solar_model_field_table`] is already
//!   level-aware for, so 850 mb temperature lands on Solar's 850 mb table,
//!   dewpoint/RH/wind-speed levels land on their unit-aware family tables,
//!   and heights land on the per-level scaled Generic ramp.
//!
//! The exposed level set is curated ([`ISO_PICKER_LEVELS_HPA`]): the classic
//! analysis surfaces 925/850/700/500/300/250 mb. The volumes carry up to 37
//! levels (1000→100 step 25), but exposing all of them would put ~185 more
//! rows in a picker that already lists ~119 raw `wrf_*` fields, and an "all
//! levels" toggle would have to rebuild the viewer's variable list mid-hour
//! (which resets its view state) — declined; the curated six cover the
//! standard upper-air charts.

/// Pressure levels (hPa) exposed as per-level picker entries, in
/// high-to-low pressure (low-to-high altitude) order. All members of the
/// canonical 37-level ladder both ingest paths write.
pub const ISO_PICKER_LEVELS_HPA: [u16; 6] = [925, 850, 700, 500, 300, 250];

/// One synthesizable per-level field kind, backed by the hour's isobaric
/// volume(s) named in [`IsoLevelField::source_volumes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsoLevelField {
    /// `temperature_iso` (K) → Solar per-level temperature tables where they
    /// exist (250/500/700/850), unit-aware surface table otherwise.
    Temperature,
    /// `dewpoint_iso` (K) → Solar dewpoint table (unit-aware).
    Dewpoint,
    /// `rh_iso` (%) — downloaded models substitute it for `dewpoint_iso`
    /// when too few dewpoint levels realize → Solar RH table.
    RelativeHumidity,
    /// Speed derived from `u_iso`/`v_iso` (m/s) at load time → Solar wind
    /// table. Derived-speed precedent: both import paths already write
    /// `wind_speed_10m` computed from U10/V10 at ingest.
    WindSpeed,
    /// `height_iso` (gpm) → scaled Generic ramp over the level's
    /// climatological span (see [`crate::solar_model_field_table`]).
    Height,
}

impl IsoLevelField {
    /// Every kind, in the order synthesized entries are appended to the
    /// picker (field-major; levels iterate [`ISO_PICKER_LEVELS_HPA`] within).
    pub const ALL: [IsoLevelField; 5] = [
        IsoLevelField::Temperature,
        IsoLevelField::Dewpoint,
        IsoLevelField::RelativeHumidity,
        IsoLevelField::WindSpeed,
        IsoLevelField::Height,
    ];

    /// Store-slug stem; `{stem}_{level}` is the synthesized store name.
    pub const fn slug_base(self) -> &'static str {
        match self {
            IsoLevelField::Temperature => "temperature",
            IsoLevelField::Dewpoint => "dewpoint",
            IsoLevelField::RelativeHumidity => "relative_humidity",
            IsoLevelField::WindSpeed => "wind_speed",
            IsoLevelField::Height => "height",
        }
    }

    /// Display-label stem; `{stem} {level} mb` is the picker entry.
    pub const fn label_base(self) -> &'static str {
        match self {
            IsoLevelField::Temperature => "Temperature",
            IsoLevelField::Dewpoint => "Dewpoint",
            IsoLevelField::RelativeHumidity => "RH",
            IsoLevelField::WindSpeed => "Wind speed",
            IsoLevelField::Height => "Height",
        }
    }

    /// The `pressure3d` store volume(s) a plane of this field is read from.
    /// Wind speed needs both components; every other kind reads one volume.
    pub const fn source_volumes(self) -> &'static [&'static str] {
        match self {
            IsoLevelField::Temperature => &["temperature_iso"],
            IsoLevelField::Dewpoint => &["dewpoint_iso"],
            IsoLevelField::RelativeHumidity => &["rh_iso"],
            IsoLevelField::WindSpeed => &["u_iso", "v_iso"],
            IsoLevelField::Height => &["height_iso"],
        }
    }
}

/// One synthesized per-level field: kind + exposed pressure level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IsoLevelSpec {
    pub field: IsoLevelField,
    pub level_hpa: u16,
}

impl IsoLevelSpec {
    /// Store-style slug (`temperature_850`) — what flows wherever a real
    /// store variable name would (map layer, Solar resolver, 🎨 bindings).
    pub fn slug(&self) -> String {
        format!("{}_{}", self.field.slug_base(), self.level_hpa)
    }

    /// Picker label (`Temperature 850 mb`). Contains spaces, so it can never
    /// be a valid store slug (invertibility guard, test-enforced).
    pub fn label(&self) -> String {
        format!("{} {} mb", self.field.label_base(), self.level_hpa)
    }
}

/// Parse a synthesized store slug (`temperature_850`) back to its spec.
/// Exact form only — the level must be one of [`ISO_PICKER_LEVELS_HPA`], so
/// real store names like `temperature_2m`, `temperature_850hpa` (downloaded
/// models' extracted 2D fields) or `temperature_8500` never match.
pub fn parse_iso_slug(name: &str) -> Option<IsoLevelSpec> {
    IsoLevelField::ALL.iter().find_map(|&field| {
        let level = name
            .strip_prefix(field.slug_base())?
            .strip_prefix('_')?
            .parse::<u16>()
            .ok()?;
        ISO_PICKER_LEVELS_HPA
            .contains(&level)
            .then_some(IsoLevelSpec {
                field,
                level_hpa: level,
            })
    })
}

/// Parse a synthesized picker label (`Temperature 850 mb`) back to its spec
/// (exact match, curated levels only) — the inverse of
/// [`IsoLevelSpec::label`], used to translate picker selections into loads.
pub fn parse_iso_label(label: &str) -> Option<IsoLevelSpec> {
    IsoLevelField::ALL.iter().find_map(|&field| {
        let level = label
            .strip_prefix(field.label_base())?
            .strip_prefix(' ')?
            .strip_suffix(" mb")?
            .parse::<u16>()
            .ok()?;
        ISO_PICKER_LEVELS_HPA
            .contains(&level)
            .then_some(IsoLevelSpec {
                field,
                level_hpa: level,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_store_slug_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'
    }

    fn every_spec() -> impl Iterator<Item = IsoLevelSpec> {
        IsoLevelField::ALL.into_iter().flat_map(|field| {
            ISO_PICKER_LEVELS_HPA
                .into_iter()
                .map(move |level_hpa| IsoLevelSpec { field, level_hpa })
        })
    }

    /// The curated exposure set is exactly the classic analysis surfaces,
    /// every one a member of the canonical 37-level ladder both ingest
    /// paths write (1000→100 hPa step 25).
    #[test]
    fn curated_levels_are_classic_surfaces_on_the_canonical_ladder() {
        assert_eq!(ISO_PICKER_LEVELS_HPA, [925, 850, 700, 500, 300, 250]);
        for level in ISO_PICKER_LEVELS_HPA {
            assert!(
                (100..=1000).contains(&level) && level % 25 == 0,
                "{level} is not on the canonical ladder"
            );
        }
    }

    /// Slug/label round-trip for every field × level, plus the two collision
    /// invariants the display-time rename relies on: slugs are valid store
    /// slugs, labels never are (they carry a space).
    #[test]
    fn slugs_and_labels_round_trip_and_never_collide() {
        let mut slugs = std::collections::HashSet::new();
        let mut labels = std::collections::HashSet::new();
        for spec in every_spec() {
            let slug = spec.slug();
            let label = spec.label();
            assert!(
                slug.chars().all(is_store_slug_char),
                "{slug}: synthesized slugs must look like store names"
            );
            assert!(
                label.chars().any(|c| !is_store_slug_char(c)),
                "{label}: labels must never be valid store slugs"
            );
            assert_eq!(parse_iso_slug(&slug), Some(spec), "{slug}");
            assert_eq!(parse_iso_label(&label), Some(spec), "{label}");
            assert!(slugs.insert(slug.clone()), "{slug}: duplicate slug");
            assert!(labels.insert(label.clone()), "{label}: duplicate label");
            // A slug never parses as a label and vice versa.
            assert_eq!(parse_iso_label(&slug), None, "{slug}");
            assert_eq!(parse_iso_slug(&label), None, "{label}");
        }
    }

    /// Real store names — canonical 2-m fields, the downloaded models'
    /// `hpa`-suffixed extracted levels, off-ladder digit runs, the raw
    /// volumes themselves — must never parse as synthesized fields.
    #[test]
    fn real_store_names_never_parse_as_iso_specs() {
        for name in [
            "temperature_2m",
            "temperature_850hpa",
            "temperature_8500",
            "temperature_1000", // on the volume ladder but not exposed
            "temperature_iso",
            "dewpoint_2m",
            "relative_humidity_2m",
            "wind_speed_10m",
            "wind_speed_850hpa",
            "height_iso",
            "geopotential_height_850hpa",
            "u_850",
            "height_",
            "height",
            "",
        ] {
            assert_eq!(parse_iso_slug(name), None, "{name}");
            assert_eq!(parse_iso_label(name), None, "{name}");
        }
    }

    /// Naming contract with the Solar resolver: every synthesized slug must
    /// resolve a palette through [`crate::solar_model_field_table`] in its
    /// store-native units — this is what makes the map layer's default look
    /// land without any per-field wiring.
    #[test]
    fn every_slug_resolves_a_solar_palette_in_store_units() {
        for spec in every_spec() {
            let units = match spec.field {
                IsoLevelField::Temperature | IsoLevelField::Dewpoint => "K",
                IsoLevelField::RelativeHumidity => "%",
                IsoLevelField::WindSpeed => "m/s",
                IsoLevelField::Height => "gpm",
            };
            assert!(
                crate::solar_model_field_table(&spec.slug(), units).is_some(),
                "{} ({units}) must resolve a default palette",
                spec.slug()
            );
        }
    }

    /// Level-awareness spot checks: temperature slugs land on Solar's
    /// per-level tables (the same physical value shades differently at
    /// different levels because each level stretches the gradient over its
    /// own °C span — the discriminating pair the solar tests prove), and
    /// each height level gets its own scaled ramp.
    #[test]
    fn slugs_hit_level_aware_tables() {
        let t250 = crate::solar_model_field_table("temperature_250", "K").expect("250 table");
        let t700 = crate::solar_model_field_table("temperature_700", "K").expect("700 table");
        // -30 °C sits at different positions in the 250 mb (-70..-20) and
        // 700 mb (-40..30) spans.
        assert_ne!(t250.sample(243.15), t700.sample(243.15));

        let h500 = crate::solar_model_field_table("height_500", "gpm").expect("500 heights");
        let h850 = crate::solar_model_field_table("height_850", "gpm").expect("850 heights");
        // 5 640 gpm is mid-range at 500 mb but far above the 850 mb span.
        assert_ne!(h500.sample(5640.0), h850.sample(5640.0));
    }
}
