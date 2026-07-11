//! IMGW-PIB POLRAD dual-polarization CMAX catalog.
//!
//! IMGW publishes these high-value products through its national file
//! datastore, not the EUMETNET Open Radar Data API. Discovery is a small
//! form POST which returns links; fetching a listing must never download an
//! HDF5 body. Each linked file is an ODIM_H5 2.3 `IMAGE` with a top-down
//! `MAX` grid and max-side projections, not a recoverable polar volume.
//!
//! File names are `YYYYMMDDHHMMSSCC<quantity>.max.h5`: `SS` is part of the
//! observation time and `CC` is a product counter. Some Ramża scans publish
//! the same KDP/RHOHV field at counters 00 and 01 while ZDR/PHIDP only appear
//! at 01. Consequently cycles group on site plus the first 14 timestamp
//! digits and deliberately ignore only `CC`; duplicate quantities prefer the
//! highest counter.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDateTime, Utc};
use reqwest::header::{ACCEPT, REFERER};

/// Public IMGW file-datastore page and request referer.
pub const IMGW_DATASTORE_URL: &str = "https://danepubliczne.imgw.pl/pl/datastore";
/// Form endpoint used to list files for one datastore product path.
pub const IMGW_DATASTORE_LIST_URL: &str = "https://danepubliczne.imgw.pl/pl/datastore/getFilesList";
/// Stable prefix used by the datastore's file links.
pub const IMGW_DATASTORE_DOWNLOAD_BASE: &str =
    "https://danepubliczne.imgw.pl/pl/datastore/getfiledown";
/// IMGW-PIB's published terms for reuse of public data.
pub const IMGW_DATA_TERMS_URL: &str = "https://danepubliczne.imgw.pl/pl/introduction";
/// Exact source notice required by section 5 of IMGW-PIB's published terms.
pub const IMGW_SOURCE_NOTICE_PL: &str = "Źródłem pochodzenia danych jest Instytut Meteorologii i Gospodarki Wodnej – Państwowy Instytut Badawczy";
/// Additional exact notice required when an output contains processed data.
pub const IMGW_PROCESSED_NOTICE_PL: &str = "Dane Instytutu Meteorologii i Gospodarki Wodnej – Państwowego Instytutu Badawczego zostały przetworzone";

/// Attribution obligations carried with IMGW-derived output surfaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImgwAttributionRequirements {
    pub terms_url: &'static str,
    pub source_notice_pl: &'static str,
    pub processed_notice_pl: &'static str,
    pub source_notice_required: bool,
    pub processed_notice_required_when_modified: bool,
}

pub const IMGW_ATTRIBUTION_REQUIREMENTS: ImgwAttributionRequirements =
    ImgwAttributionRequirements {
        terms_url: IMGW_DATA_TERMS_URL,
        source_notice_pl: IMGW_SOURCE_NOTICE_PL,
        processed_notice_pl: IMGW_PROCESSED_NOTICE_PL,
        source_notice_required: true,
        processed_notice_required_when_modified: true,
    };

/// The ten sites in the modernized POLRAD network.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ImgwPolradSite {
    Brzuchania,
    NowyGdansk,
    GoraSwietejAnny,
    Legionowo,
    Pastewnik,
    Poznan,
    Ramza,
    Rzeszow,
    Swidwin,
    Uzranki,
}

pub const IMGW_POLRAD_SITES: &[ImgwPolradSite] = &[
    ImgwPolradSite::Brzuchania,
    ImgwPolradSite::NowyGdansk,
    ImgwPolradSite::GoraSwietejAnny,
    ImgwPolradSite::Legionowo,
    ImgwPolradSite::Pastewnik,
    ImgwPolradSite::Poznan,
    ImgwPolradSite::Ramza,
    ImgwPolradSite::Rzeszow,
    ImgwPolradSite::Swidwin,
    ImgwPolradSite::Uzranki,
];

impl ImgwPolradSite {
    /// Lower-case datastore/site code.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Brzuchania => "brz",
            Self::NowyGdansk => "gdy",
            Self::GoraSwietejAnny => "gsa",
            Self::Legionowo => "leg",
            Self::Pastewnik => "pas",
            Self::Poznan => "poz",
            Self::Ramza => "ram",
            Self::Rzeszow => "rze",
            Self::Swidwin => "swi",
            Self::Uzranki => "uzr",
        }
    }

    /// Upper-case value currently stored in ODIM `/how/system`.
    pub const fn system_code(self) -> &'static str {
        match self {
            Self::Brzuchania => "BRZ",
            Self::NowyGdansk => "GDY",
            Self::GoraSwietejAnny => "GSA",
            Self::Legionowo => "LEG",
            Self::Pastewnik => "PAS",
            Self::Poznan => "POZ",
            Self::Ramza => "RAM",
            Self::Rzeszow => "RZE",
            Self::Swidwin => "SWI",
            Self::Uzranki => "UZR",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Brzuchania => "Brzuchania",
            Self::NowyGdansk => "Nowy Gdańsk",
            Self::GoraSwietejAnny => "Góra Św. Anny",
            Self::Legionowo => "Legionowo",
            Self::Pastewnik => "Pastewnik",
            Self::Poznan => "Poznań",
            Self::Ramza => "Ramża",
            Self::Rzeszow => "Rzeszów",
            Self::Swidwin => "Świdwin",
            Self::Uzranki => "Użranki",
        }
    }

    /// Site position from current ODIM `/where` metadata.
    pub const fn location(self) -> (f64, f64, f64) {
        match self {
            Self::Brzuchania => (50.394_169, 20.083_228, 434.4),
            Self::NowyGdansk => (54.500_917, 18.271_842, 261.0),
            Self::GoraSwietejAnny => (50.463_864, 18.153_211, 433.0),
            Self::Legionowo => (52.405_250, 20.961_110, 122.3),
            Self::Pastewnik => (50.892_460, 16.039_494, 691.9),
            Self::Poznan => (52.413_253, 16.796_986, 123.3),
            Self::Ramza => (50.151_328, 18.725_094, 357.1),
            Self::Rzeszow => (50.114_060, 22.037_000, 241.2),
            Self::Swidwin => (53.795_786, 15.836_828, 146.6),
            Self::Uzranki => (53.855_733, 21.412_331, 237.0),
        }
    }

    /// Exact datastore directory for the site's dual-pol CMAX files.
    pub fn cmax_path(self) -> String {
        format!("/Oper/Polrad/Produkty/HVD/HVD_{}_250.max", self.code())
    }

    pub fn from_code(code: &str) -> Option<Self> {
        IMGW_POLRAD_SITES
            .iter()
            .copied()
            .find(|site| site.code().eq_ignore_ascii_case(code.trim()))
    }
}

/// Dual-polarization quantity carried by an IMGW CMAX file.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ImgwPolradQuantity {
    Kdp,
    RhoHv,
    Zdr,
    PhiDp,
}

pub const IMGW_POLRAD_QUANTITIES: &[ImgwPolradQuantity] = &[
    ImgwPolradQuantity::Kdp,
    ImgwPolradQuantity::RhoHv,
    ImgwPolradQuantity::Zdr,
    ImgwPolradQuantity::PhiDp,
];

impl ImgwPolradQuantity {
    /// Case-sensitive token used in IMGW file names.
    pub const fn filename_token(self) -> &'static str {
        match self {
            Self::Kdp => "KDP",
            Self::RhoHv => "RhoHV",
            Self::Zdr => "ZDR",
            Self::PhiDp => "PhiDP",
        }
    }

    /// Canonical ODIM quantity value.
    pub const fn odim_quantity(self) -> &'static str {
        match self {
            Self::Kdp => "KDP",
            Self::RhoHv => "RHOHV",
            Self::Zdr => "ZDR",
            Self::PhiDp => "PHIDP",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Kdp => "Specific differential phase (KDP)",
            Self::RhoHv => "Correlation coefficient (RHOHV)",
            Self::Zdr => "Differential reflectivity (ZDR)",
            Self::PhiDp => "Differential phase (PHIDP)",
        }
    }

    pub const fn units(self) -> &'static str {
        match self {
            Self::Kdp => "deg/km",
            Self::RhoHv => "1",
            Self::Zdr => "dB",
            Self::PhiDp => "deg",
        }
    }

    fn from_filename_tail(tail: &str) -> Option<Self> {
        match tail {
            "KDP.max.h5" => Some(Self::Kdp),
            "RhoHV.max.h5" => Some(Self::RhoHv),
            "ZDR.max.h5" => Some(Self::Zdr),
            "PhiDP.max.h5" => Some(Self::PhiDp),
            _ => None,
        }
    }
}

/// One validated, directly downloadable IMGW CMAX object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImgwCmaxFile {
    pub site: ImgwPolradSite,
    pub quantity: ImgwPolradQuantity,
    pub observed_at: DateTime<Utc>,
    /// The final `CC` in the 16-digit file prefix. Not part of cycle time.
    pub product_counter: u8,
    pub filename: String,
    pub download_url: String,
    /// Stable cache/dedupe identity. It never includes a fetch time or cookie.
    pub identity: String,
}

/// All currently published quantities for one site/observation time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImgwCmaxCycle {
    pub site: ImgwPolradSite,
    pub observed_at: DateTime<Utc>,
    /// One selected file per quantity, sorted by [`ImgwPolradQuantity`].
    pub files: Vec<ImgwCmaxFile>,
    /// Changes if a late quantity or higher product counter is published.
    pub identity: String,
}

impl ImgwCmaxCycle {
    pub fn file(&self, quantity: ImgwPolradQuantity) -> Option<&ImgwCmaxFile> {
        self.files.iter().find(|file| file.quantity == quantity)
    }
}

/// Validate one IMGW CMAX file name and construct its stable fetch plan.
///
/// The parser accepts only the four known case-sensitive quantity tokens and
/// an exact ASCII basename. Slashes, query strings, invalid dates, and portal
/// assets such as PNG thumbnails are rejected.
pub fn parse_imgw_cmax_filename(site: ImgwPolradSite, filename: &str) -> Option<ImgwCmaxFile> {
    if !filename.is_ascii() || filename.len() < 16 {
        return None;
    }
    let prefix = filename.get(..16)?;
    if !prefix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let timestamp = NaiveDateTime::parse_from_str(prefix.get(..14)?, "%Y%m%d%H%M%S").ok()?;
    let quantity = ImgwPolradQuantity::from_filename_tail(filename.get(16..)?)?;
    let product_counter = prefix.get(14..16)?.parse::<u8>().ok()?;
    let path = site.cmax_path();
    Some(ImgwCmaxFile {
        site,
        quantity,
        observed_at: DateTime::<Utc>::from_naive_utc_and_offset(timestamp, Utc),
        product_counter,
        filename: filename.to_owned(),
        download_url: format!("{IMGW_DATASTORE_DOWNLOAD_BASE}{path}/{filename}"),
        identity: format!("imgw-polrad/{}/{filename}", site.code()),
    })
}

/// Parse the official datastore's HTML fragment into chronological cycles.
///
/// Only links whose path exactly matches `site.cmax_path()` are considered.
/// Malformed and unrelated links are ignored. RAM-style counter duplicates
/// are collapsed by `(site, observed_at, quantity)`, preferring the highest
/// counter while retaining seconds as part of the observation timestamp.
pub fn parse_imgw_cmax_listing(site: ImgwPolradSite, html: &str) -> Vec<ImgwCmaxCycle> {
    let path = site.cmax_path();
    let relative_prefix = format!("datastore/getfiledown{path}/");
    let rooted_prefix = format!("/pl/{relative_prefix}");
    let absolute_prefix = format!("https://danepubliczne.imgw.pl{rooted_prefix}");
    let mut cycles = BTreeMap::<DateTime<Utc>, BTreeMap<ImgwPolradQuantity, ImgwCmaxFile>>::new();

    for href in href_values(html) {
        let filename = href
            .strip_prefix(&relative_prefix)
            .or_else(|| href.strip_prefix(&rooted_prefix))
            .or_else(|| href.strip_prefix(&absolute_prefix));
        let Some(file) = filename.and_then(|name| parse_imgw_cmax_filename(site, name)) else {
            continue;
        };
        let quantities = cycles.entry(file.observed_at).or_default();
        match quantities.get(&file.quantity) {
            Some(current) if current.product_counter >= file.product_counter => {}
            _ => {
                quantities.insert(file.quantity, file);
            }
        }
    }

    cycles
        .into_iter()
        .map(|(observed_at, quantities)| {
            let files = quantities.into_values().collect::<Vec<_>>();
            let selected = files
                .iter()
                .map(|file| file.filename.as_str())
                .collect::<Vec<_>>()
                .join("+");
            let identity = format!(
                "imgw-polrad/{}/{}/{}",
                site.code(),
                observed_at.format("%Y%m%d%H%M%S"),
                selected
            );
            ImgwCmaxCycle {
                site,
                observed_at,
                identity,
                files,
            }
        })
        .collect()
}

/// Fetch and parse up to `max_cycles` newest cycles, returned oldest-first.
///
/// This performs one listing POST and downloads no HDF5 object bytes.
pub fn imgw_polrad_recent_cycles(
    site: ImgwPolradSite,
    max_cycles: usize,
) -> Result<Vec<ImgwCmaxCycle>, String> {
    if max_cycles == 0 {
        return Ok(Vec::new());
    }
    let path = site.cmax_path();
    let response = crate::metadata_http_client()
        .post(IMGW_DATASTORE_LIST_URL)
        .header(ACCEPT, "text/html,*/*")
        .header(REFERER, IMGW_DATASTORE_URL)
        .form(&[("productType", "oper"), ("path", path.as_str())])
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| {
            format!(
                "IMGW POLRAD {} listing: {}",
                site.code(),
                crate::reqwest_error_chain(&err)
            )
        })?;
    let html = crate::read_response_text_limited(
        response,
        crate::MAX_LISTING_TEXT_BYTES,
        "IMGW POLRAD listing",
    )
    .map_err(|err| format!("IMGW POLRAD {} listing: {err}", site.code()))?;
    let cycles = parse_imgw_cmax_listing(site, &html);
    if cycles.is_empty() {
        return Err(format!(
            "IMGW POLRAD {} listing contained no valid CMAX files",
            site.code()
        ));
    }
    Ok(limit_recent_cycles(cycles, max_cycles))
}

/// Fetch the newest cycle currently present in the site's listing.
pub fn imgw_polrad_latest_cycle(site: ImgwPolradSite) -> Result<ImgwCmaxCycle, String> {
    imgw_polrad_recent_cycles(site, 1)?
        .pop()
        .ok_or_else(|| format!("IMGW POLRAD {} has no current CMAX cycle", site.code()))
}

fn limit_recent_cycles(mut cycles: Vec<ImgwCmaxCycle>, max_cycles: usize) -> Vec<ImgwCmaxCycle> {
    let keep_from = cycles.len().saturating_sub(max_cycles);
    cycles.split_off(keep_from)
}

/// Extract href attribute values without treating anchor text as a file name.
/// IMGW's labels currently match their links, but the href is the fetch
/// contract and remains safe if the portal later truncates display text.
fn href_values(mut html: &str) -> Vec<&str> {
    let mut hrefs = Vec::new();
    while let Some(at) = html.find("href=") {
        html = &html[at + "href=".len()..];
        let Some(first) = html.chars().next() else {
            break;
        };
        if first == '"' || first == '\'' {
            let body = &html[first.len_utf8()..];
            let Some(end) = body.find(first) else {
                break;
            };
            hrefs.push(&body[..end]);
            html = &body[end + first.len_utf8()..];
        } else {
            let end = html
                .find(|ch: char| ch.is_whitespace() || ch == '>')
                .unwrap_or(html.len());
            hrefs.push(&html[..end]);
            html = &html[end..];
        }
    }
    hrefs
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAM_LISTING: &str = include_str!("fixtures/imgw_ram_cmax_listing.html");

    #[test]
    fn polrad_site_catalog_has_current_ten_sites_and_metadata() {
        assert_eq!(IMGW_POLRAD_SITES.len(), 10);
        assert_eq!(
            ImgwPolradSite::from_code("GDY"),
            Some(ImgwPolradSite::NowyGdansk)
        );
        assert_eq!(ImgwPolradSite::NowyGdansk.label(), "Nowy Gdańsk");
        assert_eq!(ImgwPolradSite::GoraSwietejAnny.system_code(), "GSA");
        assert_eq!(
            ImgwPolradSite::Uzranki.location(),
            (53.855_733, 21.412_331, 237.0)
        );
        assert_eq!(
            ImgwPolradSite::Brzuchania.cmax_path(),
            "/Oper/Polrad/Produkty/HVD/HVD_brz_250.max"
        );
    }

    #[test]
    fn cmax_filename_parser_separates_seconds_from_product_counter() {
        let file = parse_imgw_cmax_filename(ImgwPolradSite::Ramza, "2026071100150601PhiDP.max.h5")
            .expect("valid IMGW file");
        assert_eq!(file.observed_at.to_rfc3339(), "2026-07-11T00:15:06+00:00");
        assert_eq!(file.product_counter, 1);
        assert_eq!(file.quantity, ImgwPolradQuantity::PhiDp);
        assert_eq!(file.quantity.odim_quantity(), "PHIDP");
        assert_eq!(
            file.download_url,
            "https://danepubliczne.imgw.pl/pl/datastore/getfiledown/Oper/Polrad/Produkty/HVD/HVD_ram_250.max/2026071100150601PhiDP.max.h5"
        );
        assert_eq!(
            file.identity,
            "imgw-polrad/ram/2026071100150601PhiDP.max.h5"
        );
    }

    #[test]
    fn cmax_filename_parser_rejects_unsafe_or_unknown_names() {
        let site = ImgwPolradSite::Ramza;
        for invalid in [
            "../../2026071100150601KDP.max.h5",
            "2026071100150601KDP.max.h5?download=1",
            "2026131100150601KDP.max.h5",
            "2026071100159961KDP.max.h5",
            "2026071100150601RHOHV.max.h5",
            "2026071100150601dBZ.max.h5",
            "2026071100150601KDP.max.h5.png",
        ] {
            assert!(
                parse_imgw_cmax_filename(site, invalid).is_none(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn listing_groups_ram_counter_variants_without_splitting_cycle() {
        let cycles = parse_imgw_cmax_listing(ImgwPolradSite::Ramza, RAM_LISTING);
        assert_eq!(cycles.len(), 2);
        let first = &cycles[0];
        assert_eq!(first.observed_at.to_rfc3339(), "2026-07-11T00:15:06+00:00");
        assert_eq!(first.files.len(), 4);
        assert_eq!(
            first
                .file(ImgwPolradQuantity::Kdp)
                .map(|file| file.filename.as_str()),
            Some("2026071100150601KDP.max.h5")
        );
        assert_eq!(
            first
                .file(ImgwPolradQuantity::RhoHv)
                .map(|file| file.product_counter),
            Some(1)
        );
        assert!(first.file(ImgwPolradQuantity::PhiDp).is_some());
        assert_eq!(
            cycles[1].observed_at.to_rfc3339(),
            "2026-07-11T00:20:07+00:00"
        );
    }

    #[test]
    fn listing_accepts_only_exact_site_download_paths() {
        let cycles = parse_imgw_cmax_listing(ImgwPolradSite::Ramza, RAM_LISTING);
        assert!(cycles.iter().all(|cycle| {
            cycle
                .files
                .iter()
                .all(|file| file.site == ImgwPolradSite::Ramza)
        }));
        assert!(cycles.iter().all(|cycle| {
            cycle.files.iter().all(|file| {
                !file.filename.contains("BRZ")
                    && !file.filename.contains("png")
                    && !file.filename.contains('/')
            })
        }));
    }

    #[test]
    fn recent_limit_keeps_newest_cycles_in_chronological_order() {
        let cycles = parse_imgw_cmax_listing(ImgwPolradSite::Ramza, RAM_LISTING);
        let recent = limit_recent_cycles(cycles, 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].observed_at.to_rfc3339(),
            "2026-07-11T00:20:07+00:00"
        );
        assert!(recent[0].identity.contains("0701ZDR.max.h5"));
        assert_eq!(IMGW_ATTRIBUTION_REQUIREMENTS.terms_url, IMGW_DATA_TERMS_URL);
        assert_eq!(
            IMGW_ATTRIBUTION_REQUIREMENTS.source_notice_pl,
            IMGW_SOURCE_NOTICE_PL
        );
        assert_eq!(
            IMGW_ATTRIBUTION_REQUIREMENTS.processed_notice_pl,
            IMGW_PROCESSED_NOTICE_PL
        );
    }

    /// Listing-only live proof. The request downloads the portal's HTML
    /// fragment and constructs plans; it deliberately fetches no HDF5 body.
    #[test]
    #[ignore = "network: lists current IMGW RAM CMAX files without downloading them"]
    fn imgw_polrad_live_listing_builds_current_download_plans() {
        let cycles =
            imgw_polrad_recent_cycles(ImgwPolradSite::Ramza, 2).expect("live IMGW RAM listing");
        assert!(!cycles.is_empty() && cycles.len() <= 2);
        assert!(
            cycles
                .windows(2)
                .all(|pair| pair[0].observed_at < pair[1].observed_at)
        );
        let newest = cycles.last().expect("newest IMGW cycle");
        assert_eq!(newest.site, ImgwPolradSite::Ramza);
        assert!(newest.file(ImgwPolradQuantity::Kdp).is_some());
        assert!(newest.files.iter().all(|file| {
            file.download_url.starts_with(IMGW_DATASTORE_DOWNLOAD_BASE)
                && file.identity.starts_with("imgw-polrad/ram/")
        }));
    }
}
