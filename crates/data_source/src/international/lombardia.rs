//! ARPA Lombardia open radar volume feed.
//!
//! `https://radarlive.arpalombardia.it/Volumi/` exposes two site
//! directories, `DES/` and `FLE/`. Each directory publishes 5-minute
//! gzip-wrapped ODIM HDF5 product volumes named like
//! `Desio.20260626T230000Z_DBZH.h5.gz`. BowEcho builds one radar frame by
//! requiring DBZH + VRADH at the same timestamp and merging optional
//! same-timestamp dual-pol products when present.

use std::collections::{BTreeMap, BTreeSet};

use super::listing::{fnv1a64, join_url, parse_autoindex};
use super::{FramePlan, IntlProvider, IntlSite, PlanPart, RecentFrames};
use crate::fetch_text;

const ROOT: &str = "https://radarlive.arpalombardia.it/Volumi/";
const REQUIRED_PRODUCTS: [&str; 2] = ["DBZH", "VRADH"];
const OPTIONAL_PRODUCTS: [&str; 7] = ["TH", "ZDR", "RHOHV", "PHIDP", "KDP", "WRADH", "CLASS"];

#[derive(Clone, Copy, Debug)]
struct LombardiaSite {
    id: &'static str,
    label: &'static str,
    dir: &'static str,
    file_prefix: &'static str,
    latitude_deg: f32,
    longitude_deg: f32,
}

const SITES: [LombardiaSite; 2] = [
    LombardiaSite {
        id: "des",
        label: "Desio",
        dir: "DES",
        file_prefix: "Desio.",
        // Verified from live ODIM /where group on 2026-06-26.
        latitude_deg: 45.6273,
        longitude_deg: 9.1963,
    },
    LombardiaSite {
        id: "fle",
        label: "Flero",
        dir: "FLE",
        file_prefix: "Flero.",
        // Verified from live ODIM /where group on 2026-06-26.
        latitude_deg: 45.4814,
        longitude_deg: 10.1768,
    },
];

#[derive(Clone, Copy, Debug, Default)]
pub struct LombardiaProvider;

impl LombardiaProvider {
    pub fn new() -> Self {
        Self
    }
}

impl IntlProvider for LombardiaProvider {
    fn id(&self) -> &'static str {
        "arpa-lombardia"
    }

    fn label(&self) -> &'static str {
        "ARPA Lombardia Italy"
    }

    fn country(&self) -> &'static str {
        "Italy"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        Ok(self.static_sites())
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        let site = lombardia_site(site_id)?;
        let files = lombardia_product_files(site)?;
        let stamp = newest_common_stamp(&files).ok_or_else(|| {
            format!(
                "ARPA Lombardia {} listing had no timestamp common to DBZH and VRADH",
                site.label
            )
        })?;
        frame_plan(site, &files, &stamp)
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

impl RecentFrames for LombardiaProvider {
    fn recent_frames(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        let site = lombardia_site(site_id)?;
        let files = lombardia_product_files(site)?;
        let mut stamps = common_stamps(&files);
        let keep = count.max(1).min(stamps.len());
        stamps
            .drain(stamps.len().saturating_sub(keep)..)
            .map(|stamp| frame_plan(site, &files, &stamp))
            .collect::<Result<Vec<_>, _>>()
    }
}

fn lombardia_site(site_id: &str) -> Result<&'static LombardiaSite, String> {
    SITES
        .iter()
        .find(|site| site.id == site_id)
        .ok_or_else(|| format!("ARPA Lombardia: unknown site '{site_id}'"))
}

fn site_url(site: &LombardiaSite) -> String {
    format!("{ROOT}{}/", site.dir)
}

fn lombardia_product_files(
    site: &LombardiaSite,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, String> {
    let url = site_url(site);
    let html = fetch_text(&url)
        .map_err(|err| format!("ARPA Lombardia {} listing {url}: {err}", site.label))?;
    Ok(parse_lombardia_product_files(&html, site.file_prefix))
}

fn parse_lombardia_product_files(
    html: &str,
    file_prefix: &str,
) -> BTreeMap<String, BTreeMap<String, String>> {
    let mut by_product: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for entry in parse_autoindex(html)
        .into_iter()
        .filter(|entry| !entry.is_dir)
    {
        let Some((stamp, product)) = parse_lombardia_file_name(&entry.name, file_prefix) else {
            continue;
        };
        by_product
            .entry(product.to_owned())
            .or_default()
            .insert(stamp.to_owned(), entry.name);
    }
    by_product
}

fn parse_lombardia_file_name<'a>(name: &'a str, file_prefix: &str) -> Option<(&'a str, &'a str)> {
    let body = name.strip_prefix(file_prefix)?.strip_suffix(".h5.gz")?;
    let (stamp, product) = body.split_once('_')?;
    let iso_ok = stamp.len() == 16
        && stamp.as_bytes()[8] == b'T'
        && stamp.as_bytes()[15] == b'Z'
        && stamp.as_bytes()[..8].iter().all(u8::is_ascii_digit)
        && stamp.as_bytes()[9..15].iter().all(u8::is_ascii_digit);
    (iso_ok && !product.is_empty()).then_some((stamp, product))
}

fn common_stamps(files: &BTreeMap<String, BTreeMap<String, String>>) -> Vec<String> {
    let Some(first) = files.get(REQUIRED_PRODUCTS[0]) else {
        return Vec::new();
    };
    let mut stamps: BTreeSet<String> = first.keys().cloned().collect();
    for product in REQUIRED_PRODUCTS.iter().skip(1) {
        let Some(map) = files.get(*product) else {
            return Vec::new();
        };
        stamps.retain(|stamp| map.contains_key(stamp));
    }
    stamps.into_iter().collect()
}

fn newest_common_stamp(files: &BTreeMap<String, BTreeMap<String, String>>) -> Option<String> {
    common_stamps(files).pop()
}

fn frame_plan(
    site: &LombardiaSite,
    files: &BTreeMap<String, BTreeMap<String, String>>,
    stamp: &str,
) -> Result<FramePlan, String> {
    let url = site_url(site);
    let mut products = Vec::new();
    products.extend(REQUIRED_PRODUCTS);
    products.extend(OPTIONAL_PRODUCTS);

    let mut parts = Vec::new();
    for product in products {
        let Some(name) = files.get(product).and_then(|map| map.get(stamp)) else {
            if REQUIRED_PRODUCTS.contains(&product) {
                return Err(format!(
                    "ARPA Lombardia {} missing required {product} at {stamp}",
                    site.label
                ));
            }
            continue;
        };
        parts.push(PlanPart {
            url: join_url(&url, name),
        });
    }
    let joined = parts
        .iter()
        .map(|part| part.url.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(FramePlan {
        identity: format!(
            "{}_{}_p{}_h{:016x}",
            site.id,
            stamp,
            parts.len(),
            fnv1a64(&joined)
        ),
        parts,
        merge: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DES_LISTING: &str = r#"
        <a href="Desio.20260626T225500Z_DBZH.h5.gz">dbzh old</a>
        <a href="Desio.20260626T225500Z_VRADH.h5.gz">vel old</a>
        <a href="Desio.20260626T230000Z_DBZH.h5.gz">dbzh</a>
        <a href="Desio.20260626T230000Z_VRADH.h5.gz">vel</a>
        <a href="Desio.20260626T230000Z_ZDR.h5.gz">zdr</a>
        <a href="Desio.20260626T230000Z_RHOHV.h5.gz">rho</a>
        <a href="Desio.20260626T230000Z_KDP.h5.gz">kdp</a>
        <a href="Desio.20260626T230500Z_DBZH.h5.gz">dbzh only</a>
        <a href="../">parent</a>
    "#;

    #[test]
    fn parser_groups_lombardia_products_by_stamp() {
        let files = parse_lombardia_product_files(DES_LISTING, "Desio.");
        assert_eq!(
            newest_common_stamp(&files).as_deref(),
            Some("20260626T230000Z")
        );
        assert_eq!(
            files["KDP"]["20260626T230000Z"],
            "Desio.20260626T230000Z_KDP.h5.gz"
        );
    }

    #[test]
    fn file_name_parser_requires_exact_lombardia_shape() {
        assert_eq!(
            parse_lombardia_file_name("Flero.20260626T230000Z_DBZH.h5.gz", "Flero."),
            Some(("20260626T230000Z", "DBZH"))
        );
        assert_eq!(
            parse_lombardia_file_name("Flero.20260626T230000_DBZH.h5.gz", "Flero."),
            None
        );
        assert_eq!(
            parse_lombardia_file_name("Flero.20260626T230000Z_DBZH.h5", "Flero."),
            None
        );
    }

    #[test]
    fn frame_plan_uses_required_first_and_optional_same_stamp() {
        let files = parse_lombardia_product_files(DES_LISTING, "Desio.");
        let plan = frame_plan(&SITES[0], &files, "20260626T230000Z").expect("plan");
        assert!(plan.merge);
        assert!(plan.identity.starts_with("des_20260626T230000Z_p5_h"));
        assert!(plan.parts[0].url.ends_with("_DBZH.h5.gz"));
        assert!(plan.parts[1].url.ends_with("_VRADH.h5.gz"));
        assert!(
            plan.parts
                .iter()
                .any(|part| part.url.ends_with("_KDP.h5.gz"))
        );
    }

    #[test]
    fn provider_has_two_static_lombardia_sites() {
        let provider = LombardiaProvider::new();
        let sites = provider.static_sites();
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].provider_id, "arpa-lombardia");
        assert_eq!(sites[0].site_id, "des");
        assert_eq!(sites[1].site_id, "fle");
    }

    #[test]
    #[ignore = "live ARPA Lombardia endpoint probe"]
    fn live_lombardia_latest_resolves_split_gz_hdf5_parts() {
        let provider = LombardiaProvider::new();
        for site in provider.static_sites() {
            let plan = provider.latest(&site.site_id).expect("latest frame");
            assert!(plan.merge);
            assert!(plan.parts.len() >= 2);
            assert!(plan.parts[0].url.ends_with("_DBZH.h5.gz"));
            assert!(plan.parts[1].url.ends_with("_VRADH.h5.gz"));
        }
    }

    #[test]
    #[ignore = "live ARPA Lombardia download/decode probe"]
    fn live_lombardia_gzip_hdf5_part_decodes_through_router() {
        let provider = LombardiaProvider::new();
        let plan = provider.latest("des").expect("latest DES frame");
        let raw = crate::fetch_volume_bytes(&plan.parts[0].url).expect("download DBZH");
        let volume = nexrad_io::decode_supported_volume_bytes(&raw).expect("gzip ODIM decode");
        assert!(
            volume.cuts.iter().any(|cut| !cut.moments.is_empty()),
            "decoded volume should contain at least one moment"
        );
    }
}
