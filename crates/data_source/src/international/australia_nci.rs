//! Australian weather radar volumes from NCI's `rq0` THREDDS archive.
//!
//! The catalog at <https://thredds.nci.org.au/thredds/catalog/rq0/catalog.html>
//! exposes Bureau of Meteorology / Monash ODIM HDF5 polar volumes by site,
//! year, and UTC date. Each day has a huge `*.pvol.zip`, but THREDDS can serve
//! individual members directly:
//!
//! `.../vol/{site}_{yyyymmdd}.pvol.zip/{site}_{yyyymmdd}_{hhmmss}.pvol.h5`
//!
//! BowEcho therefore never downloads the daily ZIP. It reads the small daily
//! `*_tarlist.txt`, picks one or more HDF5 member names, and hands those direct
//! member URLs to the existing ODIM decoder.

use chrono::{Days, NaiveDate, NaiveDateTime, Utc};

use super::{FramePlan, IntlProvider, IntlSite, PlanPart, SiteCache};

const BASE: &str = "https://thredds.nci.org.au/thredds/fileServer/rq0";
const SITE_LIST_URL: &str = "https://thredds.nci.org.au/thredds/fileServer/rq0/radar_site_list.csv";
const LATEST_LOOKBACK_DAYS: u64 = 14;
const RECENT_LOOKBACK_DAYS: u64 = 7;
const MAX_RECENT_FRAMES: usize = 96;

const STATIC_SITES: &[(&str, &str, &str, f32, f32)] = &[
    ("2", "Melbourne", "Melb", -37.8553, 144.7554),
    ("3", "Wollongong", "Wollgng", -34.2625, 150.8752),
    ("4", "Newcastle", "LemnTre", -32.7298, 152.0254),
    ("5", "Carnarvon", "Carnvn", -24.8879, 113.6693),
    ("6", "Geraldton", "Gerlton", -28.8044, 114.6972),
    ("7", "Wyndham", "Wyndham", -15.4517, 128.1209),
    ("8", "Gympie", "Kanign", -25.9574, 152.577),
    ("10", "Darwin AP", "Darwin", -12.4247, 130.8919),
    ("14", "Mt Gambier", "Gambier", -37.7477, 140.7746),
    ("15", "Dampier", "Dampier", -20.6535, 116.6833),
    ("16", "Port Hedland", "PHedld", -20.3719, 118.6317),
    ("17", "Broome", "Broome", -17.9483, 122.2353),
    ("19", "Cairns", "Cairns", -16.8182, 145.6628),
    ("22", "Mackay", "Mackay", -21.1173, 149.2173),
    ("23", "Gladstone", "Gladstn", -23.8550, 151.2626),
    ("24", "Bowen", "Bowen", -19.8857, 148.0756),
    ("25", "Alice Springs", "AliceSp", -23.7950, 133.8889),
    ("26", "Perth AP", "PrthAP", -31.9273, 115.9756),
    ("27", "Woomera", "Woomera", -31.1558, 136.8044),
    ("28", "Grafton", "Grafton", -29.6206, 152.9633),
    ("29", "Learmonth", "Lrmonth", -22.1032, 113.9997),
    ("31", "Albany", "Albany", -34.9418, 117.8163),
    ("32", "Esperance", "Esprnce", -33.8303, 121.8917),
    ("33", "Ceduna", "Ceduna", -32.1298, 133.6963),
    (
        "36",
        "Gulf of Carpentaria (Mornington Island)",
        "GlfCarp",
        -16.6640,
        139.1812,
    ),
    ("37", "Hobart Airport", "Hobart", -42.8374, 147.5008),
    ("38", "Newdegate", "Ndegate", -33.0970, 119.0087),
    ("39", "Halls Creek", "HallsCk", -18.2289, 127.6628),
    (
        "40",
        "Canberra (Captains Flat)",
        "CapFlat",
        -35.6614,
        149.5122,
    ),
    ("41", "Willis Island", "Willis", -16.2874, 149.9646),
    ("42", "Katherine (Tindal)", "Tindal", -14.5124, 132.4431),
    ("44", "Giles", "Giles", -25.0332, 128.3017),
    (
        "46",
        "Adelaide (Sellicks Hill)",
        "Sellick",
        -35.3295,
        138.5024,
    ),
    ("48", "Kalgoorlie", "Kgrlie", -30.7843, 121.4549),
    ("49", "Yarrawonga", "NEVic", -36.0297, 146.0228),
    ("50", "Brisbane (Marburg)", "Marburg", -27.6063, 152.5401),
    (
        "52",
        "N.W. Tasmania (West Takone)",
        "WTakone",
        -41.1791,
        145.58,
    ),
    ("53", "Moree", "Moree", -29.4903, 149.8462),
    ("54", "Sydney (Kurnell)", "Kurnell", -34.0148, 151.2263),
    ("55", "Wagga Wagga", "Wagga", -35.1582, 147.4563),
    ("56", "Longreach", "Longrch", -23.4398, 144.2822),
    ("58", "South Doodlakine", "SthDood", -31.7778, 117.9528),
    ("63", "Darwin (Berrimah)", "Berrima", -12.4559, 130.9265),
    (
        "64",
        "Adelaide (Buckland Park)",
        "BuckPk",
        -34.6169,
        138.4689,
    ),
    ("66", "Brisbane (Mt Stapylton)", "MtStapl", -27.7178, 153.24),
    ("67", "Warrego", "Warrego", -26.4400, 147.3492),
    ("68", "Bairnsdale", "Bnsdale", -37.8876, 147.5755),
    (
        "69",
        "Namoi (Blackjack Mountain)",
        "Namoi",
        -31.0242,
        150.1919,
    ),
    ("70", "Perth (Serpentine)", "Serptin", -32.3917, 115.867),
    ("71", "Sydney (Terrey Hills)", "THills", -33.7008, 151.2094),
    ("72", "Emerald", "Emerald", -23.5498, 148.2392),
    (
        "73",
        "Townsville (Hervey Range)",
        "HrvyRng",
        -19.4198,
        146.5509,
    ),
    ("74", "Greenvale", "Grnvale", -18.9976, 144.9959),
    ("75", "Mount Isa", "MntIsa", -20.7112, 139.5552),
    ("76", "Hobart (Mt Koonya)", "Koonya", -43.1126, 147.8052),
    ("77", "Warruwi", "Arafura", -11.6485, 133.38),
    ("78", "Weipa", "Weipa78", -12.6664, 141.9247),
    ("79", "Watheroo", "Wathroo", -30.3600, 116.2896),
    ("93", "Brewarrina", "Brewarr", -29.9708, 146.8136),
    ("94", "Hillston", "Hillston", -33.5520, 145.5286),
    ("95", "Rainbow (Wimmera)", "Rainbow", -35.9976, 142.0133),
    ("96", "Yeoval", "Yeoval", -32.7444, 148.7081),
    ("97", "Mildura", "Mild_DP", -34.2871, 141.5982),
    ("98", "Taroom", "Taroom", -25.6962, 149.8982),
    ("105", "Meteopress C-band", "BrisAP", -27.3915, 153.13),
    ("106", "Townsville", "Townsville", -19.4198, 146.5509),
    ("107", "Richmond", "Rchmond", -20.7518, 143.1414),
    ("108", "Toowoomba", "Toowoomba", -27.2740, 151.993),
    ("111", "Karratha", "Karratha", -20.9924, 116.8758),
    ("112", "Gove", "Gove", -12.2750, 136.8199),
    (
        "114",
        "Carnarvon (Gascoyne)",
        "Carnarvon",
        -24.8879,
        113.6693,
    ),
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct TarlistFrame {
    file_name: String,
    site_id: String,
    date: NaiveDate,
    timestamp: NaiveDateTime,
}

/// Australia NCI: direct ODIM HDF5 member reads from the `rq0` THREDDS
/// archive. This is archive-lagged, not guaranteed true live BOM radar.
pub struct AustraliaNciProvider {
    sites: SiteCache,
}

impl AustraliaNciProvider {
    pub fn new() -> Self {
        Self {
            sites: SiteCache::new(),
        }
    }
}

impl Default for AustraliaNciProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl IntlProvider for AustraliaNciProvider {
    fn id(&self) -> &'static str {
        "australia-nci"
    }

    fn label(&self) -> &'static str {
        "NCI Australia Radar"
    }

    fn country(&self) -> &'static str {
        "Australia"
    }

    fn list_sites(&self) -> Result<Vec<IntlSite>, String> {
        self.sites.get_or_fill(|| {
            let csv = crate::fetch_text(SITE_LIST_URL)
                .map_err(|err| format!("Australia NCI site list {SITE_LIST_URL}: {err}"))?;
            let sites = parse_site_csv(&csv);
            if sites.is_empty() {
                return Err("Australia NCI site list parsed no current radar sites".to_owned());
            }
            Ok(sites)
        })
    }

    fn latest(&self, site_id: &str) -> Result<FramePlan, String> {
        validate_site_id(site_id)?;
        latest_frame(site_id).map(|frame| frame_plan(&frame)).ok_or_else(|| {
            format!(
                "Australia NCI: no tarlist frames for site {site_id} in the last {LATEST_LOOKBACK_DAYS} UTC days"
            )
        })
    }

    fn recent(&self, site_id: &str, count: usize) -> Result<Vec<FramePlan>, String> {
        validate_site_id(site_id)?;
        let count = count.clamp(1, MAX_RECENT_FRAMES);
        let mut frames = recent_frames(site_id, count);
        if frames.is_empty() {
            return Err(format!(
                "Australia NCI: no recent tarlist frames for site {site_id} in the last {RECENT_LOOKBACK_DAYS} UTC days"
            ));
        }
        frames.sort_by_key(|frame| frame.timestamp);
        let skip = frames.len().saturating_sub(count);
        Ok(frames[skip..].iter().map(frame_plan).collect())
    }

    fn static_sites(&self) -> Vec<IntlSite> {
        static_sites()
    }
}

fn static_sites() -> Vec<IntlSite> {
    STATIC_SITES
        .iter()
        .map(
            |&(id, label, short_name, latitude_deg, longitude_deg)| IntlSite {
                provider_id: "australia-nci",
                site_id: id.to_owned(),
                label: site_label(label, short_name, id),
                country: "Australia",
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            },
        )
        .collect()
}

fn site_label(label: &str, short_name: &str, site_id: &str) -> String {
    if short_name.is_empty() || short_name == label {
        format!("{label} ({site_id})")
    } else {
        format!("{label} / {short_name} ({site_id})")
    }
}

fn parse_site_csv(csv: &str) -> Vec<IntlSite> {
    let mut lines = csv.lines().filter(|line| !line.trim().is_empty());
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let headers = parse_csv_record(header);
    let index = |name: &str| headers.iter().position(|header| header == name);
    let (
        Some(id_idx),
        Some(short_idx),
        Some(location_idx),
        Some(status_idx),
        Some(lat_idx),
        Some(lon_idx),
        Some(end_idx),
        Some(notes_idx),
    ) = (
        index("id"),
        index("short_name"),
        index("location"),
        index("status"),
        index("site_lat"),
        index("site_lon"),
        index("prechange_end"),
        index("notes"),
    )
    else {
        return Vec::new();
    };

    let mut sites: Vec<_> = lines
        .filter_map(|line| {
            let row = parse_csv_record(line);
            let field = |idx: usize| row.get(idx).map(String::as_str).unwrap_or("").trim();
            let id = field(id_idx);
            let short_name = field(short_idx);
            let label = field(location_idx);
            let status = field(status_idx);
            let latitude_deg = field(lat_idx).parse::<f32>().ok()?;
            let longitude_deg = field(lon_idx).parse::<f32>().ok()?;
            if id.is_empty()
                || label.is_empty()
                || !matches!(status, "OK" | "CHECK")
                || field(end_idx) != "-"
                || field(notes_idx)
                    .to_ascii_lowercase()
                    .contains("not operational")
                || !latitude_deg.is_finite()
                || !longitude_deg.is_finite()
            {
                return None;
            }
            Some(IntlSite {
                provider_id: "australia-nci",
                site_id: id.to_owned(),
                label: site_label(label, short_name, id),
                country: "Australia",
                latitude_deg: Some(latitude_deg),
                longitude_deg: Some(longitude_deg),
            })
        })
        .collect();
    sites.sort_by_key(|site| site.site_id.parse::<u32>().unwrap_or(u32::MAX));
    sites
}

fn parse_csv_record(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(field.trim().to_owned());
                field.clear();
            }
            _ => field.push(ch),
        }
    }
    fields.push(field.trim().to_owned());
    fields
}

fn latest_frame(site_id: &str) -> Option<TarlistFrame> {
    candidate_dates(LATEST_LOOKBACK_DAYS)
        .into_iter()
        .filter_map(|date| tarlist_frames(site_id, date).ok())
        .flatten()
        .max_by_key(|frame| frame.timestamp)
}

fn recent_frames(site_id: &str, count: usize) -> Vec<TarlistFrame> {
    let mut frames = Vec::new();
    for date in candidate_dates(RECENT_LOOKBACK_DAYS).into_iter().rev() {
        if let Ok(day_frames) = tarlist_frames(site_id, date) {
            frames.extend(day_frames);
        }
        if frames.len() >= count {
            break;
        }
    }
    frames
}

fn candidate_dates(lookback_days: u64) -> Vec<NaiveDate> {
    let today = Utc::now().date_naive();
    (0..lookback_days)
        .filter_map(|offset| today.checked_sub_days(Days::new(offset)))
        .collect()
}

fn tarlist_frames(site_id: &str, date: NaiveDate) -> Result<Vec<TarlistFrame>, String> {
    let url = tarlist_url(site_id, date);
    let text = crate::fetch_text(&url)
        .map_err(|err| format!("Australia NCI tarlist {site_id} {date}: {err}"))?;
    let frames = parse_tarlist(&text, site_id)
        .map_err(|err| format!("Australia NCI tarlist {site_id} {date}: {err}"))?;
    Ok(frames
        .into_iter()
        .filter(|frame| frame.date == date)
        .collect())
}

fn parse_tarlist(text: &str, site_id: &str) -> Result<Vec<TarlistFrame>, String> {
    let mut frames = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = parse_csv_record(line);
        let Some(file_name) = fields.first().map(String::as_str) else {
            continue;
        };
        let row_site = fields.get(1).map(String::as_str).unwrap_or(site_id);
        if row_site != site_id || !file_name.ends_with(".pvol.h5") {
            continue;
        }
        let stamp = fields
            .get(2)
            .map(String::as_str)
            .or_else(|| stamp_from_file_name(file_name))
            .ok_or_else(|| format!("missing timestamp for {file_name}"))?;
        let timestamp = NaiveDateTime::parse_from_str(stamp, "%Y%m%d_%H%M%S")
            .map_err(|err| format!("bad timestamp '{stamp}' for {file_name}: {err}"))?;
        frames.push(TarlistFrame {
            file_name: file_name.to_owned(),
            site_id: site_id.to_owned(),
            date: timestamp.date(),
            timestamp,
        });
    }
    frames.sort_by_key(|frame| frame.timestamp);
    Ok(frames)
}

fn stamp_from_file_name(file_name: &str) -> Option<&str> {
    let body = file_name.strip_suffix(".pvol.h5")?;
    let stamp = body.rsplit_once('_')?.1;
    (stamp.len() == 6 && stamp.bytes().all(|byte| byte.is_ascii_digit())).then_some(stamp)?;
    let (_, date_and_time) = body.split_once('_')?;
    Some(date_and_time)
}

fn frame_plan(frame: &TarlistFrame) -> FramePlan {
    FramePlan {
        identity: format!("australia-nci_{}_{}", frame.site_id, frame.file_name),
        parts: vec![PlanPart {
            url: frame_url(frame),
        }],
        merge: false,
    }
}

fn tarlist_url(site_id: &str, date: NaiveDate) -> String {
    let yyyymmdd = date.format("%Y%m%d");
    format!(
        "{BASE}/{site_id}/{}/list/{site_id}_{yyyymmdd}_tarlist.txt",
        date.format("%Y")
    )
}

fn frame_url(frame: &TarlistFrame) -> String {
    let yyyymmdd = frame.date.format("%Y%m%d");
    format!(
        "{BASE}/{}/{}/vol/{}_{}.pvol.zip/{}",
        frame.site_id,
        frame.date.format("%Y"),
        frame.site_id,
        yyyymmdd,
        frame.file_name
    )
}

fn validate_site_id(site_id: &str) -> Result<(), String> {
    if !site_id.is_empty() && site_id.bytes().all(|byte| byte.is_ascii_digit()) {
        Ok(())
    } else {
        Err(format!("Australia NCI: invalid site id '{site_id}'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE_CSV: &str = "\
id,id_long,WIGOS,short_name,location,radar_type,postchange_start,prechange_end,site_lat,site_lon,ge_ground_altitude,site_alt,status,band,doppler,dp,eth_dhz_threshold,beamwidth,state,notes
1,1,,CampRd,Broadmeadows,,,-,-37.6899,144.9472,0,0,OK,S,F,F,,1.67,VIC,not operational
2,2,,Melb,Melbourne,,,-,-37.8553,144.7554,0,0,OK,S,T,T,,1.67,VIC,
42,42,,Tindal,Katherine (Tindal),,,-,-14.5124,132.4431,0,0,CHECK,S,T,T,,1.67,NT,
200,200,,Old,Old Radar,,,2020-01-01,-30.0,130.0,0,0,OK,S,T,T,,1.67,WA,
";

    const TARLIST: &str = "\
#fname,r_id,time_utc,file_sz_kb,n_tilts,v_res,gate_m,beamw_deg,ppi_elv_list,num_2nd_tilt_rays_removed,2nd_tilt_low_refl_area_km,2nd_tilt_high_refl_area_km
2_20260624_235500.pvol.h5,2,20260624_235500,3535,13,160,250,1.67,0.5;0.8,-999,586,2
2_20260625_000000.pvol.h5,2,20260625_000000,3520,13,160,250,1.67,0.5;0.8,-999,586,2
3_20260625_000000.pvol.h5,3,20260625_000000,3520,13,160,250,1.67,0.5;0.8,-999,586,2
";

    #[test]
    fn site_csv_parser_keeps_current_operational_sites() {
        let sites = parse_site_csv(SITE_CSV);
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].provider_id, "australia-nci");
        assert_eq!(sites[0].site_id, "2");
        assert_eq!(sites[0].label, "Melbourne / Melb (2)");
        assert_eq!(sites[1].site_id, "42");
        assert!(sites[1].label.contains("Tindal"));
    }

    #[test]
    fn tarlist_parser_filters_site_and_sorts_frames() {
        let frames = parse_tarlist(TARLIST, "2").expect("tarlist");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file_name, "2_20260624_235500.pvol.h5");
        assert_eq!(frames[1].file_name, "2_20260625_000000.pvol.h5");
        assert_eq!(frames[1].date.to_string(), "2026-06-25");
    }

    #[test]
    fn frame_plan_uses_direct_zip_member_url() {
        let frame = parse_tarlist(TARLIST, "2").expect("tarlist").remove(1);
        let plan = frame_plan(&frame);
        assert_eq!(plan.identity, "australia-nci_2_2_20260625_000000.pvol.h5");
        assert_eq!(plan.parts.len(), 1);
        assert_eq!(
            plan.parts[0].url,
            "https://thredds.nci.org.au/thredds/fileServer/rq0/2/2026/vol/2_20260625.pvol.zip/2_20260625_000000.pvol.h5"
        );
        assert!(!plan.merge);
    }

    #[test]
    fn provider_static_sites_include_australian_network() {
        let provider = AustraliaNciProvider::new();
        let sites = provider.static_sites();
        assert!(sites.len() > 60);
        assert!(sites.iter().any(|site| site.site_id == "2"));
        assert!(sites.iter().any(|site| site.site_id == "71"));
    }

    #[test]
    #[ignore = "live NCI THREDDS catalog probe"]
    fn live_latest_melbourne_resolves_member_hdf5() {
        let provider = AustraliaNciProvider::new();
        let plan = provider
            .latest("2")
            .expect("latest Melbourne archive frame");
        assert_eq!(plan.parts.len(), 1);
        assert!(plan.parts[0].url.contains(".pvol.zip/"));
        assert!(plan.parts[0].url.ends_with(".pvol.h5"));
    }

    #[test]
    #[ignore = "live NCI THREDDS download/decode probe"]
    fn live_melbourne_member_hdf5_decodes_through_router() {
        let provider = AustraliaNciProvider::new();
        let plan = provider
            .latest("2")
            .expect("latest Melbourne archive frame");
        let raw = crate::fetch_volume_bytes(&plan.parts[0].url).expect("download ODIM HDF5 member");
        let volume =
            nexrad_io::decode_supported_volume_bytes(&raw).expect("ODIM HDF5 member decode");
        assert!(
            volume.cuts.iter().any(|cut| !cut.moments.is_empty()),
            "decoded volume should contain moments"
        );
    }
}
