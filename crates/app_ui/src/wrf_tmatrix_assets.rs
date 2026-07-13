//! Fail-closed embedded and validated-local property T-matrix table sources
//! used by the WRF simulated radar research operator.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use radar_scattering::{
    P3OfficialTableKind, PolarAccumulatorQuantities, ResearchTMatrixLut, Sha256Digest,
};

use crate::wrf_property_reader::{RawPropertyCell, WrfPropertyScene};
use crate::wrf_tmatrix_band_assets::{
    LocalDirectoryTMatrixBandPackProvider, OwnedTMatrixBandBundle, S_BAND_RESEARCH_FREQUENCY_HZ,
    TMatrixBandPackCache, TMatrixBandPackDiscovery, TMatrixBandPackIdentity, TMatrixResearchBand,
    discover_tmatrix_band_packs, legacy_embedded_s_research_v1_identity,
};
use crate::wrf_tmatrix_scene::{
    WrfTMatrixBuildPeakEstimate, WrfTMatrixLutBundle, WrfTMatrixP3Resources, WrfTMatrixRainMode,
    WrfTMatrixRawEvaluator, WrfTMatrixScene,
};

const ASSET_ROOT: &str = "../../../research_only_assets/tmatrix/pytmatrix-0.3.3";
const TMATRIX_PACK_CACHE_FAMILY: &str = "bowecho-simradar/tmatrix-packs";

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
    "9a2ed9a7678fd6e8dcbaa4d26bad24fe47fe7cf95ae5eb17ad80d63a34c06b56"
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
static P3_TWO_MOMENT_RAW_EVALUATOR: OnceLock<Result<WrfTMatrixRawEvaluator<'static>, String>> =
    OnceLock::new();
static P3_THREE_MOMENT_RAW_EVALUATOR: OnceLock<Result<WrfTMatrixRawEvaluator<'static>, String>> =
    OnceLock::new();
static EXTERNAL_PROPERTY_LUT_CACHE: OnceLock<TMatrixBandPackCache> = OnceLock::new();

/// Explicit table source. Selection never changes this request to another
/// source or another frequency: callers that request an external pack receive
/// an error if that exact pack is unavailable or unvalidated.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PropertyTMatrixTableSourceKind {
    #[default]
    LegacyEmbeddedSResearchV1,
    ExternalValidatedPack,
}

#[derive(Clone)]
enum PropertyTMatrixTableStorage {
    LegacyEmbeddedS(&'static EmbeddedPropertyTMatrixLuts),
    ExternalValidated(Arc<OwnedTMatrixBandBundle>),
}

/// An ownership handle for one exact-frequency five-table source.
///
/// Keep this value alive for at least as long as anything borrowing the bundle
/// returned by [`Self::borrowed_bundle`]. In particular, the external variant
/// owns the `Arc` retained by the bounded process cache instead of manufacturing
/// a `'static` reference.
#[derive(Clone)]
pub struct PropertyTMatrixTables {
    storage: PropertyTMatrixTableStorage,
}

impl PropertyTMatrixTables {
    #[must_use]
    pub fn identity(&self) -> TMatrixBandPackIdentity {
        match &self.storage {
            PropertyTMatrixTableStorage::LegacyEmbeddedS(_) => {
                legacy_embedded_s_research_v1_identity()
            }
            PropertyTMatrixTableStorage::ExternalValidated(bundle) => {
                bundle.identity().clone().into()
            }
        }
    }

    #[must_use]
    pub fn source_kind(&self) -> PropertyTMatrixTableSourceKind {
        match &self.storage {
            PropertyTMatrixTableStorage::LegacyEmbeddedS(_) => {
                PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
            }
            PropertyTMatrixTableStorage::ExternalValidated(_) => {
                PropertyTMatrixTableSourceKind::ExternalValidatedPack
            }
        }
    }

    #[must_use]
    pub fn exact_frequency_hz(&self) -> f64 {
        match &self.storage {
            PropertyTMatrixTableStorage::LegacyEmbeddedS(_) => S_BAND_RESEARCH_FREQUENCY_HZ,
            PropertyTMatrixTableStorage::ExternalValidated(bundle) => {
                bundle.identity().exact_frequency_hz()
            }
        }
    }

    #[must_use]
    pub fn borrowed_bundle(&self) -> WrfTMatrixLutBundle<'_> {
        match &self.storage {
            PropertyTMatrixTableStorage::LegacyEmbeddedS(tables) => tables.bundle(),
            PropertyTMatrixTableStorage::ExternalValidated(bundle) => bundle.borrowed_bundle(),
        }
    }

    /// Conservative resident bytes owned by this exact table source. Embedded
    /// tables use the established compiled-asset bound; external packs bound
    /// source bytes plus decoded runtime storage at twice the validated source
    /// total, matching the embedded accounting policy.
    #[must_use]
    pub fn retained_source_bytes(&self) -> usize {
        match &self.storage {
            PropertyTMatrixTableStorage::LegacyEmbeddedS(_) => embedded_lut_memory_bytes(),
            PropertyTMatrixTableStorage::ExternalValidated(bundle) => {
                bundle.retained_source_bytes().saturating_mul(2)
            }
        }
    }
}

/// Deterministic external-pack root below BowEcho's override-aware model cache.
/// No downloader writes here: users or a separate pack installer provide the
/// signed-off local manifests and role files.
#[must_use]
pub fn property_tmatrix_pack_cache_dir() -> PathBuf {
    settings::model_cache_dir().join(TMATRIX_PACK_CACHE_FAMILY)
}

/// Inspect external packs without loading their large role files. Invalid and
/// unvalidated manifests remain visible in the returned discovery report and
/// can never satisfy exact selection.
pub fn discover_cached_property_tmatrix_packs() -> Result<Vec<TMatrixBandPackDiscovery>, String> {
    let root = property_tmatrix_pack_cache_dir();
    std::fs::create_dir_all(&root).map_err(|error| {
        format!(
            "create simulated-radar T-matrix pack directory {}: {error}",
            root.display()
        )
    })?;
    discover_tmatrix_band_packs(&LocalDirectoryTMatrixBandPackProvider::new(root))
        .map_err(|error| format!("discover local simulated-radar T-matrix packs: {error}"))
}

/// Resolve one explicitly requested source at one exact supported frequency.
///
/// Legacy embedded S is accepted only at 2.8 GHz. External S/C/X selection is
/// delegated to the validated local manifests and therefore fails closed for
/// a missing, invalid, unvalidated, or ambiguous pack. There is no cross-band
/// substitution, nearest-frequency lookup, or implicit source fallback.
pub fn load_property_tmatrix_tables_exact(
    source: PropertyTMatrixTableSourceKind,
    frequency_hz: f64,
) -> Result<PropertyTMatrixTables, String> {
    validate_table_source_frequency(source, frequency_hz)?;
    match source {
        PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1 => Ok(PropertyTMatrixTables {
            storage: PropertyTMatrixTableStorage::LegacyEmbeddedS(embedded_luts()?),
        }),
        PropertyTMatrixTableSourceKind::ExternalValidatedPack => {
            let root = property_tmatrix_pack_cache_dir();
            std::fs::create_dir_all(&root).map_err(|error| {
                format!(
                    "create simulated-radar T-matrix pack directory {}: {error}",
                    root.display()
                )
            })?;
            let provider = LocalDirectoryTMatrixBandPackProvider::new(&root);
            let discoveries = discover_tmatrix_band_packs(&provider).map_err(|error| {
                format!(
                    "discover simulated-radar T-matrix packs in {}: {error}",
                    root.display()
                )
            })?;
            let bundle = EXTERNAL_PROPERTY_LUT_CACHE
                .get_or_init(TMatrixBandPackCache::default)
                .load_exact_frequency(&provider, &discoveries, frequency_hz)
                .map_err(|error| {
                    format!(
                        "load validated external T-matrix pack at exactly {frequency_hz} Hz from {}: {error}",
                        root.display()
                    )
                })?;
            Ok(PropertyTMatrixTables {
                storage: PropertyTMatrixTableStorage::ExternalValidated(bundle),
            })
        }
    }
}

fn validate_table_source_frequency(
    source: PropertyTMatrixTableSourceKind,
    frequency_hz: f64,
) -> Result<TMatrixResearchBand, String> {
    let band = TMatrixResearchBand::from_exact_frequency_hz(frequency_hz)
        .map_err(|error| error.to_string())?;
    if source == PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
        && frequency_hz.to_bits() != S_BAND_RESEARCH_FREQUENCY_HZ.to_bits()
    {
        return Err(format!(
            "legacy embedded S research v1 exists only at exactly {S_BAND_RESEARCH_FREQUENCY_HZ} Hz; requested {frequency_hz} Hz ({band}-band); no fallback is allowed"
        ));
    }
    Ok(band)
}

pub struct PropertyTMatrixSceneBuild {
    pub scene: WrfTMatrixScene,
    pub peak: WrfTMatrixBuildPeakEstimate,
    pub table_identity: TMatrixBandPackIdentity,
    pub table_source: PropertyTMatrixTableSourceKind,
    pub exact_frequency_hz: f64,
    pub rain_mode: WrfTMatrixRainMode,
}

pub type EmbeddedPropertySceneBuild = PropertyTMatrixSceneBuild;

/// Load and validate the complete five-table bundle before opening the heavy
/// WRF property fields. The result is cached for the lifetime of the process;
/// a failed embedded contract can never fall through to another kernel.
pub fn preload_embedded_property_tmatrix_luts() -> Result<(), String> {
    embedded_luts().map(|_| ())
}

/// Qualify every immutable table needed by one native property scheme before
/// the heavy WRF scene read begins. P3 50--53 lazily acquires the exact pinned
/// official 2-moment or 3-moment v5.4 table; ISHMAEL needs only the embedded
/// five-role scattering bundle.
pub fn preload_embedded_property_tmatrix_for_scheme(
    microphysics_scheme_id: i32,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<(), String> {
    match microphysics_scheme_id {
        50..=52 => embedded_p3_raw_evaluator(P3OfficialTableKind::TwoMoment, progress).map(drop),
        53 => embedded_p3_raw_evaluator(P3OfficialTableKind::ThreeMoment, progress).map(drop),
        55 => preload_embedded_property_tmatrix_luts(),
        other => Err(format!(
            "property T-matrix tables do not support WRF mp_physics={other}"
        )),
    }
}

/// Evaluate one already spatially/temporally blended raw property cell through
/// the validated embedded bundle. The reusable evaluator is cached, so the
/// complete table contract is gated once rather than once per radar sample.
pub fn evaluate_embedded_raw_property_cell(
    raw: &RawPropertyCell,
    elevation_deg: f64,
) -> Result<Option<PolarAccumulatorQuantities>, String> {
    let evaluator = match raw.microphysics_scheme_id() {
        50..=52 => embedded_p3_raw_evaluator(P3OfficialTableKind::TwoMoment, &|_| {})?,
        53 => embedded_p3_raw_evaluator(P3OfficialTableKind::ThreeMoment, &|_| {})?,
        _ => embedded_raw_evaluator()?,
    };
    evaluator
        .evaluate(raw, elevation_deg)
        .map_err(|error| format!("evaluate embedded raw property cell: {error}"))
}

fn embedded_raw_evaluator() -> Result<WrfTMatrixRawEvaluator<'static>, String> {
    match RAW_EVALUATOR.get_or_init(|| {
        WrfTMatrixRawEvaluator::new(embedded_luts()?.bundle())
            .map_err(|error| format!("validate embedded raw property evaluator: {error}"))
    }) {
        Ok(evaluator) => Ok(evaluator.clone()),
        Err(error) => Err(error.clone()),
    }
}

fn embedded_p3_raw_evaluator(
    kind: P3OfficialTableKind,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<WrfTMatrixRawEvaluator<'static>, String> {
    // Acquisition sits outside OnceLock initialization so a transient network
    // failure is retryable. The asset module itself success-caches the Arc.
    let table = crate::wrf_p3_assets::load_or_download_official_p3_table(kind, progress)?;
    let slot = match kind {
        P3OfficialTableKind::TwoMoment => &P3_TWO_MOMENT_RAW_EVALUATOR,
        P3OfficialTableKind::ThreeMoment => &P3_THREE_MOMENT_RAW_EVALUATOR,
    };
    match slot.get_or_init(move || {
        let p3 = WrfTMatrixP3Resources::projected_area_equivalent_oblate_research(table).map_err(
            |error| format!("configure P3 projected-area T-matrix integration: {error}"),
        )?;
        WrfTMatrixRawEvaluator::new_with_p3(embedded_luts()?.bundle(), p3)
            .map_err(|error| format!("validate embedded P3 raw property evaluator: {error}"))
    }) {
        Ok(evaluator) => Ok(evaluator.clone()),
        Err(error) => Err(error.clone()),
    }
}

fn embedded_p3_resources(
    microphysics_scheme_id: i32,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<Option<WrfTMatrixP3Resources>, String> {
    let kind = match microphysics_scheme_id {
        50..=52 => P3OfficialTableKind::TwoMoment,
        53 => P3OfficialTableKind::ThreeMoment,
        _ => return Ok(None),
    };
    let table = crate::wrf_p3_assets::load_or_download_official_p3_table(kind, progress)?;
    WrfTMatrixP3Resources::projected_area_equivalent_oblate_research(table)
        .map(Some)
        .map_err(|error| format!("configure P3 projected-area T-matrix integration: {error}"))
}

/// Build one compact scattering scene from an already-resolved exact table
/// owner. The owner remains alive through estimation and evaluation, so an
/// external bundle is never self-referenced or promoted to a fake `'static`
/// lifetime.
pub fn build_property_tmatrix_scene(
    source: &WrfPropertyScene,
    maximum_owned_peak_bytes: usize,
    table_owner: PropertyTMatrixTables,
    rain_mode: WrfTMatrixRainMode,
    progress: &(impl Fn(&str) + ?Sized),
) -> Result<PropertyTMatrixSceneBuild, String> {
    let table_identity = table_owner.identity();
    let table_source = table_owner.source_kind();
    let exact_frequency_hz = table_owner.exact_frequency_hz();
    let tables = table_owner.borrowed_bundle();
    let p3 = embedded_p3_resources(source.microphysics_scheme_id(), progress)?;
    let peak = match p3.as_ref() {
        Some(p3) => WrfTMatrixScene::estimate_build_peak_with_p3(source, tables, rain_mode, p3),
        None => WrfTMatrixScene::estimate_build_peak(source, tables, rain_mode),
    }
    .map_err(|error| format!("estimate property-scattering build: {error}"))?;
    if peak.estimated_peak_bytes > maximum_owned_peak_bytes {
        return Err(format!(
            "property-scattering build needs {:.2} GiB for raw state, output plane, lookups and build scratch, but only {:.2} GiB remains inside the configured budget",
            peak.estimated_peak_bytes as f64 / 1024.0_f64.powi(3),
            maximum_owned_peak_bytes as f64 / 1024.0_f64.powi(3),
        ));
    }
    let scene = match p3 {
        Some(p3) => WrfTMatrixScene::build_with_p3_and_rain_mode(source, tables, p3, rain_mode),
        None => WrfTMatrixScene::build_with_rain_mode(source, tables, rain_mode),
    }
    .map_err(|error| format!("evaluate selected property-scattering tables: {error}"))?;
    Ok(PropertyTMatrixSceneBuild {
        scene,
        peak,
        table_identity,
        table_source,
        exact_frequency_hz,
        rain_mode,
    })
}

/// Backward-compatible embedded-S/full-property seam used by focused assets
/// tests and older callers.
pub fn build_embedded_property_tmatrix_scene(
    source: &WrfPropertyScene,
    maximum_owned_peak_bytes: usize,
) -> Result<EmbeddedPropertySceneBuild, String> {
    build_property_tmatrix_scene(
        source,
        maximum_owned_peak_bytes,
        load_property_tmatrix_tables_exact(
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
            S_BAND_RESEARCH_FREQUENCY_HZ,
        )?,
        WrfTMatrixRainMode::FullProperty,
        &|_| {},
    )
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
    use std::collections::BTreeSet;

    use radar_scattering::AxisKind;

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
                "property-rain-sband-pytmatrix-0.3.3-unvalidated-v2",
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
        let resolved = PropertyTMatrixTables {
            storage: PropertyTMatrixTableStorage::LegacyEmbeddedS(tables),
        };
        assert_eq!(
            resolved.identity(),
            TMatrixBandPackIdentity::LegacyEmbeddedSResearchV1
        );
        assert_eq!(
            resolved.source_kind(),
            PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1
        );
        assert_eq!(
            resolved.exact_frequency_hz().to_bits(),
            S_BAND_RESEARCH_FREQUENCY_HZ.to_bits()
        );
        resolved
            .borrowed_bundle()
            .validate()
            .expect("resolved legacy handle borrows the embedded bundle");
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

    #[test]
    fn source_routing_accepts_only_declared_exact_frequencies() {
        assert_eq!(
            validate_table_source_frequency(
                PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
                S_BAND_RESEARCH_FREQUENCY_HZ,
            )
            .expect("legacy exact S"),
            TMatrixResearchBand::S
        );
        for band in [
            TMatrixResearchBand::S,
            TMatrixResearchBand::C,
            TMatrixResearchBand::X,
        ] {
            assert_eq!(
                validate_table_source_frequency(
                    PropertyTMatrixTableSourceKind::ExternalValidatedPack,
                    band.exact_frequency_hz(),
                )
                .expect("declared external frequency"),
                band
            );
        }
        for band in [TMatrixResearchBand::C, TMatrixResearchBand::X] {
            let error = validate_table_source_frequency(
                PropertyTMatrixTableSourceKind::LegacyEmbeddedSResearchV1,
                band.exact_frequency_hz(),
            )
            .expect_err("legacy S cannot satisfy another band");
            assert!(error.contains("no fallback"));
        }
        assert!(
            validate_table_source_frequency(
                PropertyTMatrixTableSourceKind::ExternalValidatedPack,
                S_BAND_RESEARCH_FREQUENCY_HZ + 1.0,
            )
            .is_err()
        );
    }

    #[test]
    fn external_pack_directory_is_scoped_below_the_model_cache() {
        assert_eq!(
            property_tmatrix_pack_cache_dir(),
            settings::model_cache_dir().join(TMATRIX_PACK_CACHE_FAMILY)
        );
        assert!(
            property_tmatrix_pack_cache_dir()
                .ends_with(std::path::Path::new("bowecho-simradar").join("tmatrix-packs"))
        );
        assert!(!TMATRIX_PACK_CACHE_FAMILY.contains("simsat-cache"));
    }

    #[test]
    fn real_p3_fixture_closes_and_evaluates_embedded_rain() {
        let (Some(wrf_path), Some(table_path)) = (
            std::env::var_os("BOWECHO_WRF_PROPERTY_FIXTURE"),
            std::env::var_os("BOWECHO_P3_TABLE_FIXTURE"),
        ) else {
            return;
        };

        let file = wrf_core::WrfFile::open(&wrf_path).expect("open BOWECHO_WRF_PROPERTY_FIXTURE");
        let scheme_id = file
            .global_attr_i32("MP_PHYSICS")
            .expect("fixture has MP_PHYSICS");
        let table_kind = match scheme_id {
            50..=52 => P3OfficialTableKind::TwoMoment,
            53 => P3OfficialTableKind::ThreeMoment,
            other => panic!("property fixture must use P3 mp_physics 50-53, got {other}"),
        };
        let scene = crate::wrf_property_reader::read_wrf_property_scene(
            &file,
            crate::wrf_scene_inventory::WrfSourceIdentity(
                "fixture:p3-embedded-rain-evaluator".to_owned(),
            ),
            0,
        )
        .expect("read normalized P3 property scene");

        let source_qrain = file.read_var("QRAIN", 0).expect("read fixture QRAIN");
        let qsmall = f64::from(radar_scattering::P3_WRF_QSMALL_KGKG);
        let active_rain_indices = source_qrain
            .iter()
            .enumerate()
            .filter(|&(_, &mass)| mass >= qsmall)
            .map(|(cell_index, _)| cell_index)
            .collect::<Vec<_>>();
        drop(source_qrain);
        file.clear_cache();
        assert!(
            !active_rain_indices.is_empty(),
            "fixture must contain active P3 rain"
        );

        let rain_lut = &embedded_luts().expect("load embedded T-matrix tables").rain;
        let axis_bounds = |kind| {
            let coordinates = rain_lut
                .offline_lut()
                .header()
                .axes()
                .iter()
                .find(|axis| axis.kind() == kind)
                .unwrap_or_else(|| panic!("embedded rain table is missing {kind:?}"))
                .coordinates();
            (*coordinates.first().unwrap(), *coordinates.last().unwrap())
        };
        let diameter_bounds = axis_bounds(AxisKind::EquivolumeDiameter);
        let temperature_bounds = axis_bounds(AxisKind::Temperature);
        let axis_ratio_bounds = axis_bounds(AxisKind::MinorToMajorAxisRatio);

        let mut rain_only_indices = Vec::new();
        let mut combined_indices = Vec::new();
        let mut diameter_extrema = [(f64::INFINITY, 0_usize), (f64::NEG_INFINITY, 0_usize)];
        let mut temperature_extrema = [(f64::INFINITY, 0_usize), (f64::NEG_INFINITY, 0_usize)];
        let mut axis_ratio_extrema = [(f64::INFINITY, 0_usize), (f64::NEG_INFINITY, 0_usize)];
        for &cell_index in &active_rain_indices {
            let raw = scene
                .raw_cell(cell_index)
                .unwrap_or_else(|error| panic!("read active-rain cell {cell_index}: {error}"));
            let closed = crate::wrf_property_reader::close_raw_rain_state(
                &raw,
                radar_scattering::OrientationDefinition::Gaussian20Research,
            )
            .unwrap_or_else(|error| panic!("close active rain at cell {cell_index}: {error}"));
            let crate::wrf_property_reader::ClosedRainState::Closed(rain) = closed else {
                panic!("source-active rain at cell {cell_index} did not close as rain")
            };
            assert_eq!(
                rain.shape().bulk_density_kg_m3().to_bits(),
                radar_scattering::LIQUID_WATER_DENSITY_KG_M3.to_bits(),
                "active rain at cell {cell_index} does not use the T-matrix material density"
            );
            let diameter_m = rain.characteristic_diameter_m().value();
            let temperature_k = raw.environment().temperature_k();
            let axis_ratio = rain.minor_to_major_axis_ratio().value();
            for (label, value, (minimum, maximum)) in [
                ("diameter", diameter_m, diameter_bounds),
                ("temperature", temperature_k, temperature_bounds),
                ("axis ratio", axis_ratio, axis_ratio_bounds),
            ] {
                assert!(
                    value.is_finite() && minimum <= value && value <= maximum,
                    "active rain {label} {value} at cell {cell_index} is outside embedded LUT [{minimum}, {maximum}]"
                );
            }
            for (value, extrema) in [
                (diameter_m, &mut diameter_extrema),
                (temperature_k, &mut temperature_extrema),
                (axis_ratio, &mut axis_ratio_extrema),
            ] {
                if value < extrema[0].0 {
                    extrema[0] = (value, cell_index);
                }
                if value > extrema[1].0 {
                    extrema[1] = (value, cell_index);
                }
            }
            if raw
                .categories()
                .iter()
                .any(|category| category.mixing_ratio_kgkg() > 0.0)
            {
                combined_indices.push(cell_index);
            } else {
                rain_only_indices.push(cell_index);
            }
        }
        assert!(
            !rain_only_indices.is_empty(),
            "fixture must contain a rain-only evaluator cell"
        );
        assert!(
            !combined_indices.is_empty(),
            "fixture must contain a combined frozen-and-rain evaluator cell"
        );

        let official_table =
            radar_scattering::P3OfficialTableV54::load_path(table_kind, table_path)
                .expect("load BOWECHO_P3_TABLE_FIXTURE");
        let p3 = WrfTMatrixP3Resources::projected_area_equivalent_oblate_research(Arc::new(
            official_table,
        ))
        .expect("configure production P3 integration");
        let evaluator = WrfTMatrixRawEvaluator::new_with_p3(
            embedded_luts()
                .expect("load embedded T-matrix tables")
                .bundle(),
            p3,
        )
        .expect("construct embedded P3 raw evaluator");

        const STRATIFIED_SAMPLES_PER_CLASS: usize = 128;
        fn add_stratified_samples(
            output: &mut BTreeSet<usize>,
            indices: &[usize],
            maximum_samples: usize,
        ) {
            let stride = indices.len().div_ceil(maximum_samples).max(1);
            output.extend(
                indices
                    .iter()
                    .step_by(stride)
                    .take(maximum_samples)
                    .copied(),
            );
            if let Some(&last) = indices.last() {
                output.insert(last);
            }
        }

        let mut evaluator_indices = BTreeSet::new();
        add_stratified_samples(
            &mut evaluator_indices,
            &rain_only_indices,
            STRATIFIED_SAMPLES_PER_CLASS,
        );
        add_stratified_samples(
            &mut evaluator_indices,
            &combined_indices,
            STRATIFIED_SAMPLES_PER_CLASS,
        );
        evaluator_indices.extend(
            diameter_extrema
                .into_iter()
                .chain(temperature_extrema)
                .chain(axis_ratio_extrema)
                .map(|(_, cell_index)| cell_index),
        );
        const REPORTED_RAIN_FAILURE_CELL: usize = 4_680_030;
        if REPORTED_RAIN_FAILURE_CELL < scene.cell_count() {
            evaluator_indices.insert(REPORTED_RAIN_FAILURE_CELL);
        }

        for cell_index in evaluator_indices.iter().copied() {
            let raw = scene
                .raw_cell(cell_index)
                .unwrap_or_else(|error| panic!("read evaluator sample cell {cell_index}: {error}"));
            evaluator.evaluate(&raw, 0.5).unwrap_or_else(|error| {
                panic!("evaluate embedded raw property cell {cell_index}: {error}")
            });
        }
        eprintln!(
            "embedded P3 rain fixture: active_rain={}, rain_only={}, combined={}, evaluated={}",
            active_rain_indices.len(),
            rain_only_indices.len(),
            combined_indices.len(),
            evaluator_indices.len()
        );
    }
}
