//! Bridge between a completed Formula Lab grid and the sounding table.
//!
//! Formula Lab deliberately produces a two-dimensional field, not a scalar
//! sounding diagnostic.  A formula can therefore appear in the sounding
//! table only after it has been evaluated for the exact store timestep that
//! owns the displayed model sounding.  This module samples that completed
//! scientific field at the sounding's fractional grid coordinate; it never
//! re-runs a formula on the UI thread or pretends a raw-WRF result belongs to
//! an rw-store sounding.

use rw_ui::{FieldData, SoundingData};
use sha2::{Digest, Sha256};

/// Content-aware identity retained beside the last Formula Lab field.
///
/// Data-source identity, valid time, and forecast hour are deliberately not
/// included: the same formula should keep one table selection as it is
/// evaluated across a run. Recipe identity and canonical equation are
/// included so reusing an output name for different science cannot silently
/// retarget an existing sounding-table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormulaSoundingIdentity {
    stable_id: String,
}

impl FormulaSoundingIdentity {
    pub(crate) fn from_provenance(
        output_name: &str,
        provenance: &rw_formula::FormulaProvenance,
    ) -> Self {
        Self {
            stable_id: content_aware_formula_id(
                output_name,
                provenance.recipe_name.as_deref(),
                provenance.recipe_version.as_deref(),
                &provenance.canonical_source,
            ),
        }
    }

    fn stable_id(&self) -> &str {
        &self.stable_id
    }
}

/// One Formula Lab output offered to configurable sounding-table cells.
///
/// `id` is deliberately independent from the human-facing label so a saved
/// table selection remains stable.  An unavailable item stays in the catalog
/// with `value = None`; the table can render `--` and expose the reason rather
/// than silently replacing the user's selection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FormulaSoundingDiagnostic {
    pub id: String,
    pub label: String,
    pub units: String,
    /// Exact store timestep that produced this value. The sounding panel
    /// independently checks this before rendering, which keeps a retained
    /// model Formula result out of an explicitly selected RAOB.
    pub source_hour: rw_ui::HourKey,
    pub value: Option<f64>,
    pub unavailable_reason: Option<String>,
}

impl FormulaSoundingDiagnostic {
    fn unavailable(
        field: &FieldData,
        identity: Option<&FormulaSoundingIdentity>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            id: resolved_formula_id(field, identity),
            label: field.key.var.clone(),
            units: field.units.clone(),
            source_hour: field.key.hour.clone(),
            value: None,
            unavailable_reason: Some(reason.into()),
        }
    }

    fn ready(field: &FieldData, identity: Option<&FormulaSoundingIdentity>, value: f64) -> Self {
        Self {
            id: resolved_formula_id(field, identity),
            label: field.key.var.clone(),
            units: field.units.clone(),
            source_hour: field.key.hour.clone(),
            value: Some(value),
            unavailable_reason: None,
        }
    }

    fn ready_box_mean(
        field: &FieldData,
        identity: Option<&FormulaSoundingIdentity>,
        value: f64,
    ) -> Self {
        let mut ready = Self::ready(field, identity, value);
        ready.label.push_str(" (box mean)");
        ready
    }
}

fn resolved_formula_id(field: &FieldData, identity: Option<&FormulaSoundingIdentity>) -> String {
    identity.map_or_else(
        || legacy_formula_id(&field.key.var),
        |identity| identity.stable_id().to_owned(),
    )
}

fn legacy_formula_id(output_name: &str) -> String {
    // Deterministic fallback for tests and callers that install generated
    // fields without Formula provenance. Production Formula Lab evaluations
    // always retain the content-aware identity above.
    format!("formula_lab:legacy:{output_name}")
}

fn content_aware_formula_id(
    output_name: &str,
    recipe_name: Option<&str>,
    recipe_version: Option<&str>,
    canonical_source: &str,
) -> String {
    let source_hash = Sha256::digest(canonical_source.as_bytes());
    // 128 bits of SHA-256 keeps the id compact while leaving collision risk
    // negligible. Optional recipe strings are hex encoded with an explicit
    // Some/None marker so delimiters and Unicode cannot alias another recipe.
    let short_hash = hex_bytes(&source_hash[..16]);
    format!(
        "formula_lab:v1:{output_name}:{}:{}:{short_hash}",
        encoded_optional(recipe_name),
        encoded_optional(recipe_version)
    )
}

fn encoded_optional(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("s{}", hex_bytes(value.as_bytes())),
        None => "n".to_owned(),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

/// Resolve the last completed Formula Lab field for one displayed sounding.
///
/// Sampling intentionally matches `rw_store::HourReader::read_profile_3d`:
/// coordinates are clamped to the grid, the four bilinear corner weights are
/// accumulated over finite values only, and the surviving weights are
/// renormalized.  If every contributing corner is missing, the diagnostic is
/// retained but unavailable.
pub(crate) fn formula_diagnostic_for_sounding(
    field: &FieldData,
    identity: Option<&FormulaSoundingIdentity>,
    store_backed: bool,
    sounding: Option<&SoundingData>,
) -> FormulaSoundingDiagnostic {
    let sounding = match validate_formula_context(field, store_backed, sounding) {
        Ok(sounding) => sounding,
        Err(reason) => {
            return FormulaSoundingDiagnostic::unavailable(field, identity, reason);
        }
    };

    match sample_finite_bilinear(&field.values, field.nx, field.ny, sounding.fx, sounding.fy) {
        Some(value) => FormulaSoundingDiagnostic::ready(field, identity, value),
        None => FormulaSoundingDiagnostic::unavailable(
            field,
            identity,
            "Formula Lab result is missing at every contributing grid corner for this sounding.",
        ),
    }
}

/// Resolve a Formula Lab field for the same geographic footprint used by an
/// area-mean model sounding. This averages finite formula values over grid
/// cells inside the *sampled* (possibly grid-clipped) box, rather than
/// misleadingly reporting the representative sounding column's center point.
pub(crate) fn formula_diagnostic_for_box_sounding(
    field: &FieldData,
    identity: Option<&FormulaSoundingIdentity>,
    store_backed: bool,
    sounding: Option<&SoundingData>,
    sampled_bounds: (f64, f64, f64, f64),
) -> FormulaSoundingDiagnostic {
    if let Err(reason) = validate_formula_context(field, store_backed, sounding) {
        return FormulaSoundingDiagnostic::unavailable(field, identity, reason);
    }
    let (west, east, south, north) = sampled_bounds;
    if ![west, east, south, north].into_iter().all(f64::is_finite) || west > east || south > north {
        return FormulaSoundingDiagnostic::unavailable(
            field,
            identity,
            "The box sounding has invalid sampled geographic bounds.",
        );
    }

    let Some(grid) = &field.grid else {
        // `validate_formula_grid` above already catches this; retain a local
        // guard so this function remains panic-free if its gate changes.
        return FormulaSoundingDiagnostic::unavailable(
            field,
            identity,
            "Formula Lab result has no geographic grid for box averaging.",
        );
    };
    let mut sum = 0.0;
    let mut count = 0usize;
    let mut geographic_cells = 0usize;
    for index in 0..field.values.len() {
        let lat = f64::from(grid.lat[index]);
        let lon = f64::from(grid.lon[index]);
        if !(lat.is_finite()
            && lon.is_finite()
            && (south..=north).contains(&lat)
            && (west..=east).contains(&lon))
        {
            continue;
        }
        geographic_cells += 1;
        let value = field.values[index];
        if value.is_finite() {
            sum += f64::from(value);
            count += 1;
        }
    }
    if geographic_cells == 0 {
        return FormulaSoundingDiagnostic::unavailable(
            field,
            identity,
            "No Formula Lab grid cells fall inside the sampled box-sounding footprint.",
        );
    }
    if count == 0 {
        return FormulaSoundingDiagnostic::unavailable(
            field,
            identity,
            "Formula Lab result is missing at every grid cell in the sampled box-sounding footprint.",
        );
    }
    FormulaSoundingDiagnostic::ready_box_mean(field, identity, sum / count as f64)
}

fn validate_formula_context<'a>(
    field: &FieldData,
    store_backed: bool,
    sounding: Option<&'a SoundingData>,
) -> Result<&'a SoundingData, String> {
    if !store_backed {
        return Err(
            "This Formula Lab result came from a raw WRF file; evaluate it from the matching stored model timestep to use it in a sounding table."
                .to_owned(),
        );
    }
    let sounding = sounding.ok_or_else(|| {
        "Load a model sounding from this Formula Lab result's timestep.".to_owned()
    })?;
    if sounding.hour != field.key.hour {
        return Err(format!(
            "Formula Lab result is for {}; the displayed sounding is for {}.",
            field.key.hour, sounding.hour
        ));
    }
    if field.nx == 0 || field.ny == 0 {
        return Err("Formula Lab result has an empty grid.".to_owned());
    }
    let cells = field.nx.checked_mul(field.ny).ok_or_else(|| {
        "Formula Lab result grid dimensions overflow the addressable grid size.".to_owned()
    })?;
    if field.values.len() != cells {
        return Err(format!(
            "Formula Lab result has {} values, expected {cells} for its {}x{} grid.",
            field.values.len(),
            field.nx,
            field.ny
        ));
    }
    validate_formula_grid(field)?;
    if !sounding.fx.is_finite() || !sounding.fy.is_finite() {
        return Err("The displayed sounding has non-finite grid coordinates.".to_owned());
    }
    Ok(sounding)
}

fn validate_formula_grid(field: &FieldData) -> Result<(), String> {
    let Some(grid) = &field.grid else {
        return Err("Formula Lab result has no geographic grid.".to_owned());
    };
    if grid.nx != field.nx || grid.ny != field.ny {
        return Err(format!(
            "Formula Lab field is {}x{}, but its geographic grid is {}x{}.",
            field.nx, field.ny, grid.nx, grid.ny
        ));
    }
    let Some(cells) = grid.nx.checked_mul(grid.ny) else {
        return Err("Formula Lab geographic grid dimensions overflow usize.".to_owned());
    };
    if grid.lat.len() != cells || grid.lon.len() != cells {
        return Err(format!(
            "Formula Lab geographic grid has {} latitude and {} longitude values, expected {cells}.",
            grid.lat.len(),
            grid.lon.len()
        ));
    }
    Ok(())
}

fn sample_finite_bilinear(values: &[f32], nx: usize, ny: usize, fx: f64, fy: f64) -> Option<f64> {
    debug_assert!(nx > 0 && ny > 0 && values.len() == nx * ny);
    let fx = fx.clamp(0.0, (nx - 1) as f64);
    let fy = fy.clamp(0.0, (ny - 1) as f64);
    let (x0, x1) = (fx.floor() as usize, fx.ceil() as usize);
    let (y0, y1) = (fy.floor() as usize, fy.ceil() as usize);
    let wx = fx - x0 as f64;
    let wy = fy - y0 as f64;
    let corners = [
        (x0, y0, (1.0 - wx) * (1.0 - wy)),
        (x1, y0, wx * (1.0 - wy)),
        (x0, y1, (1.0 - wx) * wy),
        (x1, y1, wx * wy),
    ];

    let mut weight_sum = 0.0;
    let mut value_sum = 0.0;
    for (x, y, weight) in corners {
        let value = values[y * nx + x];
        if value.is_finite() {
            weight_sum += weight;
            value_sum += weight * f64::from(value);
        }
    }
    (weight_sum > 0.0).then_some(value_sum / weight_sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use rw_ui::rw_store::grid::GridFile;
    use rw_ui::{FieldKey, HourKey};

    fn hour(run: &str, hour: u16) -> HourKey {
        HourKey {
            model: "wrf".to_owned(),
            run: run.to_owned(),
            hour,
            exact_time: None,
        }
    }

    fn field(values: Vec<f32>) -> FieldData {
        FieldData {
            key: FieldKey {
                hour: hour("case", 3),
                var: "custom_vorticity".to_owned(),
            },
            units: "s-1".to_owned(),
            nx: 2,
            ny: 2,
            values,
            range: None,
            grid: Some(Arc::new(GridFile {
                nx: 2,
                ny: 2,
                lat: vec![1.0, 1.0, 0.0, 0.0],
                lon: vec![0.0, 1.0, 0.0, 1.0],
                projection: None,
                hash: "fixture-grid".to_owned(),
            })),
            lat_descending: false,
            style: None,
        }
    }

    fn sounding(fx: f64, fy: f64) -> SoundingData {
        SoundingData {
            hour: hour("case", 3),
            fx,
            fy,
            lat: None,
            lon: None,
            vars: Vec::new(),
            surface: Vec::new(),
            read_ms: 0.0,
        }
    }

    #[test]
    fn exact_hour_formula_is_bilinearly_sampled_and_namespaced() {
        let result = formula_diagnostic_for_sounding(
            &field(vec![0.0, 10.0, 20.0, 30.0]),
            None,
            true,
            Some(&sounding(0.25, 0.5)),
        );
        assert_eq!(result.id, "formula_lab:legacy:custom_vorticity");
        assert_eq!(result.label, "custom_vorticity");
        assert_eq!(result.units, "s-1");
        assert_eq!(result.value, Some(12.5));
        assert_eq!(result.unavailable_reason, None);
    }

    #[test]
    fn a_different_hour_retains_the_selection_but_has_no_value() {
        let mut other = sounding(0.0, 0.0);
        other.hour = hour("case", 4);
        let result = formula_diagnostic_for_sounding(
            &field(vec![1.0, 2.0, 3.0, 4.0]),
            None,
            true,
            Some(&other),
        );
        assert_eq!(result.id, "formula_lab:legacy:custom_vorticity");
        assert_eq!(result.value, None);
        assert!(
            result
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("displayed sounding"))
        );
    }

    #[test]
    fn coordinates_clamp_to_edges_like_store_profiles() {
        let result = formula_diagnostic_for_sounding(
            &field(vec![1.0, 2.0, 3.0, 4.0]),
            None,
            true,
            Some(&sounding(99.0, -20.0)),
        );
        assert_eq!(result.value, Some(2.0));
        assert_eq!(result.unavailable_reason, None);
    }

    #[test]
    fn finite_corner_weights_are_renormalized_and_all_missing_is_unavailable() {
        let partial = formula_diagnostic_for_sounding(
            &field(vec![f32::NAN, 10.0, f32::NAN, 30.0]),
            None,
            true,
            Some(&sounding(0.5, 0.5)),
        );
        assert_eq!(partial.value, Some(20.0));

        let missing = formula_diagnostic_for_sounding(
            &field(vec![f32::NAN; 4]),
            None,
            true,
            Some(&sounding(0.5, 0.5)),
        );
        assert_eq!(missing.value, None);
        assert!(
            missing
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("every contributing"))
        );
    }

    #[test]
    fn raw_wrf_result_is_visible_but_never_assigned_to_store_sounding() {
        let result = formula_diagnostic_for_sounding(
            &field(vec![1.0, 2.0, 3.0, 4.0]),
            None,
            false,
            Some(&sounding(0.0, 0.0)),
        );
        assert_eq!(result.id, "formula_lab:legacy:custom_vorticity");
        assert_eq!(result.value, None);
        assert!(
            result
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("raw WRF"))
        );
    }

    #[test]
    fn malformed_grid_or_nonfinite_coordinate_fails_closed() {
        let mut malformed = field(vec![1.0, 2.0, 3.0]);
        let bad_shape =
            formula_diagnostic_for_sounding(&malformed, None, true, Some(&sounding(0.0, 0.0)));
        assert_eq!(bad_shape.value, None);
        assert!(
            bad_shape
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("expected 4"))
        );

        malformed.values.push(4.0);
        let bad_coordinate =
            formula_diagnostic_for_sounding(&malformed, None, true, Some(&sounding(f64::NAN, 0.0)));
        assert_eq!(bad_coordinate.value, None);
        assert!(
            bad_coordinate
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("non-finite"))
        );
    }

    #[test]
    fn mismatched_geographic_grid_fails_closed() {
        let mut bad = field(vec![1.0, 2.0, 3.0, 4.0]);
        bad.grid = Some(Arc::new(GridFile {
            nx: 2,
            ny: 3,
            lat: vec![1.0; 6],
            lon: vec![0.0; 6],
            projection: None,
            hash: "wrong-grid".to_owned(),
        }));
        let result = formula_diagnostic_for_sounding(&bad, None, true, Some(&sounding(0.0, 0.0)));
        assert_eq!(result.value, None);
        assert!(
            result
                .unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("geographic grid is 2x3"))
        );
    }

    #[test]
    fn box_sounding_uses_finite_area_mean_not_center_point() {
        let result = formula_diagnostic_for_box_sounding(
            &field(vec![2.0, f32::NAN, 6.0, 100.0]),
            None,
            true,
            Some(&sounding(0.5, 0.5)),
            (-0.1, 0.1, -0.1, 1.1),
        );
        assert_eq!(result.id, "formula_lab:legacy:custom_vorticity");
        assert_eq!(result.label, "custom_vorticity (box mean)");
        assert_eq!(result.value, Some(4.0));
        assert_eq!(result.unavailable_reason, None);
    }

    #[test]
    fn formula_identity_is_content_aware_but_independent_of_model_time() {
        let first = content_aware_formula_id(
            "custom_vorticity",
            Some("Storm diagnostic"),
            Some("2.1"),
            "ddx(v) - ddy(u)",
        );
        let same = content_aware_formula_id(
            "custom_vorticity",
            Some("Storm diagnostic"),
            Some("2.1"),
            "ddx(v) - ddy(u)",
        );
        assert_eq!(first, same);
        assert!(first.starts_with("formula_lab:v1:custom_vorticity:"));

        let changed_equation = content_aware_formula_id(
            "custom_vorticity",
            Some("Storm diagnostic"),
            Some("2.1"),
            "ddy(u) - ddx(v)",
        );
        let changed_recipe = content_aware_formula_id(
            "custom_vorticity",
            Some("Different diagnostic"),
            Some("2.1"),
            "ddx(v) - ddy(u)",
        );
        assert_ne!(first, changed_equation);
        assert_ne!(first, changed_recipe);
    }

    #[test]
    fn retained_identity_overrides_legacy_output_name_fallback() {
        let identity = FormulaSoundingIdentity {
            stable_id: content_aware_formula_id("custom_vorticity", None, None, "ddx(v) - ddy(u)"),
        };
        let result = formula_diagnostic_for_sounding(
            &field(vec![1.0, 2.0, 3.0, 4.0]),
            Some(&identity),
            true,
            Some(&sounding(0.0, 0.0)),
        );
        assert_eq!(result.id, identity.stable_id);
        assert!(!result.id.contains(":legacy:"));
    }
}
