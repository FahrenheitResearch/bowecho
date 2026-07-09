//! Radar-band resolution for wavelength-sensitive derived products.
//!
//! A container format is not a radar-band observation: ODIM and CfRadial
//! carry S-, C-, and X-band data. Resolution therefore uses, in order,
//! decoded transmit frequency, compiled site/network classification, and a
//! small set of explicitly documented provider/site facts. Everything else
//! remains unknown and the product engine fails band-sensitive products
//! closed.

use std::fmt;

use data_source::sites::{SiteKind, SiteRef};
use product_engine::RadarBand;
use radar_core::RadarVolume;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RadarBandProvenance {
    TransmitFrequencyMhz(u32),
    UsSiteCatalog {
        site_id: String,
        classification: &'static str,
    },
    ResearchInstrumentId(String),
    InternationalCatalog {
        provider_id: String,
        site_id: String,
    },
    Unknown,
}

impl fmt::Display for RadarBandProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TransmitFrequencyMhz(mhz) => write!(formatter, "decoded frequency {mhz} MHz"),
            Self::UsSiteCatalog {
                site_id,
                classification,
            } => write!(formatter, "site catalog {site_id} ({classification})"),
            Self::ResearchInstrumentId(site_id) => {
                write!(formatter, "documented research instrument {site_id}")
            }
            Self::InternationalCatalog {
                provider_id,
                site_id,
            } => write!(formatter, "provider catalog {provider_id}/{site_id}"),
            Self::Unknown => write!(
                formatter,
                "no decoded frequency or documented site/provider band metadata"
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RadarBandResolution {
    pub band: RadarBand,
    pub provenance: RadarBandProvenance,
}

pub(crate) fn resolve(volume: &RadarVolume) -> RadarBandResolution {
    if let Some(mhz) = volume.metadata.radar_frequency_mhz
        && let Some(band) = band_for_frequency_mhz(mhz)
    {
        return RadarBandResolution {
            band,
            provenance: RadarBandProvenance::TransmitFrequencyMhz(mhz),
        };
    }

    let site_id = volume.site.id.trim();
    let canonical_us_id = site_id.to_ascii_uppercase();
    let site_ref = SiteRef::Us {
        level2_id: canonical_us_id.clone(),
    };
    if let Some(record) = data_source::sites::resolve(&site_ref) {
        let resolved = match record.kind {
            SiteKind::Wsr88d => Some((RadarBand::S, "WSR-88D")),
            SiteKind::Tdwr => Some((RadarBand::C, "TDWR")),
            SiteKind::Research => known_research_band(&canonical_us_id)
                .map(|band| (band, "research radar")),
            SiteKind::Intl { .. } => None,
        };
        if let Some((band, classification)) = resolved {
            return RadarBandResolution {
                band,
                provenance: RadarBandProvenance::UsSiteCatalog {
                    site_id: canonical_us_id,
                    classification,
                },
            };
        }
    }

    if let Some(band) = known_research_band(&canonical_us_id) {
        return RadarBandResolution {
            band,
            provenance: RadarBandProvenance::ResearchInstrumentId(canonical_us_id),
        };
    }

    // Decoders expose the instrument/site id, not the selected provider.
    // Use provider evidence only when that id resolves to exactly one known
    // international catalog row whose band is explicitly documented.
    let mut candidates = data_source::sites::all_sites().filter_map(|record| {
        let SiteRef::Intl {
            provider_id,
            site_id: catalog_site_id,
        } = record.site
        else {
            return None;
        };
        if !catalog_site_id.eq_ignore_ascii_case(site_id) {
            return None;
        }
        known_international_band(&provider_id, &catalog_site_id).map(|band| {
            RadarBandResolution {
                band,
                provenance: RadarBandProvenance::InternationalCatalog {
                    provider_id,
                    site_id: catalog_site_id,
                },
            }
        })
    });
    if let Some(candidate) = candidates.next()
        && candidates.next().is_none()
    {
        return candidate;
    }

    RadarBandResolution {
        band: RadarBand::Unknown,
        provenance: RadarBandProvenance::Unknown,
    }
}

fn band_for_frequency_mhz(mhz: u32) -> Option<RadarBand> {
    match mhz {
        2_000..=3_999 => Some(RadarBand::S),
        4_000..=7_999 => Some(RadarBand::C),
        8_000..=12_000 => Some(RadarBand::X),
        _ => None,
    }
}

fn known_research_band(site_id: &str) -> Option<RadarBand> {
    let site_id = site_id.trim().to_ascii_uppercase();
    (site_id == "FWLX"
        || site_id.starts_with("DOW")
        || site_id.starts_with("COW")
        || site_id.starts_with("RAXPOL")
        || site_id.starts_with("NOXP"))
    .then_some(RadarBand::X)
}

fn known_international_band(provider_id: &str, site_id: &str) -> Option<RadarBand> {
    match (provider_id, site_id) {
        // The provider module documents both Piemonte instruments as
        // C-band; this is provider metadata, not an ODIM assumption.
        ("piemonte", _) => Some(RadarBand::C),
        // The embedded Australian site catalog explicitly labels site 105
        // (BrisAP / Meteopress) C-band. Other BOM sites are not inferred.
        ("australia-nci", "105") => Some(RadarBand::C),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use radar_core::RadarSite;

    fn volume(site_id: &str) -> RadarVolume {
        RadarVolume::new(
            RadarSite::new(site_id),
            Utc.with_ymd_and_hms(2026, 7, 9, 0, 0, 0)
                .single()
                .expect("valid test time"),
        )
    }

    #[test]
    fn decoded_frequency_is_highest_priority_evidence() {
        let mut volume = volume("KTLX");
        volume.metadata.radar_frequency_mhz = Some(9_400);
        let resolution = resolve(&volume);
        assert_eq!(resolution.band, RadarBand::X);
        assert_eq!(
            resolution.provenance,
            RadarBandProvenance::TransmitFrequencyMhz(9_400)
        );
    }

    #[test]
    fn us_catalog_distinguishes_wsr88d_and_tdwr_without_prefix_guessing() {
        assert_eq!(resolve(&volume("KTLX")).band, RadarBand::S);
        assert_eq!(resolve(&volume("TOKC")).band, RadarBand::C);
        assert_eq!(resolve(&volume("TJUA")).band, RadarBand::S);
    }

    #[test]
    fn documented_mobile_instrument_ids_resolve_x_band() {
        assert_eq!(resolve(&volume("DOW8")).band, RadarBand::X);
        assert_eq!(resolve(&volume("RaXPol")).band, RadarBand::X);
    }

    #[test]
    fn documented_provider_site_evidence_can_resolve_a_band() {
        let resolution = resolve(&volume("bric"));
        assert_eq!(resolution.band, RadarBand::C);
        assert!(matches!(
            resolution.provenance,
            RadarBandProvenance::InternationalCatalog {
                ref provider_id,
                ..
            } if provider_id == "piemonte"
        ));
    }

    #[test]
    fn odim_and_cfradial_container_labels_do_not_imply_c_band() {
        for compression in ["odim-h5", "cfradial1-netcdf3"] {
            let mut volume = volume("UNLISTED");
            volume.metadata.compression = Some(compression.to_owned());
            let resolution = resolve(&volume);
            assert_eq!(resolution.band, RadarBand::Unknown);
            assert_eq!(resolution.provenance, RadarBandProvenance::Unknown);
        }
    }
}
