//! Sweep-local radar product derivation.
//!
//! `KDP` is the only derived product inserted as a first-class known
//! [`MomentType`]. Additional products use `MomentType::Unknown` with stable
//! short IDs so this patch does not force exhaustive-match changes through the
//! renderer, inspector, serialization, and color-table crates. BowEcho can
//! promote those IDs to dedicated enum variants later without changing the
//! algorithms here.

use std::collections::{BTreeMap, BTreeSet};

use radar_core::{ElevationCut, MomentGrid, MomentStorage, MomentType, RadarVolume};

/// Products that can be computed independently for each elevation cut.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DerivedSweepProduct {
    Kdp,
    FilteredDifferentialPhase,
    KdpUncertainty,
    SpecificAttenuation,
    PathIntegratedAttenuation,
    CorrectedReflectivity,
    SpecificDifferentialAttenuation,
    PathIntegratedDifferentialAttenuation,
    CorrectedDifferentialReflectivity,
    RainRateReflectivity,
    RainRateKdp,
    RainRateHybrid,
    LiquidWaterContent,
    HailKineticEnergy,
    CircularDepolarizationRatio,
    LogCorrelationRatio,
    ReflectivityTexture,
    VelocityTexture,
    SpectrumWidthTexture,
    DifferentialReflectivityTexture,
    CorrelationCoefficientTexture,
    DifferentialPhaseTexture,
    KdpTexture,
    ReflectivityRangeGradient,
    VelocityRangeGradient,
    MeteorologicalQuality,
    MeteorologicalGateMask,
    TdsConfidence,
    HailSignature,
    TurbulenceProxy,
}

impl DerivedSweepProduct {
    pub const ALL: &'static [Self] = &[
        Self::Kdp,
        Self::FilteredDifferentialPhase,
        Self::KdpUncertainty,
        Self::SpecificAttenuation,
        Self::PathIntegratedAttenuation,
        Self::CorrectedReflectivity,
        Self::SpecificDifferentialAttenuation,
        Self::PathIntegratedDifferentialAttenuation,
        Self::CorrectedDifferentialReflectivity,
        Self::RainRateReflectivity,
        Self::RainRateKdp,
        Self::RainRateHybrid,
        Self::LiquidWaterContent,
        Self::HailKineticEnergy,
        Self::CircularDepolarizationRatio,
        Self::LogCorrelationRatio,
        Self::ReflectivityTexture,
        Self::VelocityTexture,
        Self::SpectrumWidthTexture,
        Self::DifferentialReflectivityTexture,
        Self::CorrelationCoefficientTexture,
        Self::DifferentialPhaseTexture,
        Self::KdpTexture,
        Self::ReflectivityRangeGradient,
        Self::VelocityRangeGradient,
        Self::MeteorologicalQuality,
        Self::MeteorologicalGateMask,
        Self::TdsConfidence,
        Self::HailSignature,
        Self::TurbulenceProxy,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::Kdp => "KDP",
            Self::FilteredDifferentialPhase => "PHIF",
            Self::KdpUncertainty => "KDP_SD",
            Self::SpecificAttenuation => "AH",
            Self::PathIntegratedAttenuation => "PIA",
            Self::CorrectedReflectivity => "REFC",
            Self::SpecificDifferentialAttenuation => "ADP",
            Self::PathIntegratedDifferentialAttenuation => "PIDA",
            Self::CorrectedDifferentialReflectivity => "ZDRC",
            Self::RainRateReflectivity => "RATE_Z",
            Self::RainRateKdp => "RATE_KDP",
            Self::RainRateHybrid => "RATE",
            Self::LiquidWaterContent => "LWC",
            Self::HailKineticEnergy => "HKE",
            Self::CircularDepolarizationRatio => "CDR",
            Self::LogCorrelationRatio => "L_RHO",
            Self::ReflectivityTexture => "REF_TEX",
            Self::VelocityTexture => "VEL_TEX",
            Self::SpectrumWidthTexture => "SW_TEX",
            Self::DifferentialReflectivityTexture => "ZDR_TEX",
            Self::CorrelationCoefficientTexture => "RHO_TEX",
            Self::DifferentialPhaseTexture => "PHI_TEX",
            Self::KdpTexture => "KDP_TEX",
            Self::ReflectivityRangeGradient => "REF_GRAD_R",
            Self::VelocityRangeGradient => "VEL_GRAD_R",
            Self::MeteorologicalQuality => "MET_QI",
            Self::MeteorologicalGateMask => "MET_MASK",
            Self::TdsConfidence => "TDS_SCORE",
            Self::HailSignature => "HAIL_SCORE",
            Self::TurbulenceProxy => "TURB",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Kdp => "Specific Differential Phase",
            Self::FilteredDifferentialPhase => "Filtered Differential Phase",
            Self::KdpUncertainty => "KDP Uncertainty",
            Self::SpecificAttenuation => "Specific Attenuation",
            Self::PathIntegratedAttenuation => "Path Integrated Attenuation",
            Self::CorrectedReflectivity => "Attenuation-Corrected Reflectivity",
            Self::SpecificDifferentialAttenuation => "Specific Differential Attenuation",
            Self::PathIntegratedDifferentialAttenuation => {
                "Path Integrated Differential Attenuation"
            }
            Self::CorrectedDifferentialReflectivity => {
                "Attenuation-Corrected Differential Reflectivity"
            }
            Self::RainRateReflectivity => "Rain Rate from Reflectivity",
            Self::RainRateKdp => "Rain Rate from KDP",
            Self::RainRateHybrid => "Hybrid Polarimetric Rain Rate",
            Self::LiquidWaterContent => "Radar Liquid-Water-Content Proxy",
            Self::HailKineticEnergy => "Hail Kinetic-Energy Flux",
            Self::CircularDepolarizationRatio => "Circular Depolarization Ratio",
            Self::LogCorrelationRatio => "Logarithmic Correlation Ratio",
            Self::ReflectivityTexture => "Reflectivity Texture",
            Self::VelocityTexture => "Velocity Texture",
            Self::SpectrumWidthTexture => "Spectrum-Width Texture",
            Self::DifferentialReflectivityTexture => "Differential-Reflectivity Texture",
            Self::CorrelationCoefficientTexture => "Correlation-Coefficient Texture",
            Self::DifferentialPhaseTexture => "Differential-Phase Texture",
            Self::KdpTexture => "KDP Texture",
            Self::ReflectivityRangeGradient => "Reflectivity Range Gradient",
            Self::VelocityRangeGradient => "Radial-Velocity Range Gradient",
            Self::MeteorologicalQuality => "Meteorological Gate Quality",
            Self::MeteorologicalGateMask => "Meteorological Gate Mask",
            Self::TdsConfidence => "Tornadic-Debris-Signature Diagnostic",
            Self::HailSignature => "Dual-Pol Hail-Signature Diagnostic",
            Self::TurbulenceProxy => "Doppler Turbulence Proxy",
        }
    }

    pub const fn units(self) -> &'static str {
        match self {
            Self::Kdp => "deg/km",
            Self::FilteredDifferentialPhase => "deg",
            Self::KdpUncertainty => "deg/km",
            Self::SpecificAttenuation => "dB/km",
            Self::PathIntegratedAttenuation => "dB",
            Self::CorrectedReflectivity => "dBZ",
            Self::SpecificDifferentialAttenuation => "dB/km",
            Self::PathIntegratedDifferentialAttenuation => "dB",
            Self::CorrectedDifferentialReflectivity => "dB",
            Self::RainRateReflectivity | Self::RainRateKdp | Self::RainRateHybrid => "mm/h",
            Self::LiquidWaterContent => "g/m^3",
            Self::HailKineticEnergy => "J/(m^2 s)",
            Self::CircularDepolarizationRatio => "dB",
            Self::LogCorrelationRatio => "1",
            Self::ReflectivityTexture => "dB",
            Self::VelocityTexture | Self::SpectrumWidthTexture | Self::TurbulenceProxy => "m/s",
            Self::DifferentialReflectivityTexture => "dB",
            Self::CorrelationCoefficientTexture => "1",
            Self::DifferentialPhaseTexture => "deg",
            Self::KdpTexture => "deg/km",
            Self::ReflectivityRangeGradient => "dBZ/km",
            Self::VelocityRangeGradient => "10^-3/s",
            Self::MeteorologicalQuality | Self::MeteorologicalGateMask => "1",
            Self::TdsConfidence | Self::HailSignature => "%",
        }
    }

    pub fn moment_type(self) -> MomentType {
        if self == Self::Kdp {
            MomentType::SpecificDifferentialPhase
        } else {
            MomentType::Unknown(self.id().to_owned())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadarBand {
    S,
    C,
    X,
}

impl RadarBand {
    fn rain_kdp_coefficients(self) -> (f32, f32) {
        match self {
            Self::S => (50.70, 0.8500),
            Self::C => (29.70, 0.8500),
            Self::X => (15.81, 0.7992),
        }
    }

    fn attenuation_coefficients(self) -> (f32, f32) {
        // PHIDP-linear coefficients used by Py-ART's band table. The first
        // coefficient corrects horizontal reflectivity; the second corrects
        // differential reflectivity.
        match self {
            Self::S => (0.04, 0.004),
            Self::C => (0.08, 0.03),
            Self::X => (0.28, 0.04),
        }
    }

    fn default_kdp_bounds(self) -> (f32, f32) {
        match self {
            Self::S => (-2.0, 14.0),
            Self::C => (-2.0, 20.0),
            Self::X => (-2.0, 40.0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct KdpConfig {
    /// Robust regression window length along a radial.
    pub window_km: f32,
    pub min_window_gates: usize,
    pub max_window_gates: usize,
    pub min_valid_gates: usize,
    pub max_interpolated_gap_gates: usize,
    pub phase_period_deg: f32,
    pub min_rho_hv: f32,
    pub min_reflectivity_dbz: f32,
    pub hampel_half_window: usize,
    pub hampel_sigma: f32,
    pub huber_k: f32,
    pub kdp_min_deg_km: f32,
    pub kdp_max_deg_km: f32,
    /// Emit estimates at short, internally interpolated PHIDP gaps. Keeping
    /// this false is conservative and prevents interpolation from inventing
    /// visible precipitation.
    pub emit_interpolated_gates: bool,
    /// Number of near-range valid gates used to estimate the system-phase
    /// baseline for attenuation products.
    pub phase_baseline_gates: usize,
}

impl KdpConfig {
    pub fn for_band(band: RadarBand) -> Self {
        let (kdp_min_deg_km, kdp_max_deg_km) = band.default_kdp_bounds();
        Self {
            window_km: 3.0,
            min_window_gates: 7,
            max_window_gates: 41,
            min_valid_gates: 5,
            max_interpolated_gap_gates: 2,
            phase_period_deg: 360.0,
            min_rho_hv: 0.80,
            min_reflectivity_dbz: -10.0,
            hampel_half_window: 3,
            hampel_sigma: 3.0,
            huber_k: 1.5,
            kdp_min_deg_km,
            kdp_max_deg_km,
            emit_interpolated_gates: false,
            phase_baseline_gates: 20,
        }
    }
}

#[derive(Clone, Debug)]
pub struct QpeConfig {
    /// Marshall-Palmer/NWS-style Z=a R^b relationship.
    pub z_r_a: f32,
    pub z_r_b: f32,
    pub kdp_alpha: f32,
    pub kdp_beta: f32,
    pub hybrid_min_kdp_deg_km: f32,
    pub hybrid_min_reflectivity_dbz: f32,
    pub hybrid_min_rho_hv: f32,
    pub max_rate_mm_h: f32,
}

impl QpeConfig {
    pub fn for_band(band: RadarBand) -> Self {
        let (kdp_alpha, kdp_beta) = band.rain_kdp_coefficients();
        Self {
            z_r_a: 300.0,
            z_r_b: 1.4,
            kdp_alpha,
            kdp_beta,
            hybrid_min_kdp_deg_km: 0.30,
            hybrid_min_reflectivity_dbz: 35.0,
            hybrid_min_rho_hv: 0.90,
            max_rate_mm_h: 300.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AttenuationConfig {
    pub horizontal_db_per_degree: f32,
    pub differential_db_per_degree: f32,
    pub max_pia_db: f32,
    pub max_pida_db: f32,
}

impl AttenuationConfig {
    pub fn for_band(band: RadarBand) -> Self {
        let (horizontal_db_per_degree, differential_db_per_degree) =
            band.attenuation_coefficients();
        Self {
            horizontal_db_per_degree,
            differential_db_per_degree,
            max_pia_db: 20.0,
            max_pida_db: 10.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextureConfig {
    pub radial_radius: usize,
    pub gate_radius: usize,
    pub min_samples: usize,
}

impl Default for TextureConfig {
    fn default() -> Self {
        Self {
            radial_radius: 1,
            gate_radius: 2,
            min_samples: 5,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MeteoMaskConfig {
    pub minimum_quality: f32,
    pub minimum_reflectivity_dbz: f32,
}

impl Default for MeteoMaskConfig {
    fn default() -> Self {
        Self {
            minimum_quality: 0.50,
            minimum_reflectivity_dbz: -10.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiagnosticConfig {
    pub tds_min_reflectivity_dbz: f32,
    pub tds_rho_ceiling: f32,
    pub hail_min_reflectivity_dbz: f32,
}

impl Default for DiagnosticConfig {
    fn default() -> Self {
        Self {
            tds_min_reflectivity_dbz: 20.0,
            tds_rho_ceiling: 0.95,
            hail_min_reflectivity_dbz: 45.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DerivationConfig {
    pub products: BTreeSet<DerivedSweepProduct>,
    pub overwrite_existing: bool,
    pub band: RadarBand,
    pub kdp: KdpConfig,
    pub qpe: QpeConfig,
    pub attenuation: AttenuationConfig,
    pub texture: TextureConfig,
    pub meteo_mask: MeteoMaskConfig,
    pub diagnostics: DiagnosticConfig,
}

impl DerivationConfig {
    pub fn kdp_only() -> Self {
        Self::with_products(RadarBand::S, [DerivedSweepProduct::Kdp])
    }

    pub fn analyst_defaults() -> Self {
        Self::with_products(
            RadarBand::S,
            [
                DerivedSweepProduct::Kdp,
                DerivedSweepProduct::FilteredDifferentialPhase,
                DerivedSweepProduct::KdpUncertainty,
                DerivedSweepProduct::RainRateHybrid,
                DerivedSweepProduct::MeteorologicalQuality,
            ],
        )
    }

    pub fn all_supported() -> Self {
        Self::with_products(RadarBand::S, DerivedSweepProduct::ALL.iter().copied())
    }

    pub fn with_products(
        band: RadarBand,
        products: impl IntoIterator<Item = DerivedSweepProduct>,
    ) -> Self {
        Self {
            products: products.into_iter().collect(),
            overwrite_existing: false,
            band,
            kdp: KdpConfig::for_band(band),
            qpe: QpeConfig::for_band(band),
            attenuation: AttenuationConfig::for_band(band),
            texture: TextureConfig::default(),
            meteo_mask: MeteoMaskConfig::default(),
            diagnostics: DiagnosticConfig::default(),
        }
    }

    pub fn set_band(&mut self, band: RadarBand) {
        self.band = band;
        self.kdp = KdpConfig::for_band(band);
        self.qpe = QpeConfig::for_band(band);
        self.attenuation = AttenuationConfig::for_band(band);
    }
}

impl Default for DerivationConfig {
    fn default() -> Self {
        Self::analyst_defaults()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CutDerivationReport {
    pub inserted: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub unavailable: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DerivationReport {
    pub cuts_processed: usize,
    pub inserted: Vec<(usize, String)>,
    pub skipped_existing: Vec<(usize, String)>,
    pub unavailable: Vec<(usize, String)>,
}

/// Derive configured products on every elevation cut, preserving native moments
/// unless `overwrite_existing` is enabled.
pub fn derive_volume_in_place(
    volume: &mut RadarVolume,
    config: &DerivationConfig,
) -> DerivationReport {
    let mut report = DerivationReport::default();
    for (cut_index, cut) in volume.cuts.iter_mut().enumerate() {
        let cut_report = derive_cut_in_place(cut, config);
        report.cuts_processed += 1;
        report
            .inserted
            .extend(cut_report.inserted.into_iter().map(|id| (cut_index, id)));
        report.skipped_existing.extend(
            cut_report
                .skipped_existing
                .into_iter()
                .map(|id| (cut_index, id)),
        );
        report
            .unavailable
            .extend(cut_report.unavailable.into_iter().map(|id| (cut_index, id)));
    }
    report
}

/// Derive configured products for one cut and insert them into its moment map.
///
/// The function computes against an immutable snapshot and commits all results
/// at the end, so dependencies do not observe a half-mutated cut.
pub fn derive_cut_in_place(
    cut: &mut ElevationCut,
    config: &DerivationConfig,
) -> CutDerivationReport {
    let mut report = CutDerivationReport::default();
    let requested = &config.products;
    if requested.is_empty() {
        return report;
    }

    let phase_needed = requested.iter().any(|product| {
        matches!(
            product,
            DerivedSweepProduct::Kdp
                | DerivedSweepProduct::FilteredDifferentialPhase
                | DerivedSweepProduct::KdpUncertainty
                | DerivedSweepProduct::SpecificAttenuation
                | DerivedSweepProduct::PathIntegratedAttenuation
                | DerivedSweepProduct::CorrectedReflectivity
                | DerivedSweepProduct::SpecificDifferentialAttenuation
                | DerivedSweepProduct::PathIntegratedDifferentialAttenuation
                | DerivedSweepProduct::CorrectedDifferentialReflectivity
                | DerivedSweepProduct::RainRateKdp
                | DerivedSweepProduct::RainRateHybrid
                | DerivedSweepProduct::KdpTexture
        )
    });

    let phase_bundle = if phase_needed {
        derive_phase_bundle(cut, &config.kdp)
    } else {
        None
    };

    // Prefer a source-provided KDP field for downstream products. If no native
    // KDP is present, use the robust PHIDP retrieval from this pass.
    let native_kdp = cut
        .moments
        .get(&MomentType::SpecificDifferentialPhase)
        .cloned();
    let derived_kdp = phase_bundle.as_ref().map(|bundle| bundle.kdp.clone());
    let kdp_for_dependencies = native_kdp.as_ref().or(derived_kdp.as_ref());

    let phase_excess = phase_bundle
        .as_ref()
        .map(|bundle| phase_excess_grid(&bundle.filtered_phi, config.kdp.phase_baseline_gates));

    let pia = if needs_any(
        requested,
        &[
            DerivedSweepProduct::PathIntegratedAttenuation,
            DerivedSweepProduct::CorrectedReflectivity,
        ],
    ) {
        phase_excess.as_ref().map_or_else(
            || {
                kdp_for_dependencies.map(|kdp| {
                    integrate_kdp(
                        kdp,
                        DerivedSweepProduct::PathIntegratedAttenuation.moment_type(),
                        config.attenuation.horizontal_db_per_degree,
                        config.attenuation.max_pia_db,
                    )
                })
            },
            |phase| {
                Some(scale_positive_grid(
                    phase,
                    DerivedSweepProduct::PathIntegratedAttenuation.moment_type(),
                    config.attenuation.horizontal_db_per_degree,
                    config.attenuation.max_pia_db,
                ))
            },
        )
    } else {
        None
    };

    let pida = if needs_any(
        requested,
        &[
            DerivedSweepProduct::PathIntegratedDifferentialAttenuation,
            DerivedSweepProduct::CorrectedDifferentialReflectivity,
        ],
    ) {
        phase_excess.as_ref().map_or_else(
            || {
                kdp_for_dependencies.map(|kdp| {
                    integrate_kdp(
                        kdp,
                        DerivedSweepProduct::PathIntegratedDifferentialAttenuation.moment_type(),
                        config.attenuation.differential_db_per_degree,
                        config.attenuation.max_pida_db,
                    )
                })
            },
            |phase| {
                Some(scale_positive_grid(
                    phase,
                    DerivedSweepProduct::PathIntegratedDifferentialAttenuation.moment_type(),
                    config.attenuation.differential_db_per_degree,
                    config.attenuation.max_pida_db,
                ))
            },
        )
    } else {
        None
    };

    let mut pending = BTreeMap::<MomentType, MomentGrid>::new();

    for &product in DerivedSweepProduct::ALL {
        if !requested.contains(&product) {
            continue;
        }
        let output_moment = product.moment_type();
        if cut.moments.contains_key(&output_moment) && !config.overwrite_existing {
            report.skipped_existing.push(product.id().to_owned());
            continue;
        }

        let grid = match product {
            DerivedSweepProduct::Kdp => phase_bundle.as_ref().map(|bundle| bundle.kdp.clone()),
            DerivedSweepProduct::FilteredDifferentialPhase => phase_bundle
                .as_ref()
                .map(|bundle| bundle.filtered_phi.clone()),
            DerivedSweepProduct::KdpUncertainty => phase_bundle
                .as_ref()
                .map(|bundle| bundle.uncertainty.clone()),
            DerivedSweepProduct::SpecificAttenuation => kdp_for_dependencies.map(|kdp| {
                scale_positive_grid(
                    kdp,
                    output_moment.clone(),
                    config.attenuation.horizontal_db_per_degree,
                    f32::INFINITY,
                )
            }),
            DerivedSweepProduct::PathIntegratedAttenuation => pia.clone(),
            DerivedSweepProduct::CorrectedReflectivity => cut
                .moments
                .get(&MomentType::Reflectivity)
                .and_then(|reflectivity| {
                    pia.as_ref().map(|attenuation| {
                        add_aligned_grid(reflectivity, attenuation, output_moment.clone(), 1.0, 1.0)
                    })
                }),
            DerivedSweepProduct::SpecificDifferentialAttenuation => {
                kdp_for_dependencies.map(|kdp| {
                    scale_positive_grid(
                        kdp,
                        output_moment.clone(),
                        config.attenuation.differential_db_per_degree,
                        f32::INFINITY,
                    )
                })
            }
            DerivedSweepProduct::PathIntegratedDifferentialAttenuation => pida.clone(),
            DerivedSweepProduct::CorrectedDifferentialReflectivity => cut
                .moments
                .get(&MomentType::DifferentialReflectivity)
                .and_then(|zdr| {
                    pida.as_ref().map(|attenuation| {
                        add_aligned_grid(zdr, attenuation, output_moment.clone(), 1.0, 1.0)
                    })
                }),
            DerivedSweepProduct::RainRateReflectivity => cut
                .moments
                .get(&MomentType::Reflectivity)
                .map(|reflectivity| {
                    rain_rate_z_grid(reflectivity, output_moment.clone(), &config.qpe)
                }),
            DerivedSweepProduct::RainRateKdp => kdp_for_dependencies
                .map(|kdp| rain_rate_kdp_grid(kdp, output_moment.clone(), &config.qpe)),
            DerivedSweepProduct::RainRateHybrid => hybrid_rain_rate_grid(
                cut,
                kdp_for_dependencies,
                output_moment.clone(),
                &config.qpe,
            ),
            DerivedSweepProduct::LiquidWaterContent => cut
                .moments
                .get(&MomentType::Reflectivity)
                .map(|reflectivity| liquid_water_content_grid(reflectivity, output_moment.clone())),
            DerivedSweepProduct::HailKineticEnergy => cut
                .moments
                .get(&MomentType::Reflectivity)
                .map(|reflectivity| hail_kinetic_energy_grid(reflectivity, output_moment.clone())),
            DerivedSweepProduct::CircularDepolarizationRatio => {
                circular_depolarization_ratio_grid(cut, output_moment.clone())
            }
            DerivedSweepProduct::LogCorrelationRatio => cut
                .moments
                .get(&MomentType::CorrelationCoefficient)
                .map(|rho| log_correlation_ratio_grid(rho, output_moment.clone())),
            DerivedSweepProduct::ReflectivityTexture => {
                cut.moments.get(&MomentType::Reflectivity).map(|source| {
                    texture_grid(
                        cut,
                        source,
                        output_moment.clone(),
                        &config.texture,
                        TexturePeriod::Linear,
                    )
                })
            }
            DerivedSweepProduct::VelocityTexture => {
                cut.moments.get(&MomentType::Velocity).map(|source| {
                    texture_grid(
                        cut,
                        source,
                        output_moment.clone(),
                        &config.texture,
                        TexturePeriod::NyquistVelocity,
                    )
                })
            }
            DerivedSweepProduct::SpectrumWidthTexture => {
                cut.moments.get(&MomentType::SpectrumWidth).map(|source| {
                    texture_grid(
                        cut,
                        source,
                        output_moment.clone(),
                        &config.texture,
                        TexturePeriod::Linear,
                    )
                })
            }
            DerivedSweepProduct::DifferentialReflectivityTexture => cut
                .moments
                .get(&MomentType::DifferentialReflectivity)
                .map(|source| {
                    texture_grid(
                        cut,
                        source,
                        output_moment.clone(),
                        &config.texture,
                        TexturePeriod::Linear,
                    )
                }),
            DerivedSweepProduct::CorrelationCoefficientTexture => cut
                .moments
                .get(&MomentType::CorrelationCoefficient)
                .map(|source| {
                    texture_grid(
                        cut,
                        source,
                        output_moment.clone(),
                        &config.texture,
                        TexturePeriod::Linear,
                    )
                }),
            DerivedSweepProduct::DifferentialPhaseTexture => cut
                .moments
                .get(&MomentType::DifferentialPhase)
                .map(|source| {
                    texture_grid(
                        cut,
                        source,
                        output_moment.clone(),
                        &config.texture,
                        TexturePeriod::Fixed(config.kdp.phase_period_deg),
                    )
                }),
            DerivedSweepProduct::KdpTexture => kdp_for_dependencies.map(|source| {
                texture_grid(
                    cut,
                    source,
                    output_moment.clone(),
                    &config.texture,
                    TexturePeriod::Linear,
                )
            }),
            DerivedSweepProduct::ReflectivityRangeGradient => cut
                .moments
                .get(&MomentType::Reflectivity)
                .map(|source| range_gradient_grid(source, output_moment.clone())),
            DerivedSweepProduct::VelocityRangeGradient => cut
                .moments
                .get(&MomentType::Velocity)
                .map(|source| velocity_range_gradient_grid(cut, source, output_moment.clone())),
            DerivedSweepProduct::MeteorologicalQuality => {
                meteorological_quality_grid(cut, output_moment.clone())
            }
            DerivedSweepProduct::MeteorologicalGateMask => {
                meteorological_mask_grid(cut, output_moment.clone(), &config.meteo_mask)
            }
            DerivedSweepProduct::TdsConfidence => {
                tds_score_grid(cut, output_moment.clone(), &config.diagnostics)
            }
            DerivedSweepProduct::HailSignature => {
                hail_score_grid(cut, output_moment.clone(), &config.diagnostics)
            }
            DerivedSweepProduct::TurbulenceProxy => {
                turbulence_proxy_grid(cut, output_moment.clone(), &config.texture)
            }
        };

        if let Some(grid) = grid {
            pending.insert(output_moment, grid);
            report.inserted.push(product.id().to_owned());
        } else {
            report.unavailable.push(product.id().to_owned());
        }
    }

    for (moment, grid) in pending {
        cut.moments.insert(moment, grid);
    }
    report
}

/// Derive one product without mutating the caller's cut.
pub fn derive_product(
    cut: &ElevationCut,
    product: DerivedSweepProduct,
    config: &DerivationConfig,
) -> Option<MomentGrid> {
    let mut copy = cut.clone();
    let mut one = config.clone();
    one.products.clear();
    one.products.insert(product);
    // A caller asking for a derived product expects a newly computed value,
    // even if a field with the same ID is already present in the snapshot.
    one.overwrite_existing = true;
    derive_cut_in_place(&mut copy, &one);
    copy.moments.get(&product.moment_type()).cloned()
}

fn needs_any(requested: &BTreeSet<DerivedSweepProduct>, products: &[DerivedSweepProduct]) -> bool {
    products.iter().any(|product| requested.contains(product))
}

#[derive(Clone)]
struct PhaseBundle {
    filtered_phi: MomentGrid,
    kdp: MomentGrid,
    uncertainty: MomentGrid,
}

fn derive_phase_bundle(cut: &ElevationCut, config: &KdpConfig) -> Option<PhaseBundle> {
    let phi = cut.moments.get(&MomentType::DifferentialPhase)?;
    let rho = cut
        .moments
        .get(&MomentType::CorrelationCoefficient)
        .map(GridSampler::new);
    let reflectivity = cut
        .moments
        .get(&MomentType::Reflectivity)
        .map(GridSampler::new);

    let rows = phi.radial_count();
    let gates = phi.gate_range.gate_count;
    if rows == 0 || gates == 0 || phi.gate_range.gate_spacing_m <= 0 {
        return None;
    }

    let mut filtered_values = vec![f32::NAN; rows * gates];
    let mut kdp_values = vec![f32::NAN; rows * gates];
    let mut uncertainty_values = vec![f32::NAN; rows * gates];
    let spacing_km = phi.gate_range.gate_spacing_m as f64 / 1000.0;
    let window_gates = regression_window_gates(config, spacing_km as f32);

    for row in 0..rows {
        let radial_index = match phi.radial_indices.get(row) {
            Some(index) => *index,
            None => continue,
        };
        let mut values = vec![f32::NAN; gates];
        let mut original_valid = vec![false; gates];

        for gate in 0..gates {
            let Some(phase) = phi.scaled_value(row, gate) else {
                continue;
            };
            if !phase.is_finite() {
                continue;
            }
            let range_m = gate_center_m(phi, gate);
            if let Some(rho_sampler) = rho.as_ref()
                && let Some(rho_hv) = rho_sampler.sample(radial_index, range_m)
                && (!rho_hv.is_finite() || rho_hv < config.min_rho_hv)
            {
                continue;
            }
            if let Some(ref_sampler) = reflectivity.as_ref()
                && let Some(dbz) = ref_sampler.sample(radial_index, range_m)
                && (!dbz.is_finite() || dbz < config.min_reflectivity_dbz)
            {
                continue;
            }
            values[gate] = phase;
            original_valid[gate] = true;
        }

        unwrap_phase_in_place(
            &mut values,
            config.phase_period_deg,
            config.max_interpolated_gap_gates,
        );
        fill_short_gaps_in_place(&mut values, config.max_interpolated_gap_gates);
        let values = hampel_filter(&values, config.hampel_half_window, config.hampel_sigma);

        let mut xs = Vec::with_capacity(window_gates);
        let mut ys = Vec::with_capacity(window_gates);
        let mut fit_scratch = FitScratch::default();
        for (gate, originally_valid) in original_valid.iter().copied().enumerate().take(gates) {
            if !originally_valid && !config.emit_interpolated_gates {
                continue;
            }
            let half = window_gates / 2;
            let start = gate.saturating_sub(half);
            let end = (gate + half + 1).min(gates);
            xs.clear();
            ys.clear();
            for (sample_gate, value) in values.iter().copied().enumerate().take(end).skip(start) {
                if value.is_finite() {
                    xs.push((sample_gate as f64 - gate as f64) * spacing_km);
                    ys.push(value as f64);
                }
            }
            if xs.len() < config.min_valid_gates {
                continue;
            }
            let Some(fit) = robust_linear_fit(&xs, &ys, config.huber_k as f64, &mut fit_scratch)
            else {
                continue;
            };
            let index = row * gates + gate;
            filtered_values[index] = fit.intercept as f32;
            let kdp = (0.5 * fit.slope) as f32;
            if kdp.is_finite() && kdp >= config.kdp_min_deg_km && kdp <= config.kdp_max_deg_km {
                kdp_values[index] = kdp;
                uncertainty_values[index] = (0.5 * fit.slope_standard_error) as f32;
            }
        }
    }

    Some(PhaseBundle {
        filtered_phi: f32_grid_like(
            phi,
            DerivedSweepProduct::FilteredDifferentialPhase.moment_type(),
            filtered_values,
        ),
        kdp: f32_grid_like(phi, MomentType::SpecificDifferentialPhase, kdp_values),
        uncertainty: f32_grid_like(
            phi,
            DerivedSweepProduct::KdpUncertainty.moment_type(),
            uncertainty_values,
        ),
    })
}

fn regression_window_gates(config: &KdpConfig, spacing_km: f32) -> usize {
    let minimum = config.min_window_gates.max(3);
    let maximum = config.max_window_gates.max(minimum);
    if !spacing_km.is_finite() || spacing_km <= 0.0 {
        return minimum | 1;
    }
    let mut gates = (config.window_km.max(spacing_km) / spacing_km).round() as usize;
    gates = gates.clamp(minimum, maximum);
    if gates.is_multiple_of(2) {
        gates = if gates < maximum {
            gates + 1
        } else {
            gates.saturating_sub(1).max(3)
        };
    }
    gates
}

fn unwrap_phase_in_place(values: &mut [f32], period_deg: f32, max_gap: usize) {
    if !period_deg.is_finite() || period_deg <= 0.0 {
        return;
    }
    let mut previous = None::<f32>;
    let mut gap = 0usize;
    for value in values.iter_mut() {
        if !value.is_finite() {
            gap = gap.saturating_add(1);
            continue;
        }
        if gap > max_gap {
            previous = None;
        }
        if let Some(last) = previous {
            let wraps = ((*value - last) / period_deg).round();
            *value -= wraps * period_deg;
        }
        previous = Some(*value);
        gap = 0;
    }
}

fn fill_short_gaps_in_place(values: &mut [f32], max_gap: usize) {
    if max_gap == 0 || values.len() < 3 {
        return;
    }
    let mut index = 0;
    while index < values.len() {
        if values[index].is_finite() {
            index += 1;
            continue;
        }
        let start = index;
        while index < values.len() && !values[index].is_finite() {
            index += 1;
        }
        let gap_len = index - start;
        if start == 0 || index >= values.len() || gap_len > max_gap {
            continue;
        }
        let left = values[start - 1];
        let right = values[index];
        if !left.is_finite() || !right.is_finite() {
            continue;
        }
        for offset in 0..gap_len {
            let fraction = (offset + 1) as f32 / (gap_len + 1) as f32;
            values[start + offset] = left + fraction * (right - left);
        }
    }
}

fn hampel_filter(values: &[f32], half_window: usize, sigma_threshold: f32) -> Vec<f32> {
    if half_window == 0 || values.is_empty() {
        return values.to_vec();
    }
    let mut filtered = values.to_vec();
    let mut neighborhood = Vec::with_capacity(half_window * 2 + 1);
    let mut deviations = Vec::with_capacity(half_window * 2 + 1);
    for index in 0..values.len() {
        let value = values[index];
        if !value.is_finite() {
            continue;
        }
        let start = index.saturating_sub(half_window);
        let end = (index + half_window + 1).min(values.len());
        neighborhood.clear();
        neighborhood.extend(
            values[start..end]
                .iter()
                .copied()
                .filter(|sample| sample.is_finite()),
        );
        let Some(median) = median_f32_mut(&mut neighborhood) else {
            continue;
        };
        deviations.clear();
        deviations.extend(neighborhood.iter().map(|sample| (sample - median).abs()));
        let Some(mad) = median_f32_mut(&mut deviations) else {
            continue;
        };
        let robust_sigma = 1.4826 * mad;
        let outlier = if robust_sigma > 1.0e-4 {
            (value - median).abs() > sigma_threshold.max(0.0) * robust_sigma
        } else {
            (value - median).abs() > sigma_threshold.max(1.0)
        };
        if outlier {
            filtered[index] = median;
        }
    }
    filtered
}

#[derive(Clone, Copy, Debug)]
struct LinearFit {
    slope: f64,
    intercept: f64,
    slope_standard_error: f64,
}

#[derive(Default)]
struct FitScratch {
    weights: Vec<f64>,
    residuals: Vec<f64>,
    ordered: Vec<f64>,
    deviations: Vec<f64>,
}

fn robust_linear_fit(
    xs: &[f64],
    ys: &[f64],
    huber_k: f64,
    scratch: &mut FitScratch,
) -> Option<LinearFit> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    scratch.weights.clear();
    scratch.weights.resize(xs.len(), 1.0);
    let mut slope = 0.0;
    let mut intercept = 0.0;

    for _ in 0..3 {
        (slope, intercept) = weighted_linear_fit(xs, ys, &scratch.weights)?;
        scratch.residuals.clear();
        scratch
            .residuals
            .extend(xs.iter().zip(ys).map(|(x, y)| y - (intercept + slope * x)));
        scratch.ordered.clear();
        scratch.ordered.extend_from_slice(&scratch.residuals);
        let median_residual = median_f64_mut(&mut scratch.ordered)?;
        scratch.deviations.clear();
        scratch.deviations.extend(
            scratch
                .residuals
                .iter()
                .map(|residual| (residual - median_residual).abs()),
        );
        let sigma = 1.4826 * median_f64_mut(&mut scratch.deviations)?;
        if !sigma.is_finite() || sigma <= 1.0e-8 {
            break;
        }
        let cutoff = huber_k.max(0.1) * sigma;
        for (weight, residual) in scratch.weights.iter_mut().zip(&scratch.residuals) {
            let magnitude = residual.abs();
            *weight = if magnitude <= cutoff {
                1.0
            } else {
                cutoff / magnitude
            };
        }
    }

    let weight_sum = scratch.weights.iter().sum::<f64>();
    let x_mean = xs
        .iter()
        .zip(&scratch.weights)
        .map(|(x, weight)| x * weight)
        .sum::<f64>()
        / weight_sum;
    let centered_x_sum = xs
        .iter()
        .zip(&scratch.weights)
        .map(|(x, weight)| weight * (x - x_mean).powi(2))
        .sum::<f64>();
    let residual_sum = xs
        .iter()
        .zip(ys)
        .zip(&scratch.weights)
        .map(|((x, y), weight)| weight * (y - (intercept + slope * x)).powi(2))
        .sum::<f64>();
    let degrees_of_freedom = (xs.len() as f64 - 2.0).max(1.0);
    let variance = residual_sum / degrees_of_freedom;
    let slope_standard_error = if centered_x_sum > 1.0e-12 {
        (variance / centered_x_sum).max(0.0).sqrt()
    } else {
        f64::NAN
    };

    Some(LinearFit {
        slope,
        intercept,
        slope_standard_error,
    })
}

fn weighted_linear_fit(xs: &[f64], ys: &[f64], weights: &[f64]) -> Option<(f64, f64)> {
    let sw = weights.iter().sum::<f64>();
    let sx = xs
        .iter()
        .zip(weights)
        .map(|(x, weight)| x * weight)
        .sum::<f64>();
    let sy = ys
        .iter()
        .zip(weights)
        .map(|(y, weight)| y * weight)
        .sum::<f64>();
    let sxx = xs
        .iter()
        .zip(weights)
        .map(|(x, weight)| x * x * weight)
        .sum::<f64>();
    let sxy = xs
        .iter()
        .zip(ys)
        .zip(weights)
        .map(|((x, y), weight)| x * y * weight)
        .sum::<f64>();
    let denominator = sw * sxx - sx * sx;
    if !denominator.is_finite() || denominator.abs() <= 1.0e-12 || sw <= 0.0 {
        return None;
    }
    let slope = (sw * sxy - sx * sy) / denominator;
    let intercept = (sy - slope * sx) / sw;
    (slope.is_finite() && intercept.is_finite()).then_some((slope, intercept))
}

fn median_f32_mut(values: &mut [f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some(0.5 * (values[middle - 1] + values[middle]))
    } else {
        Some(values[middle])
    }
}

fn median_f64_mut(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        Some(0.5 * (values[middle - 1] + values[middle]))
    } else {
        Some(values[middle])
    }
}

fn median_f32(values: &[f32]) -> Option<f32> {
    let mut sorted = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(|a, b| a.total_cmp(b));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some(0.5 * (sorted[middle - 1] + sorted[middle]))
    } else {
        Some(sorted[middle])
    }
}

fn f32_grid_like(base: &MomentGrid, moment: MomentType, values: Vec<f32>) -> MomentGrid {
    debug_assert_eq!(
        values.len(),
        base.radial_count() * base.gate_range.gate_count
    );
    MomentGrid {
        moment,
        gate_range: base.gate_range.clone(),
        scale: 1.0,
        offset: 0.0,
        nodata: None,
        range_folded: None,
        radial_indices: base.radial_indices.clone(),
        storage: MomentStorage::F32(values),
    }
}

fn gate_center_m(grid: &MomentGrid, gate: usize) -> f64 {
    grid.gate_range.first_gate_m as f64 + gate as f64 * grid.gate_range.gate_spacing_m as f64
}

struct GridSampler<'a> {
    grid: &'a MomentGrid,
    row_by_radial: BTreeMap<usize, usize>,
}

impl<'a> GridSampler<'a> {
    fn new(grid: &'a MomentGrid) -> Self {
        let row_by_radial = grid
            .radial_indices
            .iter()
            .copied()
            .enumerate()
            .map(|(row, radial)| (radial, row))
            .collect();
        Self {
            grid,
            row_by_radial,
        }
    }

    fn sample(&self, radial_index: usize, range_m: f64) -> Option<f32> {
        let row = *self.row_by_radial.get(&radial_index)?;
        let spacing = self.grid.gate_range.gate_spacing_m as f64;
        if !spacing.is_finite() || spacing <= 0.0 {
            return None;
        }
        let gate_position = (range_m - self.grid.gate_range.first_gate_m as f64) / spacing;
        if !gate_position.is_finite() {
            return None;
        }
        let rounded = gate_position.round();
        if rounded < 0.0 || rounded >= self.grid.gate_range.gate_count as f64 {
            return None;
        }
        if (gate_position - rounded).abs() > 0.55 {
            return None;
        }
        self.grid.scaled_value(row, rounded as usize)
    }
}

fn phase_excess_grid(filtered_phi: &MomentGrid, baseline_gates: usize) -> MomentGrid {
    let rows = filtered_phi.radial_count();
    let gates = filtered_phi.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let baseline_candidates = (0..gates.min(baseline_gates.max(1)))
            .filter_map(|gate| filtered_phi.scaled_value(row, gate))
            .filter(|value| value.is_finite())
            .collect::<Vec<_>>();
        let Some(baseline) = median_f32(&baseline_candidates) else {
            continue;
        };
        for gate in 0..gates {
            if let Some(value) = filtered_phi.scaled_value(row, gate)
                && value.is_finite()
            {
                out[row * gates + gate] = (value - baseline).max(0.0);
            }
        }
    }
    f32_grid_like(
        filtered_phi,
        MomentType::Unknown("PHI_EXCESS".to_owned()),
        out,
    )
}

fn scale_positive_grid(
    source: &MomentGrid,
    moment: MomentType,
    coefficient: f32,
    maximum: f32,
) -> MomentGrid {
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if let Some(value) = source.scaled_value(row, gate)
                && value.is_finite()
            {
                out[row * gates + gate] = (coefficient * value.max(0.0)).min(maximum);
            }
        }
    }
    f32_grid_like(source, moment, out)
}

/// Integrate KDP into two-way path attenuation. Since d(PHIDP)/dr = 2*KDP
/// and PIA = alpha*PHIDP, d(PIA)/dr = 2*alpha*KDP.
fn integrate_kdp(
    kdp: &MomentGrid,
    moment: MomentType,
    coefficient: f32,
    maximum: f32,
) -> MomentGrid {
    let rows = kdp.radial_count();
    let gates = kdp.gate_range.gate_count;
    let dr_km = kdp.gate_range.gate_spacing_m.max(0) as f32 / 1000.0;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let mut integrated = 0.0f32;
        let mut previous = None::<f32>;
        for gate in 0..gates {
            let current = kdp
                .scaled_value(row, gate)
                .filter(|value| value.is_finite())
                .map(|value| value.max(0.0));
            if let Some(current) = current {
                let representative = previous.map_or(current, |last| 0.5 * (last + current));
                integrated += 2.0 * coefficient * representative * dr_km;
                out[row * gates + gate] = integrated.min(maximum);
                previous = Some(current);
            } else {
                previous = None;
            }
        }
    }
    f32_grid_like(kdp, moment, out)
}

fn add_aligned_grid(
    base: &MomentGrid,
    other: &MomentGrid,
    moment: MomentType,
    base_factor: f32,
    other_factor: f32,
) -> MomentGrid {
    let other = GridSampler::new(other);
    let rows = base.radial_count();
    let gates = base.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let Some(&radial_index) = base.radial_indices.get(row) else {
            continue;
        };
        for gate in 0..gates {
            let Some(base_value) = base.scaled_value(row, gate) else {
                continue;
            };
            let Some(other_value) = other.sample(radial_index, gate_center_m(base, gate)) else {
                continue;
            };
            if base_value.is_finite() && other_value.is_finite() {
                out[row * gates + gate] = base_factor * base_value + other_factor * other_value;
            }
        }
    }
    f32_grid_like(base, moment, out)
}

fn rain_rate_z_grid(
    reflectivity: &MomentGrid,
    moment: MomentType,
    config: &QpeConfig,
) -> MomentGrid {
    let rows = reflectivity.radial_count();
    let gates = reflectivity.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            let Some(dbz) = reflectivity.scaled_value(row, gate) else {
                continue;
            };
            if !dbz.is_finite() {
                continue;
            }
            let z_linear = 10.0f32.powf(0.1 * dbz);
            let rate = (z_linear / config.z_r_a.max(1.0e-6))
                .max(0.0)
                .powf(1.0 / config.z_r_b.max(1.0e-6));
            out[row * gates + gate] = rate.min(config.max_rate_mm_h);
        }
    }
    f32_grid_like(reflectivity, moment, out)
}

fn rain_rate_kdp_grid(kdp: &MomentGrid, moment: MomentType, config: &QpeConfig) -> MomentGrid {
    let rows = kdp.radial_count();
    let gates = kdp.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            let Some(value) = kdp.scaled_value(row, gate) else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            let rate = config.kdp_alpha * value.max(0.0).powf(config.kdp_beta);
            out[row * gates + gate] = rate.min(config.max_rate_mm_h);
        }
    }
    f32_grid_like(kdp, moment, out)
}

fn hybrid_rain_rate_grid(
    cut: &ElevationCut,
    kdp: Option<&MomentGrid>,
    moment: MomentType,
    config: &QpeConfig,
) -> Option<MomentGrid> {
    let reflectivity = cut.moments.get(&MomentType::Reflectivity);
    match (reflectivity, kdp) {
        (None, None) => None,
        (None, Some(kdp)) => Some(rain_rate_kdp_grid(kdp, moment, config)),
        (Some(reflectivity), None) => Some(rain_rate_z_grid(reflectivity, moment, config)),
        (Some(reflectivity), Some(kdp)) => {
            let kdp_sampler = GridSampler::new(kdp);
            let rho_sampler = cut
                .moments
                .get(&MomentType::CorrelationCoefficient)
                .map(GridSampler::new);
            let rows = reflectivity.radial_count();
            let gates = reflectivity.gate_range.gate_count;
            let mut out = vec![f32::NAN; rows * gates];
            for row in 0..rows {
                let Some(&radial_index) = reflectivity.radial_indices.get(row) else {
                    continue;
                };
                for gate in 0..gates {
                    let Some(dbz) = reflectivity.scaled_value(row, gate) else {
                        continue;
                    };
                    if !dbz.is_finite() {
                        continue;
                    }
                    let range_m = gate_center_m(reflectivity, gate);
                    let kdp_value = kdp_sampler.sample(radial_index, range_m);
                    let rho_value = rho_sampler
                        .as_ref()
                        .and_then(|sampler| sampler.sample(radial_index, range_m));
                    let use_kdp = kdp_value.is_some_and(|value| {
                        value.is_finite()
                            && value >= config.hybrid_min_kdp_deg_km
                            && dbz >= config.hybrid_min_reflectivity_dbz
                            && rho_value.is_none_or(|rho| {
                                rho.is_finite() && rho >= config.hybrid_min_rho_hv
                            })
                    });
                    let rate = if use_kdp {
                        config.kdp_alpha
                            * kdp_value.unwrap_or_default().max(0.0).powf(config.kdp_beta)
                    } else {
                        let z_linear = 10.0f32.powf(0.1 * dbz);
                        (z_linear / config.z_r_a.max(1.0e-6))
                            .max(0.0)
                            .powf(1.0 / config.z_r_b.max(1.0e-6))
                    };
                    out[row * gates + gate] = rate.min(config.max_rate_mm_h);
                }
            }
            Some(f32_grid_like(reflectivity, moment, out))
        }
    }
}

fn liquid_water_content_grid(reflectivity: &MomentGrid, moment: MomentType) -> MomentGrid {
    let rows = reflectivity.radial_count();
    let gates = reflectivity.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if let Some(dbz) = reflectivity.scaled_value(row, gate)
                && dbz.is_finite()
            {
                let z_linear = 10.0f64.powf(dbz.min(56.0) as f64 / 10.0);
                // Greene-Clark VIL integrand, converted kg/m^3 -> g/m^3.
                out[row * gates + gate] = (1000.0 * 3.44e-6 * z_linear.powf(4.0 / 7.0)) as f32;
            }
        }
    }
    f32_grid_like(reflectivity, moment, out)
}

fn hail_kinetic_energy_grid(reflectivity: &MomentGrid, moment: MomentType) -> MomentGrid {
    let rows = reflectivity.radial_count();
    let gates = reflectivity.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if let Some(dbz) = reflectivity.scaled_value(row, gate)
                && dbz.is_finite()
            {
                let weight = ((dbz - 40.0) / 10.0).clamp(0.0, 1.0);
                out[row * gates + gate] = if weight <= 0.0 {
                    0.0
                } else {
                    5.0e-6 * 10.0f32.powf(0.084 * dbz) * weight
                };
            }
        }
    }
    f32_grid_like(reflectivity, moment, out)
}

fn circular_depolarization_ratio_grid(
    cut: &ElevationCut,
    moment: MomentType,
) -> Option<MomentGrid> {
    let zdr = cut.moments.get(&MomentType::DifferentialReflectivity)?;
    let rho = GridSampler::new(cut.moments.get(&MomentType::CorrelationCoefficient)?);
    let rows = zdr.radial_count();
    let gates = zdr.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let Some(&radial_index) = zdr.radial_indices.get(row) else {
            continue;
        };
        for gate in 0..gates {
            let Some(zdr_db) = zdr.scaled_value(row, gate) else {
                continue;
            };
            let Some(rho_hv) = rho.sample(radial_index, gate_center_m(zdr, gate)) else {
                continue;
            };
            if !zdr_db.is_finite() || !rho_hv.is_finite() {
                continue;
            }
            let zdr_linear = 10.0f32.powf(0.1 * zdr_db).max(1.0e-6);
            let inverse_sqrt = (1.0 / zdr_linear).sqrt();
            let numerator = 1.0 + 1.0 / zdr_linear - 2.0 * rho_hv * inverse_sqrt;
            let denominator = 1.0 + 1.0 / zdr_linear + 2.0 * rho_hv * inverse_sqrt;
            if numerator > 0.0 && denominator > 0.0 {
                out[row * gates + gate] = 10.0 * (numerator / denominator).log10();
            }
        }
    }
    Some(f32_grid_like(zdr, moment, out))
}

fn log_correlation_ratio_grid(rho: &MomentGrid, moment: MomentType) -> MomentGrid {
    let rows = rho.radial_count();
    let gates = rho.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if let Some(value) = rho.scaled_value(row, gate)
                && value.is_finite()
            {
                let bounded = value.clamp(0.0, 0.9999);
                out[row * gates + gate] = -(1.0 - bounded).log10();
            }
        }
    }
    f32_grid_like(rho, moment, out)
}

#[derive(Clone, Copy)]
enum TexturePeriod {
    Linear,
    Fixed(f32),
    NyquistVelocity,
}

struct AzimuthNeighborhood {
    sorted_rows: Vec<usize>,
    rank_by_row: Vec<usize>,
    wraps: bool,
}

impl AzimuthNeighborhood {
    fn new(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        let mut azimuth_rows = grid
            .radial_indices
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(row, radial_index)| {
                cut.radials
                    .get(radial_index)
                    .map(|radial| (radial.azimuth_deg.rem_euclid(360.0), row))
            })
            .collect::<Vec<_>>();
        azimuth_rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let sorted_rows = azimuth_rows.iter().map(|(_, row)| *row).collect::<Vec<_>>();
        let mut rank_by_row = vec![0usize; grid.radial_count()];
        for (rank, row) in sorted_rows.iter().copied().enumerate() {
            if row < rank_by_row.len() {
                rank_by_row[row] = rank;
            }
        }
        let largest_gap = if azimuth_rows.len() >= 2 {
            let mut largest = 0.0f32;
            for pair in azimuth_rows.windows(2) {
                largest = largest.max(pair[1].0 - pair[0].0);
            }
            largest.max(360.0 - azimuth_rows.last().unwrap().0 + azimuth_rows[0].0)
        } else {
            360.0
        };
        Self {
            sorted_rows,
            rank_by_row,
            wraps: azimuth_rows.len() >= 180 && largest_gap <= 10.0,
        }
    }

    fn rows_near(&self, row: usize, radius: usize) -> Vec<usize> {
        if self.sorted_rows.is_empty() || row >= self.rank_by_row.len() {
            return Vec::new();
        }
        let rank = self.rank_by_row[row] as isize;
        let count = self.sorted_rows.len() as isize;
        let mut rows = Vec::with_capacity(radius * 2 + 1);
        for offset in -(radius as isize)..=(radius as isize) {
            let mut neighbor = rank + offset;
            if self.wraps {
                neighbor = neighbor.rem_euclid(count);
            } else if neighbor < 0 || neighbor >= count {
                continue;
            }
            rows.push(self.sorted_rows[neighbor as usize]);
        }
        rows
    }
}

fn texture_grid(
    cut: &ElevationCut,
    source: &MomentGrid,
    moment: MomentType,
    config: &TextureConfig,
    period_policy: TexturePeriod,
) -> MomentGrid {
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let neighborhood = AzimuthNeighborhood::new(cut, source);
    let mut out = vec![f32::NAN; rows * gates];

    for row in 0..rows {
        let neighbor_rows = neighborhood.rows_near(row, config.radial_radius);
        let period = match period_policy {
            TexturePeriod::Linear => None,
            TexturePeriod::Fixed(period) => (period > 0.0 && period.is_finite()).then_some(period),
            TexturePeriod::NyquistVelocity => source
                .radial_indices
                .get(row)
                .and_then(|radial_index| cut.radials.get(*radial_index))
                .and_then(|radial| radial.nyquist_velocity_mps)
                .map(|nyquist| 2.0 * nyquist.abs())
                .filter(|period| *period > 0.0 && period.is_finite()),
        };
        for gate in 0..gates {
            let gate_start = gate.saturating_sub(config.gate_radius);
            let gate_end = (gate + config.gate_radius + 1).min(gates);
            let mut samples = Vec::with_capacity(neighbor_rows.len() * (gate_end - gate_start));
            for &neighbor_row in &neighbor_rows {
                for neighbor_gate in gate_start..gate_end {
                    if let Some(value) = source.scaled_value(neighbor_row, neighbor_gate)
                        && value.is_finite()
                    {
                        samples.push(value);
                    }
                }
            }
            if samples.len() < config.min_samples {
                continue;
            }
            let reference = source
                .scaled_value(row, gate)
                .filter(|value| value.is_finite())
                .unwrap_or(samples[0]);
            if let Some(period) = period {
                for sample in &mut samples {
                    *sample = reference + wrapped_delta(*sample - reference, period);
                }
            }
            let mean = samples.iter().sum::<f32>() / samples.len() as f32;
            let variance = samples
                .iter()
                .map(|sample| (sample - mean).powi(2))
                .sum::<f32>()
                / samples.len() as f32;
            out[row * gates + gate] = variance.max(0.0).sqrt();
        }
    }
    f32_grid_like(source, moment, out)
}

fn wrapped_delta(delta: f32, period: f32) -> f32 {
    (delta + 0.5 * period).rem_euclid(period) - 0.5 * period
}

fn range_gradient_grid(source: &MomentGrid, moment: MomentType) -> MomentGrid {
    range_gradient_grid_with_period(source, moment, |_| None)
}

fn velocity_range_gradient_grid(
    cut: &ElevationCut,
    source: &MomentGrid,
    moment: MomentType,
) -> MomentGrid {
    range_gradient_grid_with_period(source, moment, |row| {
        source
            .radial_indices
            .get(row)
            .and_then(|radial_index| cut.radials.get(*radial_index))
            .and_then(|radial| radial.nyquist_velocity_mps)
            .map(|nyquist| 2.0 * nyquist.abs())
            .filter(|period| *period > 0.0 && period.is_finite())
    })
}

fn range_gradient_grid_with_period<F>(
    source: &MomentGrid,
    moment: MomentType,
    period_for_row: F,
) -> MomentGrid
where
    F: Fn(usize) -> Option<f32>,
{
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let spacing_km = source.gate_range.gate_spacing_m as f32 / 1000.0;
    let mut out = vec![f32::NAN; rows * gates];
    if !spacing_km.is_finite() || spacing_km <= 0.0 {
        return f32_grid_like(source, moment, out);
    }
    for row in 0..rows {
        let period = period_for_row(row);
        for gate in 0..gates {
            let left = (1..=2).find_map(|distance| {
                gate.checked_sub(distance).and_then(|index| {
                    source
                        .scaled_value(row, index)
                        .filter(|value| value.is_finite())
                        .map(|value| (index, value))
                })
            });
            let right = (1..=2).find_map(|distance| {
                let index = gate + distance;
                (index < gates)
                    .then(|| source.scaled_value(row, index))
                    .flatten()
                    .filter(|value| value.is_finite())
                    .map(|value| (index, value))
            });
            let gradient = match (left, right) {
                (Some((left_gate, left_value)), Some((right_gate, right_value))) => Some(
                    range_delta(left_value, right_value, period)
                        / ((right_gate - left_gate) as f32 * spacing_km),
                ),
                (Some((left_gate, left_value)), None) => source
                    .scaled_value(row, gate)
                    .filter(|value| value.is_finite())
                    .map(|center| {
                        range_delta(left_value, center, period)
                            / ((gate - left_gate) as f32 * spacing_km)
                    }),
                (None, Some((right_gate, right_value))) => source
                    .scaled_value(row, gate)
                    .filter(|value| value.is_finite())
                    .map(|center| {
                        range_delta(center, right_value, period)
                            / ((right_gate - gate) as f32 * spacing_km)
                    }),
                (None, None) => None,
            };
            if let Some(gradient) = gradient
                && gradient.is_finite()
            {
                out[row * gates + gate] = gradient;
            }
        }
    }
    f32_grid_like(source, moment, out)
}

fn range_delta(left: f32, right: f32, period: Option<f32>) -> f32 {
    let delta = right - left;
    period
        .map(|period| wrapped_delta(delta, period))
        .unwrap_or(delta)
}

fn meteorological_quality_grid(cut: &ElevationCut, moment: MomentType) -> Option<MomentGrid> {
    let base = cut
        .moments
        .get(&MomentType::CorrelationCoefficient)
        .or_else(|| cut.moments.get(&MomentType::Reflectivity))?;
    let rho = cut
        .moments
        .get(&MomentType::CorrelationCoefficient)
        .map(GridSampler::new);
    let reflectivity = cut
        .moments
        .get(&MomentType::Reflectivity)
        .map(GridSampler::new);
    let rows = base.radial_count();
    let gates = base.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let Some(&radial_index) = base.radial_indices.get(row) else {
            continue;
        };
        for gate in 0..gates {
            let range_m = gate_center_m(base, gate);
            let rho_value = rho
                .as_ref()
                .and_then(|sampler| sampler.sample(radial_index, range_m));
            let dbz = reflectivity
                .as_ref()
                .and_then(|sampler| sampler.sample(radial_index, range_m));
            if rho_value.is_none() && dbz.is_none() {
                continue;
            }
            let rho_score = rho_value
                .filter(|value| value.is_finite())
                .map(|value| ((value - 0.70) / 0.30).clamp(0.0, 1.0));
            let reflectivity_score = dbz
                .filter(|value| value.is_finite())
                .map(|value| ((value + 10.0) / 20.0).clamp(0.0, 1.0));
            out[row * gates + gate] = match (rho_score, reflectivity_score) {
                (Some(rho_score), Some(reflectivity_score)) => {
                    0.75 * rho_score + 0.25 * reflectivity_score
                }
                (Some(score), None) | (None, Some(score)) => score,
                (None, None) => f32::NAN,
            };
        }
    }
    Some(f32_grid_like(base, moment, out))
}

fn meteorological_mask_grid(
    cut: &ElevationCut,
    moment: MomentType,
    config: &MeteoMaskConfig,
) -> Option<MomentGrid> {
    let quality =
        meteorological_quality_grid(cut, MomentType::Unknown("_INTERNAL_MET_QI".to_owned()))?;
    let reflectivity = cut
        .moments
        .get(&MomentType::Reflectivity)
        .map(GridSampler::new);
    let rows = quality.radial_count();
    let gates = quality.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let Some(&radial_index) = quality.radial_indices.get(row) else {
            continue;
        };
        for gate in 0..gates {
            let Some(score) = quality.scaled_value(row, gate) else {
                continue;
            };
            if !score.is_finite() {
                continue;
            }
            let reflectivity_ok = reflectivity.as_ref().is_none_or(|sampler| {
                sampler
                    .sample(radial_index, gate_center_m(&quality, gate))
                    .is_some_and(|dbz| dbz.is_finite() && dbz >= config.minimum_reflectivity_dbz)
            });
            out[row * gates + gate] = if score >= config.minimum_quality && reflectivity_ok {
                1.0
            } else {
                0.0
            };
        }
    }
    Some(f32_grid_like(&quality, moment, out))
}

fn tds_score_grid(
    cut: &ElevationCut,
    moment: MomentType,
    config: &DiagnosticConfig,
) -> Option<MomentGrid> {
    let reflectivity = cut.moments.get(&MomentType::Reflectivity)?;
    let rho = GridSampler::new(cut.moments.get(&MomentType::CorrelationCoefficient)?);
    let zdr = cut
        .moments
        .get(&MomentType::DifferentialReflectivity)
        .map(GridSampler::new);
    let rows = reflectivity.radial_count();
    let gates = reflectivity.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let Some(&radial_index) = reflectivity.radial_indices.get(row) else {
            continue;
        };
        for gate in 0..gates {
            let Some(dbz) = reflectivity.scaled_value(row, gate) else {
                continue;
            };
            let range_m = gate_center_m(reflectivity, gate);
            let Some(rho_hv) = rho.sample(radial_index, range_m) else {
                continue;
            };
            if !dbz.is_finite() || !rho_hv.is_finite() {
                continue;
            }
            if dbz < config.tds_min_reflectivity_dbz || rho_hv > config.tds_rho_ceiling {
                out[row * gates + gate] = 0.0;
                continue;
            }
            let reflectivity_score =
                ((dbz - config.tds_min_reflectivity_dbz) / 30.0).clamp(0.0, 1.0);
            let rho_score = ((config.tds_rho_ceiling - rho_hv) / 0.25).clamp(0.0, 1.0);
            let zdr_score = zdr
                .as_ref()
                .and_then(|sampler| sampler.sample(radial_index, range_m))
                .filter(|value| value.is_finite())
                .map_or(0.5, |value| ((3.0 - value) / 5.0).clamp(0.0, 1.0));
            out[row * gates + gate] =
                100.0 * reflectivity_score * rho_score * (0.5 + 0.5 * zdr_score);
        }
    }
    Some(f32_grid_like(reflectivity, moment, out))
}

fn hail_score_grid(
    cut: &ElevationCut,
    moment: MomentType,
    config: &DiagnosticConfig,
) -> Option<MomentGrid> {
    let reflectivity = cut.moments.get(&MomentType::Reflectivity)?;
    let rho = cut
        .moments
        .get(&MomentType::CorrelationCoefficient)
        .map(GridSampler::new);
    let zdr = cut
        .moments
        .get(&MomentType::DifferentialReflectivity)
        .map(GridSampler::new);
    let rows = reflectivity.radial_count();
    let gates = reflectivity.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let Some(&radial_index) = reflectivity.radial_indices.get(row) else {
            continue;
        };
        for gate in 0..gates {
            let Some(dbz) = reflectivity.scaled_value(row, gate) else {
                continue;
            };
            if !dbz.is_finite() {
                continue;
            }
            if dbz < config.hail_min_reflectivity_dbz {
                out[row * gates + gate] = 0.0;
                continue;
            }
            let range_m = gate_center_m(reflectivity, gate);
            let reflectivity_score =
                ((dbz - config.hail_min_reflectivity_dbz) / 20.0).clamp(0.0, 1.0);
            let zdr_score = zdr
                .as_ref()
                .and_then(|sampler| sampler.sample(radial_index, range_m))
                .filter(|value| value.is_finite())
                .map_or(0.5, |value| ((2.5 - value) / 3.5).clamp(0.0, 1.0));
            let rho_score = rho
                .as_ref()
                .and_then(|sampler| sampler.sample(radial_index, range_m))
                .filter(|value| value.is_finite())
                .map_or(0.5, |value| ((0.99 - value) / 0.12).clamp(0.0, 1.0));
            out[row * gates + gate] =
                100.0 * reflectivity_score * (0.55 + 0.25 * zdr_score + 0.20 * rho_score);
        }
    }
    Some(f32_grid_like(reflectivity, moment, out))
}

fn turbulence_proxy_grid(
    cut: &ElevationCut,
    moment: MomentType,
    texture_config: &TextureConfig,
) -> Option<MomentGrid> {
    let spectrum_width = cut.moments.get(&MomentType::SpectrumWidth);
    let velocity = cut.moments.get(&MomentType::Velocity);
    match (spectrum_width, velocity) {
        (None, None) => None,
        (Some(width), None) => Some(remap_grid(width, moment, |value| value.max(0.0))),
        (None, Some(velocity)) => Some(texture_grid(
            cut,
            velocity,
            moment,
            texture_config,
            TexturePeriod::NyquistVelocity,
        )),
        (Some(width), Some(velocity)) => {
            let velocity_texture = texture_grid(
                cut,
                velocity,
                MomentType::Unknown("_INTERNAL_VEL_TEX".to_owned()),
                texture_config,
                TexturePeriod::NyquistVelocity,
            );
            let velocity_texture = GridSampler::new(&velocity_texture);
            let rows = width.radial_count();
            let gates = width.gate_range.gate_count;
            let mut out = vec![f32::NAN; rows * gates];
            for row in 0..rows {
                let Some(&radial_index) = width.radial_indices.get(row) else {
                    continue;
                };
                for gate in 0..gates {
                    let Some(sw) = width.scaled_value(row, gate) else {
                        continue;
                    };
                    let texture = velocity_texture
                        .sample(radial_index, gate_center_m(width, gate))
                        .filter(|value| value.is_finite())
                        .unwrap_or(0.0);
                    if sw.is_finite() {
                        out[row * gates + gate] = sw.max(0.0).hypot(texture.max(0.0));
                    }
                }
            }
            Some(f32_grid_like(width, moment, out))
        }
    }
}

fn remap_grid(
    source: &MomentGrid,
    moment: MomentType,
    transform: impl Fn(f32) -> f32,
) -> MomentGrid {
    let rows = source.radial_count();
    let gates = source.gate_range.gate_count;
    let mut out = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        for gate in 0..gates {
            if let Some(value) = source.scaled_value(row, gate)
                && value.is_finite()
            {
                out[row * gates + gate] = transform(value);
            }
        }
    }
    f32_grid_like(source, moment, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, Radial};

    fn gate_range(gates: usize, spacing_m: i32) -> GateRange {
        GateRange {
            first_gate_m: 0,
            gate_spacing_m: spacing_m,
            gate_count: gates,
        }
    }

    fn f32_grid(moment: MomentType, values: Vec<Vec<f32>>, spacing_m: i32) -> MomentGrid {
        let rows = values.len();
        let gates = values.first().map_or(0, Vec::len);
        assert!(values.iter().all(|row| row.len() == gates));
        MomentGrid {
            moment,
            gate_range: gate_range(gates, spacing_m),
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..rows).collect(),
            storage: MomentStorage::F32(values.into_iter().flatten().collect()),
        }
    }

    fn cut_with_rows(rows: usize, gates: usize, spacing_m: i32) -> ElevationCut {
        let mut cut = ElevationCut::new(0.5, Some(1));
        for row in 0..rows {
            cut.radials.push(Radial {
                azimuth_deg: row as f32 * 360.0 / rows.max(1) as f32,
                elevation_deg: 0.5,
                time_offset_ms: row as i32,
                gate_range: gate_range(gates, spacing_m),
                nyquist_velocity_mps: Some(25.0),
                radial_status: None,
            });
        }
        cut
    }

    #[test]
    fn unwraps_phase_crossing_zero() {
        let mut values = vec![350.0, 355.0, 2.0, 7.0, f32::NAN, 12.0];
        unwrap_phase_in_place(&mut values, 360.0, 8);
        assert!((values[2] - 362.0).abs() < 1.0e-5);
        assert!((values[3] - 367.0).abs() < 1.0e-5);
        assert!((values[5] - 372.0).abs() < 1.0e-5);
    }

    #[test]
    fn unwrap_does_not_bridge_long_missing_phase_gap() {
        let mut values = vec![350.0, 355.0, f32::NAN, f32::NAN, f32::NAN, 2.0, 7.0];
        unwrap_phase_in_place(&mut values, 360.0, 2);
        assert!((values[0] - 350.0).abs() < 1.0e-5);
        assert!((values[1] - 355.0).abs() < 1.0e-5);
        assert!((values[5] - 2.0).abs() < 1.0e-5);
        assert!((values[6] - 7.0).abs() < 1.0e-5);
    }

    #[test]
    fn linear_wrapped_phi_retrieves_kdp() {
        let gates = 101;
        let spacing_m = 250;
        let expected_kdp = 2.0f32;
        let phi = (0..gates)
            .map(|gate| {
                let range_km = gate as f32 * spacing_m as f32 / 1000.0;
                (350.0 + 2.0 * expected_kdp * range_km).rem_euclid(360.0)
            })
            .collect::<Vec<_>>();
        let mut cut = cut_with_rows(1, gates, spacing_m);
        cut.moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(MomentType::DifferentialPhase, vec![phi], spacing_m),
        );
        cut.moments.insert(
            MomentType::CorrelationCoefficient,
            f32_grid(
                MomentType::CorrelationCoefficient,
                vec![vec![0.99; gates]],
                spacing_m,
            ),
        );
        cut.moments.insert(
            MomentType::Reflectivity,
            f32_grid(MomentType::Reflectivity, vec![vec![40.0; gates]], spacing_m),
        );

        let report = derive_cut_in_place(&mut cut, &DerivationConfig::kdp_only());
        assert_eq!(report.inserted, vec!["KDP"]);
        let kdp = cut
            .moments
            .get(&MomentType::SpecificDifferentialPhase)
            .unwrap();
        for gate in 10..(gates - 10) {
            let value = kdp.scaled_value(0, gate).unwrap();
            assert!((value - expected_kdp).abs() < 0.02, "gate {gate}: {value}");
        }
    }

    #[test]
    fn short_gap_is_used_for_fit_but_not_emitted_by_default() {
        let gates = 41;
        let spacing_m = 250;
        let mut phi = (0..gates)
            .map(|gate| 20.0 + gate as f32)
            .collect::<Vec<_>>();
        phi[20] = f32::NAN;
        phi[21] = f32::NAN;
        let mut cut = cut_with_rows(1, gates, spacing_m);
        cut.moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(MomentType::DifferentialPhase, vec![phi], spacing_m),
        );
        derive_cut_in_place(&mut cut, &DerivationConfig::kdp_only());
        let kdp = cut
            .moments
            .get(&MomentType::SpecificDifferentialPhase)
            .unwrap();
        assert!(kdp.scaled_value(0, 19).unwrap().is_finite());
        assert!(kdp.scaled_value(0, 20).unwrap().is_nan());
        assert!(kdp.scaled_value(0, 21).unwrap().is_nan());
        assert!(kdp.scaled_value(0, 22).unwrap().is_finite());
    }

    #[test]
    fn native_kdp_is_preserved() {
        let gates = 20;
        let spacing_m = 250;
        let mut cut = cut_with_rows(1, gates, spacing_m);
        cut.moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(
                MomentType::DifferentialPhase,
                vec![vec![30.0; gates]],
                spacing_m,
            ),
        );
        cut.moments.insert(
            MomentType::SpecificDifferentialPhase,
            f32_grid(
                MomentType::SpecificDifferentialPhase,
                vec![vec![9.0; gates]],
                spacing_m,
            ),
        );
        let report = derive_cut_in_place(&mut cut, &DerivationConfig::kdp_only());
        assert_eq!(report.skipped_existing, vec!["KDP"]);
        assert_eq!(
            cut.moments[&MomentType::SpecificDifferentialPhase].scaled_value(0, 5),
            Some(9.0)
        );
    }

    #[test]
    fn filtered_phase_survives_when_kdp_is_out_of_bounds() {
        let gates = 101;
        let spacing_m = 250;
        let too_large_kdp = 30.0f32;
        let phi = (0..gates)
            .map(|gate| {
                let range_km = gate as f32 * spacing_m as f32 / 1000.0;
                (40.0 + 2.0 * too_large_kdp * range_km).rem_euclid(360.0)
            })
            .collect::<Vec<_>>();
        let mut cut = cut_with_rows(1, gates, spacing_m);
        cut.moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(MomentType::DifferentialPhase, vec![phi], spacing_m),
        );
        cut.moments.insert(
            MomentType::CorrelationCoefficient,
            f32_grid(
                MomentType::CorrelationCoefficient,
                vec![vec![0.99; gates]],
                spacing_m,
            ),
        );
        cut.moments.insert(
            MomentType::Reflectivity,
            f32_grid(MomentType::Reflectivity, vec![vec![40.0; gates]], spacing_m),
        );

        let config = DerivationConfig::with_products(
            RadarBand::S,
            [
                DerivedSweepProduct::Kdp,
                DerivedSweepProduct::FilteredDifferentialPhase,
            ],
        );
        let report = derive_cut_in_place(&mut cut, &config);
        assert_eq!(report.inserted, vec!["KDP", "PHIF"]);
        let phif = cut
            .moments
            .get(&DerivedSweepProduct::FilteredDifferentialPhase.moment_type())
            .unwrap();
        let kdp = cut
            .moments
            .get(&MomentType::SpecificDifferentialPhase)
            .unwrap();
        assert!(phif.scaled_value(0, 40).unwrap().is_finite());
        assert!(kdp.scaled_value(0, 40).unwrap().is_nan());
    }

    #[test]
    fn hampel_filter_removes_spike_in_flat_window() {
        let values = vec![5.0, 5.0, 5.0, 50.0, 5.0, 5.0, 5.0];
        let filtered = hampel_filter(&values, 3, 3.0);
        assert_eq!(filtered[3], 5.0);
    }

    #[test]
    fn velocity_range_gradient_uses_nyquist_wrapped_delta() {
        let mut cut = cut_with_rows(1, 3, 1000);
        cut.moments.insert(
            MomentType::Velocity,
            f32_grid(MomentType::Velocity, vec![vec![24.0, -24.0, -23.0]], 1000),
        );
        let config = DerivationConfig::with_products(
            RadarBand::S,
            [DerivedSweepProduct::VelocityRangeGradient],
        );
        let report = derive_cut_in_place(&mut cut, &config);
        assert_eq!(report.inserted, vec!["VEL_GRAD_R"]);
        let gradient = cut
            .moments
            .get(&DerivedSweepProduct::VelocityRangeGradient.moment_type())
            .unwrap()
            .scaled_value(0, 1)
            .unwrap();
        assert!(
            (gradient - 1.5).abs() < 0.01,
            "expected wrapped +1.5 m/s/km, got {gradient}"
        );
    }

    #[test]
    fn rho_qc_is_aligned_by_physical_range() {
        let gates = 31;
        let spacing_m = 250;
        let mut cut = cut_with_rows(1, gates, spacing_m);
        cut.moments.insert(
            MomentType::DifferentialPhase,
            f32_grid(
                MomentType::DifferentialPhase,
                vec![(0..gates).map(|gate| gate as f32).collect()],
                spacing_m,
            ),
        );
        let mut rho = f32_grid(
            MomentType::CorrelationCoefficient,
            vec![vec![0.99; 16]],
            500,
        );
        rho.gate_range.first_gate_m = 0;
        if let MomentStorage::F32(values) = &mut rho.storage {
            values[5] = 0.3;
        }
        cut.moments.insert(MomentType::CorrelationCoefficient, rho);
        derive_cut_in_place(&mut cut, &DerivationConfig::kdp_only());
        let kdp = &cut.moments[&MomentType::SpecificDifferentialPhase];
        // 2.5 km in the PHI grid aligns with gate 5 in the 500 m RHO grid.
        assert!(kdp.scaled_value(0, 10).unwrap().is_nan());
    }

    #[test]
    fn cdr_is_finite_for_valid_dual_pol_values() {
        let mut cut = cut_with_rows(1, 1, 250);
        cut.moments.insert(
            MomentType::DifferentialReflectivity,
            f32_grid(MomentType::DifferentialReflectivity, vec![vec![1.0]], 250),
        );
        cut.moments.insert(
            MomentType::CorrelationCoefficient,
            f32_grid(MomentType::CorrelationCoefficient, vec![vec![0.97]], 250),
        );
        let cdr = circular_depolarization_ratio_grid(
            &cut,
            DerivedSweepProduct::CircularDepolarizationRatio.moment_type(),
        )
        .unwrap();
        assert!(cdr.scaled_value(0, 0).unwrap().is_finite());
    }

    #[test]
    fn s_band_coefficients_match_reference_tables() {
        assert_eq!(RadarBand::S.rain_kdp_coefficients(), (50.70, 0.8500));
        assert_eq!(RadarBand::S.attenuation_coefficients(), (0.04, 0.004));
    }
}
