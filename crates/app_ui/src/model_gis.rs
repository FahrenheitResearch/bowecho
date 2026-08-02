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
const CACHE_SCHEMA: &str = "bowecho-model-gis-v2";
const READY_MARKER: &str = "ready-v2";

const NATURAL_EARTH_RESOLUTIONS: [&str; 2] = ["10m", "110m"];
const OCEAN_LAYER: &str = "ocean";
const LAND_LAYER: &str = "land";
const LAKES_LAYER: &str = "lakes";
const COUNTRIES_LAYER: &str = "admin_0_countries";
const COAST_LAYER: &str = "coastline";
const ADMIN0_LAYER: &str = "admin_0_boundary_lines_land";
const ADMIN1_LAYER: &str = "admin_1_states_provinces_lines";
const COUNTIES_NAME: &str = "cb_2023_us_county_5m";

// rustwx-render paints ocean before land. A full-world ocean backing polygon
// keeps that layer deterministic without pretending BowEcho's country rings
// contain Natural Earth's separate ocean geometry.
const WORLD_OCEAN_BACKGROUND: &[BasemapLine] = &[BasemapLine {
    bbox: [-180.0, -90.0, 180.0, 90.0],
    points: &[
        (-180.0, -90.0),
        (180.0, -90.0),
        (180.0, 90.0),
        (-180.0, 90.0),
    ],
}];

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

    materialize_runtime_basemap(
        root,
        basemap_data::BASEMAP_WORLD_COUNTRY_LINES,
        &[
            basemap_data::BASEMAP_US_STATE_LINES,
            basemap_data::BASEMAP_CANADA_ADMIN_LINES,
            basemap_data::BASEMAP_MEXICO_ADMIN_LINES,
            basemap_data::BASEMAP_JAPAN_ADMIN_LINES,
        ],
        basemap_data::BASEMAP_US_COUNTY_LINES,
    )
}

fn materialize_runtime_basemap(
    root: &Path,
    world_country_lines: &[BasemapLine],
    admin1_groups: &[&[BasemapLine]],
    county_lines: &[BasemapLine],
) -> Result<(), String> {
    for resolution in NATURAL_EARTH_RESOLUTIONS {
        let natural_earth = natural_earth_root(root, resolution);
        std::fs::create_dir_all(&natural_earth)
            .map_err(|error| format!("create model GIS {resolution} cache: {error}"))?;

        // BowEcho embeds one world-country polygon/line source. Materialize
        // every filename used by rustwx-render so its resolution preference
        // cannot escape this override and pick up a developer's Cargo checkout.
        write_polygon_shapefile(
            &natural_earth_layer_path(root, resolution, OCEAN_LAYER),
            WORLD_OCEAN_BACKGROUND,
        )?;
        write_polygon_shapefile(
            &natural_earth_layer_path(root, resolution, LAND_LAYER),
            world_country_lines,
        )?;
        write_polygon_shapefile(
            &natural_earth_layer_path(root, resolution, COUNTRIES_LAYER),
            world_country_lines,
        )?;
        write_empty_shapefile(&natural_earth_layer_path(root, resolution, LAKES_LAYER))?;
        write_line_shapefile(
            &natural_earth_layer_path(root, resolution, COAST_LAYER),
            &[world_country_lines],
        )?;
        write_line_shapefile(
            &natural_earth_layer_path(root, resolution, ADMIN0_LAYER),
            &[world_country_lines],
        )?;
        write_line_shapefile(
            &natural_earth_layer_path(root, resolution, ADMIN1_LAYER),
            admin1_groups,
        )?;
    }

    let counties = root.join("us_counties_5m");
    std::fs::create_dir_all(&counties)
        .map_err(|error| format!("create model GIS county cache: {error}"))?;
    write_line_shapefile(
        &counties.join(format!("{COUNTIES_NAME}.shp")),
        &[county_lines],
    )?;

    std::fs::write(root.join(READY_MARKER), CACHE_SCHEMA)
        .map_err(|error| format!("finish model GIS cache: {error}"))?;
    Ok(())
}

fn runtime_basemap_ready(root: &Path) -> bool {
    std::fs::read_to_string(root.join(READY_MARKER)).is_ok_and(|value| value == CACHE_SCHEMA)
        && NATURAL_EARTH_RESOLUTIONS
            .into_iter()
            .all(|resolution| natural_earth_resolution_ready(root, resolution))
        && shapefile_pair_ready(
            &root
                .join("us_counties_5m")
                .join(format!("{COUNTIES_NAME}.shp")),
        )
}

fn natural_earth_root(root: &Path, resolution: &str) -> std::path::PathBuf {
    root.join(format!("natural_earth_{resolution}"))
}

fn natural_earth_layer_path(root: &Path, resolution: &str, layer: &str) -> std::path::PathBuf {
    natural_earth_root(root, resolution).join(format!("ne_{resolution}_{layer}.shp"))
}

fn natural_earth_resolution_ready(root: &Path, resolution: &str) -> bool {
    [
        OCEAN_LAYER,
        LAND_LAYER,
        COUNTRIES_LAYER,
        COAST_LAYER,
        ADMIN0_LAYER,
        ADMIN1_LAYER,
    ]
    .into_iter()
    .all(|layer| shapefile_pair_ready(&natural_earth_layer_path(root, resolution, layer)))
        // BowEcho currently has no embedded lake geometry. Keep an explicit,
        // valid empty layer in the override rather than allowing a machine's
        // unrelated checkout assets to decide whether lakes appear.
        && shapefile_pair_has_header(&natural_earth_layer_path(root, resolution, LAKES_LAYER))
}

fn shapefile_pair_ready(shp: &Path) -> bool {
    [shp.to_path_buf(), shp.with_extension("shx")]
        .iter()
        .all(|path| path.metadata().is_ok_and(|metadata| metadata.len() > 100))
}

fn shapefile_pair_has_header(shp: &Path) -> bool {
    [shp.to_path_buf(), shp.with_extension("shx")]
        .iter()
        .all(|path| path.metadata().is_ok_and(|metadata| metadata.len() >= 100))
}

fn write_empty_shapefile(path: &Path) -> Result<(), String> {
    let mut writer = shapefile::ShapeWriter::from_path(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    writer
        .finalize()
        .map_err(|error| format!("finalize {}: {error}", path.display()))
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

    const TEST_ADMIN1_LINES: &[BasemapLine] = &[BasemapLine {
        bbox: [-104.0, 38.0, -102.0, 40.0],
        points: &[(-104.0, 38.0), (-103.0, 39.0), (-102.0, 40.0)],
    }];

    #[test]
    fn embedded_model_gis_lines_round_trip_through_shapefile() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        let path = temp.path().join("linework.shp");
        write_line_shapefile(&path, &[TEST_LINES]).expect("write GIS linework");

        let loaded =
            shapefile::read_shapes_as::<_, shapefile::Polyline>(&path).expect("read GIS linework");
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded[0]
                .part(0)
                .expect("single line part")
                .iter()
                .map(|point| (point.x, point.y))
                .collect::<Vec<_>>(),
            vec![(-100.0, 30.0), (-99.0, 31.0), (-98.0, 32.0)]
        );
        assert_eq!(
            loaded[1]
                .part(0)
                .expect("single line part")
                .iter()
                .map(|point| (point.x, point.y))
                .collect::<Vec<_>>(),
            vec![(-90.0, 35.0), (-89.0, 36.0), (-88.5, 35.0)]
        );
        assert!(shapefile_pair_ready(&path));
    }

    #[test]
    fn embedded_model_gis_polygons_round_trip_through_shapefile() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        let path = temp.path().join("land.shp");
        write_polygon_shapefile(&path, TEST_LINES).expect("write GIS polygons");

        let loaded =
            shapefile::read_shapes_as::<_, shapefile::Polygon>(&path).expect("read GIS polygons");
        assert_eq!(loaded.len(), 2);
        assert!(
            loaded
                .iter()
                .all(|polygon| polygon.ring(0).is_some_and(|ring| ring.points().len() >= 4))
        );
        assert!(shapefile_pair_ready(&path));
    }

    #[test]
    fn runtime_cache_materializes_both_natural_earth_resolutions() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        materialize_runtime_basemap(
            temp.path(),
            TEST_LINES,
            &[TEST_ADMIN1_LINES],
            TEST_ADMIN1_LINES,
        )
        .expect("materialize model GIS cache");

        assert!(runtime_basemap_ready(temp.path()));
        for resolution in NATURAL_EARTH_RESOLUTIONS {
            assert!(natural_earth_resolution_ready(temp.path(), resolution));

            for layer in [
                OCEAN_LAYER,
                LAND_LAYER,
                COUNTRIES_LAYER,
                COAST_LAYER,
                ADMIN0_LAYER,
                ADMIN1_LAYER,
            ] {
                assert!(shapefile_pair_ready(&natural_earth_layer_path(
                    temp.path(),
                    resolution,
                    layer,
                )));
            }
            assert!(shapefile_pair_has_header(&natural_earth_layer_path(
                temp.path(),
                resolution,
                LAKES_LAYER,
            )));

            let admin0 = shapefile::read_shapes_as::<_, shapefile::Polyline>(
                natural_earth_layer_path(temp.path(), resolution, ADMIN0_LAYER),
            )
            .expect("read admin-0 linework");
            let admin1 = shapefile::read_shapes_as::<_, shapefile::Polyline>(
                natural_earth_layer_path(temp.path(), resolution, ADMIN1_LAYER),
            )
            .expect("read admin-1 linework");
            assert_eq!(admin0.len(), TEST_LINES.len());
            assert_eq!(admin1.len(), TEST_ADMIN1_LINES.len());
            assert_eq!(
                admin1[0]
                    .part(0)
                    .expect("single admin-1 part")
                    .iter()
                    .map(|point| (point.x, point.y))
                    .collect::<Vec<_>>(),
                vec![(-104.0, 38.0), (-103.0, 39.0), (-102.0, 40.0)]
            );
        }
    }

    #[test]
    fn legacy_ready_marker_does_not_validate_v2_cache() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        materialize_runtime_basemap(
            temp.path(),
            TEST_LINES,
            &[TEST_ADMIN1_LINES],
            TEST_ADMIN1_LINES,
        )
        .expect("materialize model GIS cache");
        assert!(runtime_basemap_ready(temp.path()));

        std::fs::remove_file(temp.path().join(READY_MARKER)).expect("remove v2 marker");
        std::fs::write(temp.path().join("ready-v1"), "bowecho-model-gis-v1")
            .expect("write legacy marker");

        assert!(!runtime_basemap_ready(temp.path()));
    }

    #[test]
    fn incomplete_model_gis_cache_is_never_treated_as_ready() {
        let temp = tempfile::tempdir().expect("temporary model GIS root");
        std::fs::write(temp.path().join(READY_MARKER), CACHE_SCHEMA).expect("write marker");
        assert!(!runtime_basemap_ready(temp.path()));
    }
}
