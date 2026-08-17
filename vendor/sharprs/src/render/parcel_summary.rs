//! Native parcel summaries for external diagnostic annotation.
//!
//! This does not compute any external diagnostics. It only exposes the
//! SB/ML/MU parcel context already computed by `sharprs` so callers can
//! annotate entraining diagnostics such as ECAPE without pretending those
//! values came from the sounding engine itself.

use super::ComputedParams;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeParcelFlavor {
    SurfaceBased,
    MixedLayer,
    MostUnstable,
}

impl NativeParcelFlavor {
    pub const fn short_label(self) -> &'static str {
        match self {
            Self::SurfaceBased => "SB",
            Self::MixedLayer => "ML",
            Self::MostUnstable => "MU",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeParcelSummary {
    pub flavor: NativeParcelFlavor,
    pub cape_j_kg: f64,
    pub cin_j_kg: f64,
    pub lcl_m_agl: f64,
    pub lfc_m_agl: f64,
    pub el_m_agl: f64,
}

pub fn native_parcel_summaries(params: &ComputedParams) -> [NativeParcelSummary; 3] {
    [
        NativeParcelSummary {
            flavor: NativeParcelFlavor::SurfaceBased,
            cape_j_kg: params.sfcpcl.bplus,
            cin_j_kg: params.sfcpcl.bminus,
            lcl_m_agl: params.sfcpcl.lclhght,
            lfc_m_agl: params.sfcpcl.lfchght,
            el_m_agl: params.sfcpcl.elhght,
        },
        NativeParcelSummary {
            flavor: NativeParcelFlavor::MixedLayer,
            cape_j_kg: params.mlpcl.bplus,
            cin_j_kg: params.mlpcl.bminus,
            lcl_m_agl: params.mlpcl.lclhght,
            lfc_m_agl: params.mlpcl.lfchght,
            el_m_agl: params.mlpcl.elhght,
        },
        NativeParcelSummary {
            flavor: NativeParcelFlavor::MostUnstable,
            cape_j_kg: params.mupcl.bplus,
            cin_j_kg: params.mupcl.bminus,
            lcl_m_agl: params.mupcl.lclhght,
            lfc_m_agl: params.mupcl.lfchght,
            el_m_agl: params.mupcl.elhght,
        },
    ]
}

#[cfg(test)]
mod tests {
    use crate::profile::{Profile, StationInfo};
    use crate::render::{compute_all_params, native_parcel_summaries};

    fn sample_profile() -> Profile {
        Profile::from_uv(
            &[1000.0, 925.0, 850.0, 700.0, 500.0, 300.0],
            &[110.0, 780.0, 1510.0, 3120.0, 5610.0, 9200.0],
            &[28.0, 22.5, 16.0, 3.0, -14.5, -39.0],
            &[21.0, 17.0, 11.5, -4.5, -29.0, -50.0],
            &[5.0, 10.0, 18.0, 32.0, 48.0, 62.0],
            &[8.0, 12.0, 15.0, 13.0, 2.0, -8.0],
            &[],
            StationInfo {
                station_id: "TST".into(),
                latitude: 35.22,
                longitude: -97.44,
                elevation: 397.0,
                datetime: "2026-04-14T20:00:00Z".into(),
            },
        )
        .expect("profile")
    }

    #[test]
    fn exposes_native_sb_ml_mu_parcel_rows() {
        let profile = sample_profile();
        let params = compute_all_params(&profile);
        let rows = native_parcel_summaries(&params);

        assert_eq!(rows[0].flavor.short_label(), "SB");
        assert_eq!(rows[1].flavor.short_label(), "ML");
        assert_eq!(rows[2].flavor.short_label(), "MU");
        assert!(rows.iter().all(|row| row.cape_j_kg.is_finite()));
        assert!(rows.iter().all(|row| row.cin_j_kg.is_finite()));
    }
}
