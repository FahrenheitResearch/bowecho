//! Runtime basemap bridge for rusty-weather's raw model-field viewer.
//!
//! BowEcho already compiles the map linework in [`crate::basemap_data`] into
//! the executable.  `rw-ui` currently discovers equivalent linework as ESRI
//! shapefiles next to the rusty-weather workspace, which is useful in that
//! repository but leaves a standalone BowEcho executable with empty
//! Broad/Regional/Counties layers.  Materialize a small, versioned cache from
//! BowEcho's embedded data before `rw-ui` is constructed and point
//! `rustwx-render` at it through its supported `RUSTWX_BASEMAP_DIR` override.

use std::path::Path;

use crate::basemap_data::{self, BasemapLine};

const RUSTWX_BASEMAP_DIR_ENV: &str = "RUSTWX_BASEMAP_DIR";
const CACHE_SCHEMA: &str = "bowecho-model-gis-v1";
const READY_MARKER: &str = "ready-v1";

const LAND_NAME: &str = "ne_10m_land";
const COAST_NAME: &str = "ne_10m_coastline";
const ADMIN0_NAME: &str = "ne_10m_admin_0_boundary_lines_land";
const ADMIN1_NAME: &str = "ne_10m_admin_1_states_provinces_lines";
const COUNTIES_NAME: &str = "cb_2023_us_county_5m";

/// Prepare the standalone model-viewer GIS cache unless the user explicitly
/// supplied rustwx-render's normal basemap override.
pub(crate) fn prepare_runtime_basemap() -> Result<(), String> {
    if std::env::var_os(RUSTWX_BASEMAP_DIR_ENV).is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }

    let root = settings::bowecho_config_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("bowecho"))
        .join(CACHE_SCHEMA);
    ensure_runtime_basemap(&root)?;

    // SAFETY: `main` calls this once, before eframe or any BowEcho worker is
    // started. No other thread can concurrently read or mutate the process
    // environment at this point.
    unsafe { std::env::set_var(RUSTWX_BASEMAP_DIR_ENV, &root) };
    Ok(())
}

fn ensure_runtime_basemap(root: &Path) -> Result<(), String> {
    if runtime_basemap_ready(root) {
        return Ok(());
    }

    let natural_earth = root.join("natural_earth_10m");
    let counties = root.join("us_counties_5m");
    std::fs::create_dir_all(&natural_earth)
        .map_err(|error| format!("create model GIS cache: {error}"))?;
    std::fs::create_dir_all(&counties)
        .map_err(|error| format!("create model GIS county cache: {error}"))?;

    // The generated world-country rings contain both coastlines and land
    // borders, so they belong in the admin-0 layer. The model viewer treats
    // the role as ordinary political linework and does not need polygon
    // attributes or DBF records.
    write_polygon_shapefile(
        &natural_earth.join(format!("{LAND_NAME}.shp")),
        basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
    )?;
    write_line_shapefile(
        &natural_earth.join(format!("{COAST_NAME}.shp")),
        &[basemap_data::BASEMAP_WORLD_COUNTRY_LINES],
    )?;
    write_line_shapefile(
        &natural_earth.join(format!("{ADMIN0_NAME}.shp")),
        &[basemap_data::BASEMAP_WORLD_COUNTRY_LINES],
    )?;
    write_line_shapefile(
        &natural_earth.join(format!("{ADMIN1_NAME}.shp")),
        &[
            basemap_data::BASEMAP_US_STATE_LINES,
            basemap_data::BASEMAP_CANADA_ADMIN_LINES,
            basemap_data::BASEMAP_MEXICO_ADMIN_LINES,
            basemap_data::BASEMAP_JAPAN_ADMIN_LINES,
        ],
    )?;
    write_line_shapefile(
        &counties.join(format!("{COUNTIES_NAME}.shp")),
        &[basemap_data::BASEMAP_US_COUNTY_LINES],
    )?;

    std::fs::write(root.join(READY_MARKER), CACHE_SCHEMA)
        .map_err(|error| format!("finish model GIS cache: {error}"))?;
    Ok(())
}

fn runtime_basemap_ready(root: &Path) -> bool {
    root.join(READY_MARKER).is_file()
        && shapefile_pair_ready(
            &root
                .join("natural_earth_10m")
                .join(format!("{COAST_NAME}.shp")),
        )
        && shapefile_pair_ready(
            &root
                .join("natural_earth_10m")
                .join(format!("{LAND_NAME}.shp")),
        )
        && shapefile_pair_ready(
            &root
                .join("natural_earth_10m")
                .join(format!("{ADMIN0_NAME}.shp")),
        )
        && shapefile_pair_ready(
            &root
                .join("natural_earth_10m")
                .join(format!("{ADMIN1_NAME}.shp")),
        )
        && shapefile_pair_ready(
            &root
                .join("us_counties_5m")
                .join(format!("{COUNTIES_NAME}.shp")),
        )
}

fn shapefile_pair_ready(shp: &Path) -> bool {
    [shp.to_path_buf(), shp.with_extension("shx")]
        .iter()
        .all(|path| path.metadata().is_ok_and(|metadata| metadata.len() > 100))
}

fn write_polygon_shapefile(path: &Path, lines: &[BasemapLine]) -> Result<(), String> {
    let mut writer = shapefile::ShapeWriter::from_path(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let mut written = 0usize;
    for line in lines {
        let points = line
            .points
            .iter()
            .filter(|(lon, lat)| lon.is_finite() && lat.is_finite())
            .map(|(lon, lat)| shapefile::Point::new(f64::from(*lon), f64::from(*lat)))
            .collect::<Vec<_>>();
        if points.len() < 3 {
            continue;
        }
        let polygon = shapefile::Polygon::new(shapefile::PolygonRing::Outer(points));
        writer
            .write_shape(&polygon)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        written += 1;
    }
    if written == 0 {
        return Err(format!(
            "no usable model GIS polygons for {}",
            path.display()
        ));
    }
    writer
        .finalize()
        .map_err(|error| format!("finalize {}: {error}", path.display()))
}

fn write_line_shapefile(path: &Path, groups: &[&[BasemapLine]]) -> Result<(), String> {
    let mut writer = shapefile::ShapeWriter::from_path(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let mut written = 0usize;
    for line in groups.iter().flat_map(|group| group.iter()) {
        let points = line
            .points
            .iter()
            .filter(|(lon, lat)| lon.is_finite() && lat.is_finite())
            .map(|(lon, lat)| shapefile::Point::new(f64::from(*lon), f64::from(*lat)))
            .collect::<Vec<_>>();
        if points.len() < 2 {
            continue;
        }
        writer
            .write_shape(&shapefile::Polyline::new(points))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        written += 1;
    }
    if written == 0 {
        return Err(format!("no usable model GIS lines for {}", path.display()));
    }
    writer
        .finalize()
        .map_err(|error| format!("finalize {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_LINES: &[BasemapLine] = &[
        BasemapLine {
            bbox: [-100.0, 30.0, -98.0, 32.0],
            points: &[(-100.0, 30.0), (-99.0, 31.0), (-98.0, 32.0)],
        },
        BasemapLine {
            bbox: [-90.0, 35.0, -88.5, 36.0],
            points: &[(-90.0, 35.0), (-89.0, 36.0), (-88.5, 35.0)],
        },
    ];

    #[test]
    fn embedded_model_gis_lines_round_trip_through_shapefile() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        let path = temp.path().join("linework.shp");
        write_line_shapefile(&path, &[TEST_LINES]).expect("write GIS linework");

        let loaded = rustwx_render::load_lines_from_shapefile(&path).expect("read GIS linework");
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded[0],
            vec![(-100.0, 30.0), (-99.0, 31.0), (-98.0, 32.0)]
        );
        assert_eq!(loaded[1], vec![(-90.0, 35.0), (-89.0, 36.0), (-88.5, 35.0)]);
        assert!(shapefile_pair_ready(&path));
    }

    #[test]
    fn embedded_model_gis_polygons_round_trip_through_shapefile() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        let path = temp.path().join("land.shp");
        write_polygon_shapefile(&path, TEST_LINES).expect("write GIS polygons");

        let loaded = rustwx_render::load_polygons_from_shapefile(&path).expect("read GIS polygons");
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|polygon| polygon[0].len() >= 4));
        assert!(shapefile_pair_ready(&path));
    }

    #[test]
    fn incomplete_model_gis_cache_is_never_treated_as_ready() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        std::fs::write(temp.path().join(READY_MARKER), CACHE_SCHEMA).expect("write marker");
        assert!(!runtime_basemap_ready(temp.path()));
    }
}
