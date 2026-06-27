use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rustwx_core::{GridProjection, ModelId};
use rustwx_products::viewer::operational_style_for_store_variable;
use rw_store::grid::GridFile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GridCompositeSource {
    MrmsLowestAltitudeReflectivity,
    MrmsCompositeReflectivity,
    EumetnetOperaDbzh,
}

impl GridCompositeSource {
    pub(crate) const ALL: [Self; 3] = [
        Self::MrmsLowestAltitudeReflectivity,
        Self::MrmsCompositeReflectivity,
        Self::EumetnetOperaDbzh,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity => "MRMS lowest-altitude reflectivity",
            Self::MrmsCompositeReflectivity => "MRMS composite reflectivity",
            Self::EumetnetOperaDbzh => "EUMETNET OPERA DBZH composite",
        }
    }

    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity => "MRMS low REF",
            Self::MrmsCompositeReflectivity => "MRMS CREF",
            Self::EumetnetOperaDbzh => "OPERA DBZH",
        }
    }

    pub(crate) fn model_slug(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity | Self::MrmsCompositeReflectivity => "mrms",
            Self::EumetnetOperaDbzh => "eumetnet-opera",
        }
    }

    pub(crate) fn variable_slug(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity => "mrms_reflectivity_lowest_altitude",
            Self::MrmsCompositeReflectivity => "mrms_composite_reflectivity",
            Self::EumetnetOperaDbzh => "eumetnet_opera_dbzh_composite",
        }
    }

    pub(crate) fn from_variable_slug(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|source| source.variable_slug() == value)
    }

    pub(crate) fn map_center(self) -> (f32, f32, f32) {
        match self {
            Self::MrmsLowestAltitudeReflectivity | Self::MrmsCompositeReflectivity => {
                (39.0, -97.0, 26.0)
            }
            Self::EumetnetOperaDbzh => (50.5, 10.0, 22.0),
        }
    }
}

pub(crate) type GridCompositeResult = Result<rw_ui::FieldData, String>;

pub(crate) fn load_latest_field(source: GridCompositeSource) -> GridCompositeResult {
    let selected = match source {
        GridCompositeSource::MrmsLowestAltitudeReflectivity => {
            rustwx_io::extract_mrms_latest_reflectivity_at_lowest_altitude()
                .map_err(|err| format!("MRMS lowest-altitude reflectivity failed: {err}"))?
        }
        GridCompositeSource::MrmsCompositeReflectivity => {
            rustwx_io::extract_mrms_latest_composite_reflectivity()
                .map_err(|err| format!("MRMS composite reflectivity failed: {err}"))?
        }
        GridCompositeSource::EumetnetOperaDbzh => {
            let end = Utc::now();
            let start = end - Duration::minutes(35);
            let range = format!("{}/{}", format_opera_time(start), format_opera_time(end));
            rustwx_io::fetch_eumetnet_opera_latest_dbzh_for_range(&range)
                .map_err(|err| format!("EUMETNET OPERA DBZH failed: {err}"))?
        }
    };
    selected_field_to_field_data(source, selected)
}

fn selected_field_to_field_data(
    source: GridCompositeSource,
    selected: rustwx_core::SelectedField2D,
) -> GridCompositeResult {
    let nx = selected.grid.shape.nx;
    let ny = selected.grid.shape.ny;
    let grid = Arc::new(GridFile {
        nx,
        ny,
        lat: selected.grid.lat_deg,
        lon: selected.grid.lon_deg,
        projection: selected.projection.clone(),
        hash: grid_identity(source, &selected.projection, nx, ny),
    });
    let lat_descending = grid.lat_descending().unwrap_or(false);
    let selector_json = serde_json::to_value(selected.selector)
        .map_err(|err| format!("grid composite selector encode failed: {err}"))?;
    let mut values = selected.values;
    let style = operational_style_for_store_variable(
        source.variable_slug(),
        &selector_json,
        &selected.units,
        ModelId::Hrrr,
    );
    let units = match &style {
        Some(style) => {
            if !style.convert.is_none() {
                for value in &mut values {
                    *value = style.convert.apply(*value);
                }
            }
            style.display_units.clone()
        }
        None => selected.units,
    };
    sanitize_grid_composite_values(source, &mut values);
    let range = rw_ui::colormap::finite_min_max(&values);
    Ok(rw_ui::FieldData {
        key: rw_ui::FieldKey {
            hour: rw_ui::HourKey {
                model: source.model_slug().to_owned(),
                run: "latest".to_owned(),
                hour: 0,
            },
            var: source.variable_slug().to_owned(),
        },
        units,
        nx,
        ny,
        values,
        range,
        grid: Some(grid),
        lat_descending,
        style,
    })
}

fn sanitize_grid_composite_values(source: GridCompositeSource, values: &mut [f32]) {
    if !matches!(
        source,
        GridCompositeSource::MrmsLowestAltitudeReflectivity
            | GridCompositeSource::MrmsCompositeReflectivity
            | GridCompositeSource::EumetnetOperaDbzh
    ) {
        return;
    }
    for value in values {
        if !value.is_finite() || *value < -25.0 || *value > 95.0 {
            *value = f32::NAN;
        }
    }
}

fn grid_identity(
    source: GridCompositeSource,
    projection: &Option<GridProjection>,
    nx: usize,
    ny: usize,
) -> String {
    format!(
        "{}:{nx}x{ny}:{:?}",
        source.variable_slug(),
        projection.as_ref()
    )
}

fn format_opera_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%MZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};

    #[test]
    fn mrms_field_conversion_builds_stable_layer_key() {
        let grid = LatLonGrid::new(
            GridShape { nx: 2, ny: 2 },
            vec![40.0, 40.0, 39.0, 39.0],
            vec![-101.0, -100.0, -101.0, -100.0],
        )
        .unwrap();
        let selected = SelectedField2D::new(
            FieldSelector::altitude_msl(CanonicalField::RadarReflectivity, 500),
            "dBZ",
            grid,
            vec![10.0, 20.0, -32.0, 120.0],
        )
        .unwrap();

        let field = selected_field_to_field_data(
            GridCompositeSource::MrmsLowestAltitudeReflectivity,
            selected,
        )
        .unwrap();

        assert_eq!(field.key.hour.model, "mrms");
        assert_eq!(field.key.var, "mrms_reflectivity_lowest_altitude");
        assert_eq!(field.nx, 2);
        assert_eq!(field.ny, 2);
        assert_eq!(field.range, Some((10.0, 20.0)));
        assert!(field.values[2].is_nan());
        assert!(field.values[3].is_nan());
        assert!(field.grid.is_some());
    }
}
