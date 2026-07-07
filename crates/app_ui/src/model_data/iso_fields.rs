//! Loader for the synthesized per-level isobaric map fields.
//!
//! The rw-ui store worker cannot serve these: its `LoadField` reads
//! `surface2d` tile chunks (`read_full_2d`), and the `*_iso` upper air is
//! stored as `pressure3d` COLUMN chunks. So when the picker selects a
//! synthesized entry ("Temperature 850 mb" — see
//! [`color_tables::iso_levels`] for the naming contract), the Model Data
//! dock routes the load here instead: a background thread opens the hour
//! through the same [`StoreView`] the worker uses, reads the volume with
//! `read_full_3d`, slices the requested level plane
//! ([`crate::upper_air::extract_level_plane`] — the upper-air quicklook's
//! proven path), and hands back a perfectly ordinary [`FieldData`] the
//! viewer and the map layer render like any 2-D store field.
//!
//! Wind speed is derived at load time (`hypot(u_iso, v_iso)` per cell) —
//! the same derivation both import paths already bake into
//! `wind_speed_10m` at ingest, just evaluated per level on demand.
//!
//! Style/units parity with the worker's `load_field`: the 🎨 editor's
//! [`StyleOverrideSettings`] resolve against the synthesized slug and the
//! source volume's stored selector, and a resolved style's unit conversion
//! is applied to the plane — so a user palette bound to `temperature_850`
//! behaves exactly as it would on a real store variable. Without a style
//! the plane keeps its store-native units (K / m/s / gpm / %), which is
//! what the unit-aware Solar palette resolution keys on.
//!
//! Memory note: `read_full_3d` materializes the whole volume transiently
//! (`levels x ny x nx` f32) before the plane is sliced out — a plane read
//! must decode every column chunk regardless (each chunk carries all
//! levels), and this is the same cost the upper-air quicklook pays per
//! build (it reads FOUR volumes; wind speed here reads two, everything
//! else one).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

use color_tables::iso_levels::{IsoLevelField, IsoLevelSpec};
use rw_store::grid::GridFile;
use rw_ui::{FieldData, FieldKey, StoreView, StyleOverrideSettings};

/// One in-flight background plane load. `key` is the slug-named field the
/// result will carry (the dock's stale-drop compares against it).
pub(crate) struct IsoFieldLoadTask {
    pub key: FieldKey,
    pub rx: Receiver<Result<FieldData, String>>,
}

/// Spawn a background load of one synthesized per-level field. `key.var`
/// must be `spec`'s slug; `overrides` is the 🎨 editor's current
/// (normalized) settings snapshot. `repaint` wakes the UI when the result
/// lands (the loader thread has no repaint hook of its own — same pattern
/// as the import workers).
pub(crate) fn spawn_load(
    store_root: PathBuf,
    key: FieldKey,
    spec: IsoLevelSpec,
    overrides: StyleOverrideSettings,
    repaint: eframe::egui::Context,
) -> IsoFieldLoadTask {
    let (tx, rx) = channel();
    let task_key = key.clone();
    std::thread::spawn(move || {
        rw_ingest::throttle::set_current_thread_background_priority();
        let result = load_level_field(&store_root, &key, spec, &overrides);
        let _ = tx.send(result);
        repaint.request_repaint();
    });
    IsoFieldLoadTask { key: task_key, rx }
}

/// Read one level plane (or derive wind speed from two) and package it as
/// a normal 2-D [`FieldData`] — grid, orientation, style resolution and
/// unit conversion all mirroring the rw-ui worker's `load_field`.
pub(crate) fn load_level_field(
    store_root: &Path,
    key: &FieldKey,
    spec: IsoLevelSpec,
    overrides: &StyleOverrideSettings,
) -> Result<FieldData, String> {
    let view = StoreView::new(store_root);
    let reader = view
        .open_hour(&key.hour.model, &key.hour.run, key.hour.hour)
        .map_err(|err| format!("open {}: {err}", key.hour))?;
    let (nx, ny) = (reader.meta().nx, reader.meta().ny);
    let model_id = reader.meta().model.parse::<rustwx_core::ModelId>().ok();

    // Read one plane per source volume (one for scalars, u then v for wind
    // speed). Units + stored selector ride along from the first volume.
    let mut units = String::new();
    let mut selector = serde_json::Value::Null;
    let mut planes = Vec::with_capacity(spec.field.source_volumes().len());
    for (index, volume) in spec.field.source_volumes().iter().enumerate() {
        let meta = reader.variable(volume).ok_or_else(|| {
            format!(
                "{} has no {volume} volume (was this hour imported before sounding \
                 volumes were written?)",
                key.hour
            )
        })?;
        if index == 0 {
            units = meta.units.clone();
            selector = meta.selector.clone();
        }
        let levels_hpa = meta.levels_hpa.clone();
        let full = reader
            .read_full_3d(volume)
            .map_err(|err| format!("{volume}: {err}"))?;
        planes.push(crate::upper_air::extract_level_plane(
            &full,
            &levels_hpa,
            spec.level_hpa,
            nx,
            ny,
        )?);
    }
    let mut values = match spec.field {
        IsoLevelField::WindSpeed => {
            let v = planes.pop().ok_or("wind speed needs u and v planes")?;
            let mut u = planes.pop().ok_or("wind speed needs u and v planes")?;
            // hypot propagates NaN, so below-ground levels stay masked.
            for (u_value, v_value) in u.iter_mut().zip(&v) {
                *u_value = u_value.hypot(*v_value);
            }
            u
        }
        _ => planes.pop().ok_or("volume produced no plane")?,
    };

    // Grid + display orientation, same trust rules as the worker: only a
    // grid whose dims match the field, orientation derived from the lat
    // axis (never assumed), default south-to-north.
    let grid = view
        .open_grid(&key.hour.model, &key.hour.run)
        .ok()
        .map(Arc::new)
        .filter(|grid| grid.nx == nx && grid.ny == ny);
    let lat_descending = grid
        .as_deref()
        .and_then(GridFile::lat_descending)
        .unwrap_or(false);

    // 🎨 user binding / operational style for the synthesized slug, then
    // the style's unit conversion — parity with the worker's load_field so
    // the scale, hover readout and range chip all speak the same units.
    let style = overrides.style_for_store_variable(&key.var, &selector, &units, model_id);
    let units = match &style {
        Some(style) => {
            if !style.convert.is_none() {
                for value in &mut values {
                    *value = style.convert.apply(*value);
                }
            }
            style.display_units.clone()
        }
        None => units,
    };
    let range = rw_ui::colormap::finite_min_max(&values);
    Ok(FieldData {
        key: key.clone(),
        units,
        nx,
        ny,
        values,
        range,
        grid,
        lat_descending,
        style,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};
    use rw_store::{PressureVolumeInput, write_hour_from_fields};
    use rw_ui::HourKey;

    const NX: usize = 4;
    const NY: usize = 3;
    /// Fixture volume levels — deliberately handed to the writer ascending
    /// to prove the store's descending sort is what the loader sees.
    const LEVELS: [u16; 3] = [500, 700, 850];

    fn cells() -> usize {
        NX * NY
    }

    /// Deterministic plane: `base + cell index`, so every (volume, level,
    /// cell) is distinguishable.
    fn plane(base: f32) -> Vec<f32> {
        (0..cells()).map(|c| base + c as f32).collect()
    }

    fn grid() -> LatLonGrid {
        let mut lat = Vec::with_capacity(cells());
        let mut lon = Vec::with_capacity(cells());
        for y in 0..NY {
            for x in 0..NX {
                lat.push(30.0 + 0.1 * y as f32); // south-to-north
                lon.push(-100.0 + 0.1 * x as f32);
            }
        }
        LatLonGrid::new(GridShape::new(NX, NY).expect("dims"), lat, lon).expect("grid")
    }

    /// Write a one-hour `wrf`-slug store with the five iso volumes (u/v
    /// constant 3/4 m/s so speed is exactly 5) and return its root.
    fn write_fixture(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("bowecho-iso-fields-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let t2m = SelectedField2D::new(
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            grid(),
            plane(288.0),
        )
        .expect("values sized to the grid");
        let temp_planes: Vec<(u16, Vec<f32>)> = LEVELS
            .iter()
            .map(|&level| (level, plane(f32::from(level))))
            .collect();
        let dew_planes: Vec<(u16, Vec<f32>)> = LEVELS
            .iter()
            .map(|&level| (level, plane(f32::from(level) - 5.0)))
            .collect();
        let height_planes: Vec<(u16, Vec<f32>)> = LEVELS
            .iter()
            .map(|&level| (level, plane(f32::from(level) * 10.0)))
            .collect();
        let u_planes: Vec<(u16, Vec<f32>)> = LEVELS
            .iter()
            .map(|&level| (level, vec![3.0; cells()]))
            .collect();
        let v_planes: Vec<(u16, Vec<f32>)> = LEVELS
            .iter()
            .map(|&level| (level, vec![4.0; cells()]))
            .collect();
        fn volume<'a>(
            name: &'a str,
            units: &'a str,
            levels: &'a [(u16, Vec<f32>)],
        ) -> PressureVolumeInput<'a> {
            PressureVolumeInput {
                name,
                units,
                selector_template: serde_json::json!({
                    "source": "wrf",
                    "field": name,
                    "vertical": "isobaric",
                }),
                levels: levels
                    .iter()
                    .map(|(hpa, plane)| (*hpa, plane.as_slice()))
                    .collect(),
            }
        }
        let volumes = [
            volume("temperature_iso", "K", &temp_planes),
            volume("dewpoint_iso", "K", &dew_planes),
            volume("u_iso", "m/s", &u_planes),
            volume("v_iso", "m/s", &v_planes),
            volume("height_iso", "gpm", &height_planes),
        ];
        write_hour_from_fields(
            &root,
            "wrf",
            "local_wrf_19740403_090000",
            0,
            &[("temperature_2m", &t2m)],
            &volumes,
            "iso-fields-test",
            1_780_000_000,
        )
        .expect("write fixture hour");
        root
    }

    fn fixture_key(var: &str) -> FieldKey {
        FieldKey {
            hour: HourKey {
                model: "wrf".to_owned(),
                run: "local_wrf_19740403_090000".to_owned(),
                hour: 0,
            },
            var: var.to_owned(),
        }
    }

    fn spec(field: IsoLevelField, level_hpa: u16) -> IsoLevelSpec {
        IsoLevelSpec { field, level_hpa }
    }

    /// Store round-trip tolerance: the hour codec quantizes dense chunks
    /// (~1e-3 of the chunk span), so read-back is close, not bit-exact.
    const TOL: f32 = 0.05;

    #[track_caller]
    fn assert_close(actual: &[f32], expected: &[f32], what: &str) {
        assert_eq!(actual.len(), expected.len(), "{what}: length");
        for (index, (a, e)) in actual.iter().zip(expected).enumerate() {
            assert!((a - e).abs() <= TOL, "{what}[{index}]: {a} vs expected {e}");
        }
    }

    /// A scalar plane loads with store-native units, the run grid, derived
    /// orientation, and the written level's values — the full map-layer
    /// contract for a synthesized field.
    #[test]
    fn loads_the_right_plane_with_native_units_and_grid() {
        let root = write_fixture("temp");
        let key = fixture_key("temperature_850");
        let field = load_level_field(
            &root,
            &key,
            spec(IsoLevelField::Temperature, 850),
            &StyleOverrideSettings::default(),
        )
        .expect("temperature_850 loads");

        assert_eq!(field.key, key);
        assert_eq!((field.nx, field.ny), (NX, NY));
        assert_eq!(field.units, "K", "store-native Kelvin must ride along");
        assert_close(&field.values, &plane(850.0), "the 850 hPa plane");
        let (lo, hi) = field.range.expect("finite plane has a range");
        assert!((lo - 850.0).abs() <= TOL, "range lo {lo}");
        assert!(
            (hi - (850.0 + (cells() - 1) as f32)).abs() <= TOL,
            "range hi {hi}"
        );
        assert!(field.grid.is_some(), "run grid rides along");
        assert!(!field.lat_descending, "south-to-north fixture grid");
        assert!(field.style.is_none(), "no bindings -> native units");

        // Heights keep their gpm units and their own volume's values.
        let heights = load_level_field(
            &root,
            &fixture_key("height_500"),
            spec(IsoLevelField::Height, 500),
            &StyleOverrideSettings::default(),
        )
        .expect("height_500 loads");
        assert_eq!(heights.units, "gpm");
        assert_close(&heights.values, &plane(5000.0), "the 500 hPa heights");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Wind speed is derived per cell from the u/v volumes (3/4 -> 5 m/s
    /// everywhere), in the components' units.
    #[test]
    fn wind_speed_is_hypot_of_u_and_v() {
        let root = write_fixture("wind");
        let field = load_level_field(
            &root,
            &fixture_key("wind_speed_700"),
            spec(IsoLevelField::WindSpeed, 700),
            &StyleOverrideSettings::default(),
        )
        .expect("wind_speed_700 loads");
        assert_eq!(field.units, "m/s");
        assert_close(&field.values, &vec![5.0; cells()], "derived speed");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Missing level / missing volume both fail with messages naming the
    /// problem (surfaced in the viewer's error state).
    #[test]
    fn missing_level_and_missing_volume_are_clear_errors() {
        let root = write_fixture("missing");
        let err = load_level_field(
            &root,
            &fixture_key("temperature_925"),
            spec(IsoLevelField::Temperature, 925),
            &StyleOverrideSettings::default(),
        )
        .expect_err("925 is not a fixture level");
        assert!(err.contains("925"), "error names the level: {err}");

        let err = load_level_field(
            &root,
            &fixture_key("relative_humidity_850"),
            spec(IsoLevelField::RelativeHumidity, 850),
            &StyleOverrideSettings::default(),
        )
        .expect_err("fixture has no rh_iso");
        assert!(err.contains("rh_iso"), "error names the volume: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A 🎨 user table bound to the synthesized slug resolves into the
    /// loaded field's style — parity with how the store worker treats real
    /// variables (audit #11 lineage: the user's palette must win on the
    /// map layer, which keys off `field.style`).
    #[test]
    fn user_color_binding_resolves_for_synthesized_slugs() {
        let root = write_fixture("style");
        let mut settings = StyleOverrideSettings::default();
        settings.upsert_table(rw_ui::UserColorTable::simple("My 850", "My 850", "K"));
        settings.bind_product(&rw_ui::normalize_product_key("temperature_850"), "My 850");
        let settings = settings.normalized();

        let field = load_level_field(
            &root,
            &fixture_key("temperature_850"),
            spec(IsoLevelField::Temperature, 850),
            &settings,
        )
        .expect("styled load");
        assert!(field.style.is_some(), "bound user table becomes the style");

        // An unbound slug keeps the native-units, no-style path.
        let plain = load_level_field(
            &root,
            &fixture_key("dewpoint_850"),
            spec(IsoLevelField::Dewpoint, 850),
            &settings,
        )
        .expect("plain load");
        assert!(plain.style.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
