//! Unit-system preference (Settings ▸ Display ▸ Units) and the shared
//! formatters the analyst-facing readouts go through: lowest-beam menu
//! rows, ctrl+click statuses, the cursor inspector, station plots, and
//! the range-circle annotation label.
//!
//! Pure module — no egui, no app state — so every conversion is unit
//! tested with exact expected strings. Conversion factors: 1 m =
//! 3.28084 ft, 1 km = 0.621371 mi (NIST SP 811).
//!
//! Deliberately NOT routed through here: warning-desk Vrot readouts
//! (kt · nm · kft — the operational convention worldwide), skew-T °C,
//! SPC report magnitudes (mph / inches are the source-data semantics),
//! and the metric-neutral "km range" status chip.

use settings::AppSettings;

/// The persisted `AppSettings::units` slug, parsed. Imperial is the
/// default (US-born app); unknown slugs read as imperial too, so a
/// hand-edited config can never break startup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Units {
    #[default]
    Imperial,
    Metric,
}

impl Units {
    pub(crate) fn from_slug(slug: &str) -> Self {
        if slug.trim().eq_ignore_ascii_case("metric") {
            Self::Metric
        } else {
            Self::Imperial
        }
    }

    pub(crate) fn from_settings(settings: &AppSettings) -> Self {
        Self::from_slug(&settings.units)
    }

    /// The slug `AppSettings::units` persists.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            Self::Imperial => "imperial",
            Self::Metric => "metric",
        }
    }

    /// Settings-combo display name.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Imperial => "Imperial",
            Self::Metric => "Metric",
        }
    }
}

const FT_PER_M: f32 = 3.280_84;
const MI_PER_KM: f32 = 0.621_371;

/// Beam height above the radar. Imperial keeps kft (the analyst
/// convention); metric reads best in m under 1 km and km above.
pub(crate) fn format_beam_height(meters: f32, units: Units) -> String {
    match units {
        Units::Imperial => format!("{:.1} kft", meters * FT_PER_M / 1000.0),
        Units::Metric => {
            if meters < 1000.0 {
                format!("{meters:.0} m")
            } else {
                format!("{:.1} km", meters / 1000.0)
            }
        }
    }
}

/// Ground distance (site rankings, "closest radar" statuses).
pub(crate) fn format_distance_km(km: f32, units: Units) -> String {
    match units {
        Units::Imperial => format!("{:.0} mi", km * MI_PER_KM),
        Units::Metric => format!("{km:.0} km"),
    }
}

/// A single temperature with its degree suffix.
pub(crate) fn format_temperature_c(c: f32, units: Units) -> String {
    match units {
        Units::Imperial => format!("{:.0}°F", c * 9.0 / 5.0 + 32.0),
        Units::Metric => format!("{c:.0}°C"),
    }
}

/// Inspector T/Td pair sharing one degree suffix — the compact GR2A
/// form ("57/52°F", "14/11°C").
pub(crate) fn format_temp_pair_c(t_c: f32, td_c: f32, units: Units) -> String {
    match units {
        Units::Imperial => format!(
            "{:.0}/{:.0}°F",
            t_c * 9.0 / 5.0 + 32.0,
            td_c * 9.0 / 5.0 + 32.0
        ),
        Units::Metric => format!("{t_c:.0}/{td_c:.0}°C"),
    }
}

/// Bare station-plot temperature value — station models omit the unit
/// suffix (the plot legend carries it).
pub(crate) fn format_station_temp_c(c: f32, units: Units) -> String {
    match units {
        Units::Imperial => format!("{:.0}", c * 9.0 / 5.0 + 32.0),
        Units::Metric => format!("{c:.0}"),
    }
}

/// Range-circle annotation label: imperial leads with miles but keeps
/// the km in parentheses (radar range rings are spoken in km); metric
/// reads km alone.
pub(crate) fn format_range_ring_km(km: f32, units: Units) -> String {
    match units {
        Units::Imperial => format!("{:.1} mi ({:.1} km)", km * MI_PER_KM, km),
        Units::Metric => format!("{km:.1} km"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_round_trips_and_unknown_reads_imperial() {
        assert_eq!(Units::from_slug("imperial"), Units::Imperial);
        assert_eq!(Units::from_slug("metric"), Units::Metric);
        assert_eq!(Units::from_slug(" METRIC "), Units::Metric);
        for bogus in ["", "si", "freedom", "Metric units", "0"] {
            assert_eq!(Units::from_slug(bogus), Units::Imperial, "{bogus:?}");
        }
        for units in [Units::Imperial, Units::Metric] {
            assert_eq!(Units::from_slug(units.slug()), units);
        }
        // A default config (serde default "imperial") parses to Imperial.
        assert_eq!(
            Units::from_settings(&AppSettings::default()),
            Units::Imperial
        );
    }

    #[test]
    fn beam_height_imperial_is_kft() {
        // 900 m × 3.28084 / 1000 = 2.9527 kft → "3.0 kft".
        assert_eq!(format_beam_height(900.0, Units::Imperial), "3.0 kft");
        assert_eq!(format_beam_height(0.0, Units::Imperial), "0.0 kft");
        // 3657.6 m = 12.0 kft exactly.
        assert_eq!(format_beam_height(3657.6, Units::Imperial), "12.0 kft");
    }

    #[test]
    fn beam_height_metric_switches_m_to_km_at_1000() {
        assert_eq!(format_beam_height(900.0, Units::Metric), "900 m");
        assert_eq!(format_beam_height(74.4, Units::Metric), "74 m");
        assert_eq!(format_beam_height(1000.0, Units::Metric), "1.0 km");
        assert_eq!(format_beam_height(3650.0, Units::Metric), "3.7 km");
    }

    #[test]
    fn distance_converts_km_to_mi_only_for_imperial() {
        // 100 km × 0.621371 = 62.1371 → "62 mi".
        assert_eq!(format_distance_km(100.0, Units::Imperial), "62 mi");
        assert_eq!(format_distance_km(460.0, Units::Imperial), "286 mi");
        assert_eq!(format_distance_km(100.0, Units::Metric), "100 km");
        assert_eq!(format_distance_km(0.4, Units::Metric), "0 km");
    }

    #[test]
    fn temperature_uses_exact_f_conversion() {
        // 100 °C = 212 °F; 0 °C = 32 °F; -40 is the crossover.
        assert_eq!(format_temperature_c(100.0, Units::Imperial), "212°F");
        assert_eq!(format_temperature_c(0.0, Units::Imperial), "32°F");
        assert_eq!(format_temperature_c(-40.0, Units::Imperial), "-40°F");
        assert_eq!(format_temperature_c(21.5, Units::Metric), "22°C");
    }

    #[test]
    fn temp_pair_shares_one_suffix() {
        // 14 °C → 57.2 °F, 11 °C → 51.8 °F.
        assert_eq!(format_temp_pair_c(14.0, 11.0, Units::Imperial), "57/52°F");
        assert_eq!(format_temp_pair_c(14.0, 11.0, Units::Metric), "14/11°C");
    }

    #[test]
    fn station_plot_values_are_bare_numbers() {
        assert_eq!(format_station_temp_c(14.0, Units::Imperial), "57");
        assert_eq!(format_station_temp_c(14.0, Units::Metric), "14");
        assert_eq!(format_station_temp_c(-5.0, Units::Metric), "-5");
    }

    #[test]
    fn range_ring_label_leads_with_the_chosen_system() {
        // 10 km × 0.621371 = 6.21371 → "6.2 mi (10.0 km)".
        assert_eq!(
            format_range_ring_km(10.0, Units::Imperial),
            "6.2 mi (10.0 km)"
        );
        assert_eq!(format_range_ring_km(10.0, Units::Metric), "10.0 km");
        assert_eq!(
            format_range_ring_km(160.9344, Units::Imperial),
            "100.0 mi (160.9 km)"
        );
    }
}
