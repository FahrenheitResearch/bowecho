//! ARPA Piemonte open radar volume feed for north-western Italy.
//!
//! ARPA Piemonte documents two C-band Doppler/polarimetric radars, Bric
//! della Croce and Monte Settepani, updating every 5 minutes and publishing
//! the last hour of OPERA HDF5 volumes. The public directories are plain
//! Apache autoindexes:
//!
//! - `https://www.arpa.piemonte.it/rischi_naturali/radar/bric/`
//! - `https://www.arpa.piemonte.it/rischi_naturali/radar/sett/`
//!
//! Each timestamp has a full `PAGZ4x_C_PIEM_YYYYMMDDHHMMSS.h5` volume plus
//! smaller per-moment sidecars. BowEcho uses the full volume as the frame so
//! the existing ODIM decoder sees the site geometry and all moments together.

use super::listing::{join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames};
use crate::fetch_text;

const BRIC_ROOT: &str = "https://www.arpa.piemonte.it/rischi_naturali/radar/bric/";
const SETT_ROOT: &str = "https://www.arpa.piemonte.it/rischi_naturali/radar/sett/";

#[derive(Clone, Copy, Debug)]
struct PiemonteSite {
    id: &'static str,
    label: &'static str,
    root: &'static str,
    file_prefix: &'static str,
    latitude_deg: f32,
    longitude_deg: f32,
}

const SITES: [PiemonteSite; 2] = [
    PiemonteSite {
        id: "bric",
        label: "Bric della Croce",
        root: BRIC_ROOT,
        file_prefix: "PAGZ41_C_PIEM_",
        // Verified from live ODIM /where group on 2026-06-26.
        latitude_deg: 45.0342,
        longitude_deg: 7.7327,
    },
    PiemonteSite {
        id: "sett",
        label: "Monte Settepani",
        root: SETT_ROOT,
        file_prefix: "PAGZ42_C_PIEM_",
        // Verified from live ODIM /where group on 2026-06-26.
        latitude_deg: 44.2450,
        longitude_deg: 8.1978,
    },
];

/// ARPA Piemonte, Italy: two live single-file OPERA HDF5 polar volume sites.
#[derive(Clone, Copy, Debug, Default)]
pub struct PiemonteProvider;

impl PiemonteProvider {
    pub fn new() -> Self {
        Self
    }
}

impl IntlProvider for PiemonteProvider {
    fn id(&self) -> &'static str {
        "arpa-piemonte"
    }

    fn label(&self) -> &'static str {
        "ARPA Piemonte Italy"
    }

    fn country(&self) -> &'static str {
        "Italy"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        Ok(self.static_sites())
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        let site = piemonte_site(site_id)?;
        newest_frame(site).ok_or_else(|| {
            format!(
                "ARPA Piemonte {} listing returned no full {}YYYYMMDDHHMMSS.h5 volumes",
                site.label, site.file_prefix
            )
        })
    }

    fn recent_source(&self) -> Option<&dyn RecentFrames> {
        Some(self)
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        SITES
            .iter()
            .map(|site| IntlSite {
                provider_id: self.id(),
                site_id: site.id.to_owned(),
                label: site.label.to_owned(),
                country: self.country(),
                latitude_deg: Some(site.latitude_deg),
                longitude_deg: Some(site.longitude_deg),
            })
            .collect()
    }
}

impl RecentFrames for PiemonteProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        let site = piemonte_site(site_id)?;
        let mut files = piemonte_volume_files(site)?;
        files.sort();
        let keep = count.max(1).min(files.len());
        Ok(files
            .into_iter()
            .rev()
            .take(keep)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|file| frame_plan(site, file))
            .collect())
    }
}

fn piemonte_site(site_id: &str) -> Result<&'static PiemonteSite, String> {
    SITES
        .iter()
        .find(|site| site.id == site_id)
        .ok_or_else(|| format!("ARPA Piemonte: unknown site '{site_id}'"))
}

fn newest_frame(site: &PiemonteSite) -> Option<FramePlan> {
    let mut files = piemonte_volume_files(site).ok()?;
    files.sort();
    files.pop().map(|file| frame_plan(site, file))
}

fn piemonte_volume_files(site: &PiemonteSite) -> Result<Vec<String>, String> {
    let html = fetch_text(site.root)
        .map_err(|err| format!("ARPA Piemonte {} listing {}: {err}", site.label, site.root))?;
    Ok(parse_piemonte_volume_files(&html, site.file_prefix))
}

fn frame_plan(site: &PiemonteSite, file: String) -> FramePlan {
    FramePlan {
        identity: file.clone(),
        parts: vec![PlanPart {
            url: join_url(site.root, &file),
        }],
        merge: false,
    }
}

fn parse_piemonte_volume_files(html: &str, file_prefix: &str) -> Vec<String> {
    parse_autoindex(html)
        .into_iter()
        .filter(|entry| !entry.is_dir && piemonte_stamp(&entry.name, file_prefix).is_some())
        .map(|entry| entry.name)
        .collect()
}

fn piemonte_stamp<'a>(name: &'a str, file_prefix: &str) -> Option<&'a str> {
    let stamp = name.strip_prefix(file_prefix)?.strip_suffix(".h5")?;
    (stamp.len() == 14 && stamp.as_bytes().iter().all(u8::is_ascii_digit)).then_some(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIC_LISTING: &str = r#"
        <a href="PAGZ41_C_PIEM_20260626205502.h5">full</a>
        <a href="PAGZ41_C_PIEM_20260626205502RhoHVu.h5">rho</a>
        <a href="PAGZ41_C_PIEM_20260626205502Vu.h5">vel</a>
        <a href="PAGZ41_C_PIEM_20260626210002.h5">newer</a>
        <a href="?C=N;O=D">sort</a>
        <a href="../">parent</a>
    "#;

    #[test]
    fn full_volume_parser_excludes_per_moment_sidecars() {
        let files = parse_piemonte_volume_files(BRIC_LISTING, "PAGZ41_C_PIEM_");
        assert_eq!(
            files,
            [
                "PAGZ41_C_PIEM_20260626205502.h5",
                "PAGZ41_C_PIEM_20260626210002.h5"
            ]
        );
    }

    #[test]
    fn stamp_requires_exact_full_volume_name() {
        assert_eq!(
            piemonte_stamp("PAGZ42_C_PIEM_20260626205502.h5", "PAGZ42_C_PIEM_"),
            Some("20260626205502")
        );
        assert_eq!(
            piemonte_stamp("PAGZ42_C_PIEM_20260626205502dBZ.h5", "PAGZ42_C_PIEM_"),
            None
        );
        assert_eq!(
            piemonte_stamp("PAGZ42_C_PIEM_2026062620550.h5", "PAGZ42_C_PIEM_"),
            None
        );
    }

    #[test]
    fn provider_has_two_static_italian_sites() {
        let provider = PiemonteProvider::new();
        let sites = provider.static_sites();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].provider_id, "arpa-piemonte");
        assert_eq!(sites[0].site_id, "bric");
        assert_eq!(sites[1].site_id, "sett");
        assert_eq!(sites[0].country, "Italy");
    }

    #[test]
    #[ignore = "live ARPA Piemonte endpoint probe"]
    fn live_piemonte_latest_resolves_full_hdf5_volumes() {
        let provider = PiemonteProvider::new();
        for site in provider.static_sites() {
            let plan = provider.latest(&site.site_id).expect("latest frame");
            assert_eq!(plan.parts.len(), 1);
            assert!(!plan.merge);
            assert!(plan.identity.ends_with(".h5"), "{}", plan.identity);
            assert!(
                !plan.identity.contains("RhoHV")
                    && !plan.identity.contains("ZDR")
                    && !plan.identity.contains("Vu")
                    && !plan.identity.contains("dBZ")
                    && !plan.identity.contains("dBuZ"),
                "{}",
                plan.identity
            );
            assert!(
                plan.parts[0]
                    .url
                    .starts_with("https://www.arpa.piemonte.it/")
            );
        }
    }
}
