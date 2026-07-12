//! Embedded, fail-closed property T-matrix tables used by the WRF simulated
//! radar research operator.

use std::sync::OnceLock;

use radar_scattering::{PolarAccumulatorQuantities, ResearchTMatrixLut, Sha256Digest};

use crate::wrf_property_reader::{RawPropertyCell, WrfPropertyScene};
use crate::wrf_tmatrix_scene::{
    WrfTMatrixBuildPeakEstimate, WrfTMatrixLutBundle, WrfTMatrixRainMode, WrfTMatrixRawEvaluator,
    WrfTMatrixScene,
};

const ASSET_ROOT: &str = "../../../research_only_assets/tmatrix/pytmatrix-0.3.3";

macro_rules! embedded_table {
    ($prefix:ident, $directory:literal, $sha256:literal) => {
        const $prefix: (&[u8], &[u8], &str) = (
            include_bytes!(concat!(
                "../../../research_only_assets/tmatrix/pytmatrix-0.3.3/",
                $directory,
                "/table.lut"
            )),
            include_bytes!(concat!(
                "../../../research_only_assets/tmatrix/pytmatrix-0.3.3/",
                $directory,
                "/config.json"
            )),
            $sha256,
        );
    };
}

embedded_table!(
    DRY_OBLATE,
    "property_p3_ishmael_dry_oblate_sband_unvalidated",
    "30c8da4093b845faa415339f2cb5b4831f3450dc18afea3aacb2e2fabdcc4ad8"
);
embedded_table!(
    DRY_PROLATE,
    "property_p3_ishmael_dry_prolate_sband_unvalidated",
    "7a563e1103cb1a61ccb94ce72513d82b9fdd68a6faddb4aa8ae46112fb0109c0"
);
embedded_table!(
    WET_OBLATE,
    "property_p3_ishmael_wet_oblate_sband_unvalidated",
    "6c376422c512ebfc37dc5b2038defea799995d1821170da74b4af87276df1dd7"
);
embedded_table!(
    WET_PROLATE,
    "property_p3_ishmael_wet_prolate_sband_unvalidated",
    "9c55a51eb63a982005564eb1f35bbb24dfad5f22a65ed820ac7c1d5cf19f1040"
);
embedded_table!(
    RAIN,
    "property_rain_sband_unvalidated",
    "e6f37ab31fb6fb634d11cdb1cdb615e0d19217ebc7d11056c2e52cdd975a76ea"
);

struct EmbeddedPropertyTMatrixLuts {
    dry_oblate: ResearchTMatrixLut,
    dry_prolate: ResearchTMatrixLut,
    wet_oblate: ResearchTMatrixLut,
    wet_prolate: ResearchTMatrixLut,
    rain: ResearchTMatrixLut,
}

impl EmbeddedPropertyTMatrixLuts {
    fn bundle(&self) -> WrfTMatrixLutBundle<'_> {
        WrfTMatrixLutBundle::new(
            &self.dry_oblate,
            &self.dry_prolate,
            &self.wet_oblate,
            &self.wet_prolate,
            &self.rain,
        )
    }
}

static PROPERTY_LUTS: OnceLock<Result<EmbeddedPropertyTMatrixLuts, String>> = OnceLock::new();
static RAW_EVALUATOR: OnceLock<Result<WrfTMatrixRawEvaluator<'static>, String>> = OnceLock::new();

pub struct EmbeddedPropertySceneBuild {
    pub scene: WrfTMatrixScene,
    pub peak: WrfTMatrixBuildPeakEstimate,
}

/// Load and validate the complete five-table bundle before opening the heavy
/// WRF property fields. The result is cached for the lifetime of the process;
/// a failed embedded contract can never fall through to another kernel.
pub fn preload_embedded_property_tmatrix_luts() -> Result<(), String> {
    embedded_luts().map(|_| ())
}

/// Evaluate one already spatially/temporally blended raw property cell through
/// the validated embedded bundle. The reusable evaluator is cached, so the
/// complete table contract is gated once rather than once per radar sample.
pub fn evaluate_embedded_raw_property_cell(
    raw: &RawPropertyCell,
    elevation_deg: f64,
) -> Result<Option<PolarAccumulatorQuantities>, String> {
    embedded_raw_evaluator()?
        .evaluate(raw, elevation_deg)
        .map_err(|error| format!("evaluate embedded raw property cell: {error}"))
}

fn embedded_raw_evaluator() -> Result<WrfTMatrixRawEvaluator<'static>, String> {
    match RAW_EVALUATOR.get_or_init(|| {
        WrfTMatrixRawEvaluator::new(embedded_luts()?.bundle())
            .map_err(|error| format!("validate embedded raw property evaluator: {error}"))
    }) {
        Ok(evaluator) => Ok(*evaluator),
        Err(error) => Err(error.clone()),
    }
}

/// Build one compact scattering scene from the exact embedded research tables.
pub fn build_embedded_property_tmatrix_scene(
    source: &WrfPropertyScene,
    maximum_owned_peak_bytes: usize,
) -> Result<EmbeddedPropertySceneBuild, String> {
    let tables = embedded_luts()?.bundle();
    let rain_mode = WrfTMatrixRainMode::FullProperty;
    let peak = WrfTMatrixScene::estimate_build_peak(source, tables, rain_mode)
        .map_err(|error| format!("estimate embedded property-scattering build: {error}"))?;
    if peak.estimated_peak_bytes > maximum_owned_peak_bytes {
        return Err(format!(
            "property-scattering build needs {:.2} GiB for raw state, output plane, lookups and build scratch, but only {:.2} GiB remains inside the configured budget",
            peak.estimated_peak_bytes as f64 / 1024.0_f64.powi(3),
            maximum_owned_peak_bytes as f64 / 1024.0_f64.powi(3),
        ));
    }
    let scene = WrfTMatrixScene::build_with_rain_mode(source, tables, rain_mode)
        .map_err(|error| format!("evaluate embedded property-scattering tables: {error}"))?;
    Ok(EmbeddedPropertySceneBuild { scene, peak })
}

fn embedded_luts() -> Result<&'static EmbeddedPropertyTMatrixLuts, String> {
    match PROPERTY_LUTS.get_or_init(load_embedded_luts) {
        Ok(tables) => Ok(tables),
        Err(error) => Err(error.clone()),
    }
}

fn load_embedded_luts() -> Result<EmbeddedPropertyTMatrixLuts, String> {
    let tables = EmbeddedPropertyTMatrixLuts {
        dry_oblate: load_one("dry oblate", DRY_OBLATE)?,
        dry_prolate: load_one("dry prolate", DRY_PROLATE)?,
        wet_oblate: load_one("wet oblate", WET_OBLATE)?,
        wet_prolate: load_one("wet prolate", WET_PROLATE)?,
        rain: load_one("standalone/residual rain", RAIN)?,
    };
    tables
        .bundle()
        .validate()
        .map_err(|error| format!("validate complete embedded property T-matrix bundle: {error}"))?;
    Ok(tables)
}

fn load_one(
    label: &str,
    (lut_bytes, config_bytes, expected_sha256): (&[u8], &[u8], &str),
) -> Result<ResearchTMatrixLut, String> {
    let expected = Sha256Digest::from_hex(expected_sha256)
        .map_err(|error| format!("invalid embedded {label} SHA-256 constant: {error}"))?;
    ResearchTMatrixLut::load(lut_bytes, expected, config_bytes)
        .map_err(|error| format!("load embedded {label} table from {ASSET_ROOT}: {error}"))
}

/// Conservative resident bytes for the five compiled files plus their decoded
/// immutable runtime tables. Two complete file lengths bound the static bytes,
/// decoded payload/header allocations and small descriptor overhead.
#[must_use]
pub const fn embedded_lut_memory_bytes() -> usize {
    let file_and_config_bytes = DRY_OBLATE.0.len()
        + DRY_OBLATE.1.len()
        + DRY_PROLATE.0.len()
        + DRY_PROLATE.1.len()
        + WET_OBLATE.0.len()
        + WET_OBLATE.1.len()
        + WET_PROLATE.0.len()
        + WET_PROLATE.1.len()
        + RAIN.0.len()
        + RAIN.1.len();
    file_and_config_bytes.saturating_mul(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tables_pass_whole_file_and_typed_runtime_gates() {
        preload_embedded_property_tmatrix_luts().expect("preload complete embedded bundle");
        let tables = embedded_luts().expect("all five embedded research tables");
        for (table, expected_id, expected_sha256) in [
            (
                &tables.dry_oblate,
                "property-p3-ishmael-dry-oblate-sband-pytmatrix-0.3.3-unvalidated-v1",
                DRY_OBLATE.2,
            ),
            (
                &tables.dry_prolate,
                "property-p3-ishmael-dry-prolate-sband-pytmatrix-0.3.3-unvalidated-v1",
                DRY_PROLATE.2,
            ),
            (
                &tables.wet_oblate,
                "property-p3-ishmael-wet-oblate-sband-pytmatrix-0.3.3-unvalidated-v1",
                WET_OBLATE.2,
            ),
            (
                &tables.wet_prolate,
                "property-p3-ishmael-wet-prolate-sband-pytmatrix-0.3.3-unvalidated-v1",
                WET_PROLATE.2,
            ),
            (
                &tables.rain,
                "property-rain-sband-pytmatrix-0.3.3-unvalidated-v1",
                RAIN.2,
            ),
        ] {
            assert_eq!(table.descriptor().table_id(), expected_id);
            assert_eq!(
                table.file_sha256(),
                Sha256Digest::from_hex(expected_sha256).expect("valid frozen table SHA-256")
            );
        }
        tables
            .bundle()
            .validate()
            .expect("embedded tables share the exact complete bundle contract");
        assert!(embedded_lut_memory_bytes() >= 2 * RAIN.0.len());
    }

    #[test]
    fn bundle_validation_rejects_a_role_swap() {
        let tables = embedded_luts().expect("all five embedded research tables");
        let swapped = WrfTMatrixLutBundle::new(
            &tables.dry_prolate,
            &tables.dry_oblate,
            &tables.wet_oblate,
            &tables.wet_prolate,
            &tables.rain,
        );
        assert!(swapped.validate().is_err());
    }
}
