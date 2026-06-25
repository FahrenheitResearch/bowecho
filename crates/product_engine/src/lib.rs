//! Radar product derivation and registry support.
//!
//! The engine has three deliberately separate layers:
//! - [`sweep`] derives products that live on one elevation cut and can therefore
//!   be inserted into `ElevationCut::moments`.
//! - [`volume`] derives products that require the vertical column from multiple
//!   elevation cuts.
//! - [`temporal`] combines already co-registered grids from multiple volumes.
//!
//! Keeping these layers separate prevents a volume product such as composite
//! reflectivity or VIL from being mislabeled as a native single-sweep moment.

mod sweep;
mod temporal;
mod volume;

pub use sweep::{
    AttenuationConfig, CutDerivationReport, DerivationConfig, DerivationReport,
    DerivedSweepProduct, DiagnosticConfig, KdpConfig, MeteoMaskConfig, QpeConfig, RadarBand,
    TextureConfig, derive_cut_in_place, derive_product, derive_volume_in_place,
};
pub use temporal::{
    accumulate_rate_grids, difference_grid, exceedance_duration_grid, exceedance_probability_grid,
    maximum_swath_grid, mean_grid, minimum_swath_grid, trend_grid,
};
pub use volume::{
    CappiInterpolation, cappi_grid, column_max_grid, column_mean_grid, column_min_grid,
    echo_base_grid, echo_depth_grid, echo_top_height_grid, height_of_max_reflectivity_grid,
    low_level_composite_reflectivity_grid,
};

use radar_core::{MomentType, ProductId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProductDescriptor {
    pub id: ProductId,
    pub display_name: &'static str,
    pub source: ProductSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductSource {
    BaseMoment(MomentType),
    Derived,
}

pub fn base_products() -> Vec<ProductDescriptor> {
    [
        (MomentType::Reflectivity, "Base Reflectivity"),
        (MomentType::Velocity, "Base Velocity"),
        (MomentType::SpectrumWidth, "Spectrum Width"),
        (
            MomentType::DifferentialReflectivity,
            "Differential Reflectivity",
        ),
        (
            MomentType::CorrelationCoefficient,
            "Correlation Coefficient",
        ),
        (MomentType::DifferentialPhase, "Differential Phase"),
        (
            MomentType::SpecificDifferentialPhase,
            "Specific Differential Phase",
        ),
    ]
    .into_iter()
    .map(|(moment, display_name)| ProductDescriptor {
        id: ProductId::from(moment.clone()),
        display_name,
        source: ProductSource::BaseMoment(moment),
    })
    .collect()
}

/// Sweep-local products implemented by this crate.
pub fn derived_products() -> Vec<ProductDescriptor> {
    DerivedSweepProduct::ALL
        .iter()
        .copied()
        .map(|product| ProductDescriptor {
            id: ProductId(product.id().to_owned()),
            display_name: product.display_name(),
            source: ProductSource::Derived,
        })
        .collect()
}

/// Volume products already implemented by BowEcho or provided by [`volume`].
///
/// The first eight IDs match BowEcho's existing `render2d::volumetric` paths;
/// the remaining products are implemented in this crate.
pub fn volume_products() -> Vec<ProductDescriptor> {
    [
        ("CREF", "Composite Reflectivity"),
        ("ET", "Echo Tops"),
        ("VIL", "Vertically Integrated Liquid"),
        ("VILD", "VIL Density"),
        ("SHI", "Severe Hail Index"),
        ("MESH", "Maximum Estimated Size of Hail"),
        ("POSH", "Probability of Severe Hail"),
        ("POH", "Probability of Hail"),
        ("CAPPI", "Constant Altitude PPI"),
        ("LLCREF", "Low-Level Composite Reflectivity"),
        ("EBASE", "Echo Base"),
        ("EDEPTH", "Echo Depth"),
        ("HMAX", "Height of Maximum Reflectivity"),
        ("CMAX", "Column Maximum"),
        ("CMIN", "Column Minimum"),
        ("CMEAN", "Column Mean"),
    ]
    .into_iter()
    .map(|(id, display_name)| ProductDescriptor {
        id: ProductId(id.to_owned()),
        display_name,
        source: ProductSource::Derived,
    })
    .collect()
}

/// Temporal/grid-combination products implemented by [`temporal`].
pub fn temporal_products() -> Vec<ProductDescriptor> {
    [
        ("DIFF", "Volume-to-Volume Difference"),
        ("TREND", "Volume-to-Volume Trend"),
        ("SWATH_MAX", "Maximum Swath"),
        ("SWATH_MIN", "Minimum Swath"),
        ("ACCUM", "Rate Accumulation"),
        ("DURATION", "Threshold Exceedance Duration"),
        ("PROB", "Threshold Exceedance Probability"),
    ]
    .into_iter()
    .map(|(id, display_name)| ProductDescriptor {
        id: ProductId(id.to_owned()),
        display_name,
        source: ProductSource::Derived,
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_includes_reflectivity_and_kdp() {
        let base = base_products();
        assert!(base.iter().any(|product| product.id.0 == "REF"));
        assert!(base.iter().any(|product| product.id.0 == "KDP"));
    }

    #[test]
    fn derived_ids_are_unique() {
        let mut ids = derived_products()
            .into_iter()
            .map(|product| product.id.0)
            .collect::<Vec<_>>();
        let original_len = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), original_len);
    }

    #[test]
    fn registry_includes_specific_differential_phase() {
        assert!(base_products().iter().any(|product| product.id.0 == "KDP"));
    }
}
