//! miniDerecho's curated product enum + capability-as-value
//! (miniderecho-spec §3.9, §11 M1). A fresh, small mirror of the
//! render2d-facing subset — deliberately NOT BowEcho's `DisplayProduct`,
//! which is entangled with the DerivedProduct picker machinery. M2 adds
//! CREF/VIL via product_engine/render2d::volumetric.
//!
//! Capability is a VALUE (house trust-label rule, spec §0 invariant 5):
//! [`capability`] returns either the tilt list a product can render from or
//! the reason string the UI greys the product with — the reason IS the
//! returned value, never a UI-side guess.

use radar_core::{MomentType, RadarVolume};
use render2d::StormMotion;

/// Knots → m/s (same constant BowEcho uses).
const KNOT_TO_MPS: f32 = 0.514_444;

/// M1 storm-relative default, matching BowEcho's
/// `DEFAULT_STORM_MOTION_DIRECTION_DEG` / `DEFAULT_STORM_MOTION_SPEED_KT`
/// (45° at 35 kt). A user-adjustable motion input is post-M1; the constant
/// keeps SRV honest rather than greyed.
pub const DEFAULT_STORM_MOTION: StormMotion = StormMotion {
    direction_deg: 45.0,
    speed_mps: 35.0 * KNOT_TO_MPS,
};

/// The v1 product set (spec §3.9): REF plus the core moments a Level-II
/// volume actually carries, the dealiased/storm-relative velocity pair
/// included. Order = bar/picker display order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Product {
    Ref,
    Vel,
    DealiasedVel,
    Srv,
    Cc,
    Zdr,
    Kdp,
    Sw,
}

impl Product {
    pub const ALL: [Product; 8] = [
        Self::Ref,
        Self::Vel,
        Self::DealiasedVel,
        Self::Srv,
        Self::Cc,
        Self::Zdr,
        Self::Kdp,
        Self::Sw,
    ];

    /// Settings persistence key (MiniSettings; stable, lowercase).
    pub fn key(&self) -> &'static str {
        match self {
            Self::Ref => "ref",
            Self::Vel => "vel",
            Self::DealiasedVel => "dvel",
            Self::Srv => "srv",
            Self::Cc => "cc",
            Self::Zdr => "zdr",
            Self::Kdp => "kdp",
            Self::Sw => "sw",
        }
    }

    /// Total decode of a persisted key: unknown keys fall back to REF
    /// (never drop the user into a blank product).
    pub fn from_key(key: &str) -> Product {
        Self::ALL
            .into_iter()
            .find(|product| product.key() == key)
            .unwrap_or(Self::Ref)
    }

    /// Short bar label (BowEcho's grammar: DVEL/SRV).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Ref => "REF",
            Self::Vel => "VEL",
            Self::DealiasedVel => "DVEL",
            Self::Srv => "SRV",
            Self::Cc => "CC",
            Self::Zdr => "ZDR",
            Self::Kdp => "KDP",
            Self::Sw => "SW",
        }
    }

    pub fn long_label(&self) -> &'static str {
        match self {
            Self::Ref => "Reflectivity",
            Self::Vel => "Velocity",
            Self::DealiasedVel => "Dealiased Velocity",
            Self::Srv => "Storm Relative Velocity",
            Self::Cc => "Correlation Coefficient",
            Self::Zdr => "Differential Reflectivity",
            Self::Kdp => "Specific Differential Phase",
            Self::Sw => "Spectrum Width",
        }
    }

    /// The moment the volume must carry for this product to render.
    pub fn base_moment(&self) -> MomentType {
        match self {
            Self::Ref => MomentType::Reflectivity,
            Self::Vel | Self::DealiasedVel | Self::Srv => MomentType::Velocity,
            Self::Cc => MomentType::CorrelationCoefficient,
            Self::Zdr => MomentType::DifferentialReflectivity,
            Self::Kdp => MomentType::SpecificDifferentialPhase,
            Self::Sw => MomentType::SpectrumWidth,
        }
    }

    /// How the render worker realizes this product over render2d's public
    /// API (color tables are the crate defaults — Analyst Velocity HD +
    /// house REF; zero new color systems, spec §4).
    pub fn render_product(&self) -> crate::render_worker::RenderProduct {
        use crate::render_worker::RenderProduct;
        match self {
            Self::DealiasedVel => RenderProduct::DealiasedVelocity,
            Self::Srv => RenderProduct::StormRelative {
                storm_motion: DEFAULT_STORM_MOTION,
            },
            _ => RenderProduct::Moment(self.base_moment()),
        }
    }
}

/// One selectable tilt: a concrete cut index plus its elevation angle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TiltOption {
    pub cut_index: usize,
    pub elevation_deg: f32,
}

/// Tilt key used for dedupe and for the persisted tilt preference:
/// elevation in tenths of a degree (Eq-safe integer — MiniSettings derives
/// Eq, so no f32 field).
pub fn elevation_tenths(elevation_deg: f32) -> i32 {
    (elevation_deg * 10.0).round() as i32
}

/// Tilt list for a product: cuts carrying its base moment with data,
/// lowest-first (the house convention), deduped to one entry per tenth of
/// a degree keeping the LATEST cut (a mid-volume SAILS rescan of 0.5°
/// supersedes the volume-start sweep).
pub fn tilt_options(volume: &RadarVolume, product: Product) -> Vec<TiltOption> {
    let moment = product.base_moment();
    let mut options: Vec<TiltOption> = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, cut)| {
            cut.moments
                .get(&moment)
                .is_some_and(|grid| !grid.radial_indices.is_empty())
        })
        .map(|(cut_index, cut)| TiltOption {
            cut_index,
            elevation_deg: cut.elevation_deg,
        })
        .collect();
    options.sort_by(|a, b| {
        a.elevation_deg
            .total_cmp(&b.elevation_deg)
            .then(a.cut_index.cmp(&b.cut_index))
    });
    // Same tenth-degree bucket: the later element (higher cut index after
    // the sort above) replaces the kept one.
    options.dedup_by(|later, kept| {
        if elevation_tenths(later.elevation_deg) == elevation_tenths(kept.elevation_deg) {
            *kept = *later;
            true
        } else {
            false
        }
    });
    options
}

/// Capability-as-value: the tilts this product can render from this
/// volume, or the reason the UI greys it with. The reason string IS the
/// value — callers display it verbatim (hover text), never invent one.
pub fn capability(volume: &RadarVolume, product: Product) -> Result<Vec<TiltOption>, String> {
    if volume.cuts.is_empty() {
        return Err("no cuts decoded yet".to_owned());
    }
    let options = tilt_options(volume, product);
    if options.is_empty() {
        return Err(format!(
            "this volume carries no {}",
            product.base_moment().short_name()
        ));
    }
    Ok(options)
}

/// Resolve the persisted tilt preference against a volume's tilt list:
/// nearest elevation wins (ties go low); no preference ⇒ the lowest tilt.
/// Returns `None` only for an empty list.
pub fn select_tilt(options: &[TiltOption], preference_tenths: Option<i32>) -> Option<TiltOption> {
    let Some(preference) = preference_tenths else {
        return options.first().copied();
    };
    options
        .iter()
        .min_by_key(|option| {
            let tenths = elevation_tenths(option.elevation_deg);
            ((tenths - preference).abs(), tenths)
        })
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use radar_core::{ElevationCut, GateRange, MomentGrid, RadarSite};

    fn cut_with(elevation_deg: f32, moments: &[MomentType]) -> ElevationCut {
        let mut cut = ElevationCut::new(elevation_deg, None);
        for moment in moments {
            let mut grid = MomentGrid::new_u8(
                moment.clone(),
                GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 1_000,
                    gate_count: 4,
                },
                1.0,
                0.0,
                Some(0),
                Some(1),
            );
            grid.push_u8_row_slice(0, &[10, 20, 30, 40]).expect("row");
            cut.moments.insert(moment.clone(), grid);
        }
        cut
    }

    /// A split-cut VCP shape: surveillance cuts carry REF, Doppler cuts
    /// VEL/SW, dual-pol everywhere REF lives; no KDP anywhere (Level-II
    /// carries PHI — KDP greys honestly).
    fn split_cut_volume() -> RadarVolume {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        let dual = [
            MomentType::Reflectivity,
            MomentType::CorrelationCoefficient,
            MomentType::DifferentialReflectivity,
        ];
        volume.cuts.push(cut_with(0.5, &dual)); // surveillance 0.5°
        volume.cuts.push(cut_with(
            0.5,
            &[MomentType::Velocity, MomentType::SpectrumWidth],
        )); // Doppler 0.5°
        volume.cuts.push(cut_with(1.4, &dual));
        volume.cuts.push(cut_with(
            1.4,
            &[MomentType::Velocity, MomentType::SpectrumWidth],
        ));
        volume.cuts.push(cut_with(0.5, &dual)); // SAILS rescan of 0.5°
        volume
    }

    #[test]
    fn product_keys_round_trip_and_unknown_falls_back_to_ref() {
        for product in Product::ALL {
            assert_eq!(Product::from_key(product.key()), product);
        }
        assert_eq!(Product::from_key("nonsense"), Product::Ref);
        assert_eq!(Product::from_key(""), Product::Ref);
    }

    #[test]
    fn capability_per_product_over_a_split_cut_volume() {
        let volume = split_cut_volume();
        for product in [
            Product::Ref,
            Product::Vel,
            Product::DealiasedVel,
            Product::Srv,
            Product::Cc,
            Product::Zdr,
            Product::Sw,
        ] {
            let tilts = capability(&volume, product)
                .unwrap_or_else(|reason| panic!("{product:?} should be available: {reason}"));
            assert!(!tilts.is_empty(), "{product:?}");
        }
        // KDP is not a Level-II moment: greyed with the reason as value.
        assert_eq!(
            capability(&volume, Product::Kdp),
            Err("this volume carries no KDP".to_owned())
        );
        // The three velocity products share the VEL requirement.
        let no_vel = {
            let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
            volume.cuts.push(cut_with(0.5, &[MomentType::Reflectivity]));
            volume
        };
        for product in [Product::Vel, Product::DealiasedVel, Product::Srv] {
            assert_eq!(
                capability(&no_vel, product),
                Err("this volume carries no VEL".to_owned()),
                "{product:?}"
            );
        }
    }

    #[test]
    fn empty_volume_reports_no_cuts_yet() {
        let volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());
        assert_eq!(
            capability(&volume, Product::Ref),
            Err("no cuts decoded yet".to_owned())
        );
    }

    #[test]
    fn tilt_list_is_lowest_first_and_keeps_the_latest_duplicate_cut() {
        let volume = split_cut_volume();
        let tilts = tilt_options(&volume, Product::Ref);
        let shape: Vec<(usize, i32)> = tilts
            .iter()
            .map(|t| (t.cut_index, elevation_tenths(t.elevation_deg)))
            .collect();
        // 0.5° dedupes onto the SAILS rescan (cut 4, not cut 0); 1.4°
        // follows; Doppler-only cuts never appear for REF.
        assert_eq!(shape, vec![(4, 5), (2, 14)]);

        let vel_tilts = tilt_options(&volume, Product::Vel);
        let vel_shape: Vec<usize> = vel_tilts.iter().map(|t| t.cut_index).collect();
        assert_eq!(vel_shape, vec![1, 3]);
    }

    #[test]
    fn select_tilt_defaults_low_and_honors_the_nearest_preference() {
        let volume = split_cut_volume();
        let tilts = tilt_options(&volume, Product::Ref);
        assert_eq!(select_tilt(&tilts, None).unwrap().cut_index, 4);
        // Preference 1.3° → nearest is 1.4°.
        assert_eq!(select_tilt(&tilts, Some(13)).unwrap().cut_index, 2);
        // Preference far above everything clamps to the highest.
        assert_eq!(select_tilt(&tilts, Some(195)).unwrap().cut_index, 2);
        // Exact midpoint ties go to the LOWER tilt (0.95° between 0.5/1.4
        // rounds per-tenth: 9 is 4 away from 5 and 5 away from 14).
        assert_eq!(select_tilt(&tilts, Some(9)).unwrap().cut_index, 4);
        assert_eq!(select_tilt(&[], None), None);
        assert_eq!(select_tilt(&[], Some(5)), None);
    }

    #[test]
    fn srv_maps_onto_the_storm_relative_render_product_with_the_default_motion() {
        use crate::render_worker::RenderProduct;
        match Product::Srv.render_product() {
            RenderProduct::StormRelative { storm_motion } => {
                assert_eq!(storm_motion.direction_deg, 45.0);
                assert!((storm_motion.speed_mps - 35.0 * KNOT_TO_MPS).abs() < 1e-6);
            }
            other => panic!("want StormRelative, got {other:?}"),
        }
        assert_eq!(
            Product::DealiasedVel.render_product(),
            RenderProduct::DealiasedVelocity
        );
        assert_eq!(
            Product::Cc.render_product(),
            RenderProduct::Moment(MomentType::CorrelationCoefficient)
        );
    }
}
