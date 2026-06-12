//! Observed (radiosonde) soundings, rendered through the SAME native
//! skew-T pipeline as model soundings — full sharprs parameter suites on
//! real RAOB launches.
//!
//! Two archives of the same provider family, in preference order:
//!
//! 1. University of Wyoming upper-air archive
//!    (<https://weather.uwyo.edu/upperair/sounding.html>, CGI:
//!    `cgi-bin/sounding?TYPE=TEXT:LIST&YEAR=&MONTH=&FROM=DDHH&TO=DDHH&
//!    STNM=wmo`): serves EVERY transmitted launch — synoptic 00/12z AND
//!    off-hour specials (verified: ILX 21z 11 Jun 2026, BMX 18z 27 Apr
//!    2011, OUN 18z 03 May 1999) — decades deep (per-station
//!    commissioning limits). One windowed request returns every launch
//!    in the range, so the launch nearest the displayed frame is found
//!    without probing hour by hour.
//! 2. Iowa Environmental Mesonet RAOB archive (JSON, no key): site list
//!    from the RAOB network GeoJSON, profiles from `json/raob.py` at the
//!    synoptic hours. Synoptic-only (00/12z; no specials — verified
//!    empty for the launches above) but reaches 1940s starts for legacy
//!    stations. Used when a site has no WMO number or Wyoming fails.
//!
//! Archive-aware: callers pass the displayed frame's time and get the
//! launch nearest BEFORE it (+90 min grace for transmission), specials
//! included.
//!
//! (ACARS/AMDAR aircraft profiles are MADIS-gated — the public GSL text
//! server is gone — so aircraft soundings wait on a bring-your-own
//! credentials integration.)

use std::sync::OnceLock;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};

#[derive(Clone, Debug, PartialEq)]
pub struct RaobSite {
    /// IEM/ICAO-style id ("KILX") — what `json/raob.py` accepts.
    pub id: String,
    /// Human station name ("Lincoln IL/US") for marker hover chips.
    pub name: String,
    pub lat: f32,
    pub lon: f32,
    /// WMO synoptic number (74560) — what the Wyoming CGI's STNM wants.
    /// None: IEM-only station.
    pub synop: Option<u32>,
}

/// The launch-site catalog embedded at build time so RAOB markers draw
/// (and nearest-site picks resolve) without any network: every
/// `online: true` station of the IEM RAOB network metadata
/// (<https://mesonet.agron.iastate.edu/geojson/network/RAOB.geojson>,
/// captured 2026-06-12) — the US complement plus the Canada / Mexico /
/// Caribbean / Pacific launch sites both archives serve, each with its
/// WMO number. (id, name, lat, lon, wmo synop)
const RAOB_STATIC_TABLE: &[(&str, &str, f32, f32, u32)] = &[
    ("CWMJ", "MANIWAKI", 46.38, -75.9, 71722),
    ("CWPL", "Pickle Lake ON/CN", 51.47, -90.2, 71845),
    ("CWQI", "Yarmouth NC/CN", 43.83, -66.0, 71603),
    ("CWSE", "Edmonton/Stony_plai AB/CN", 53.55, -113.9, 71119),
    ("CYAH", "La Grande Iv Arp QB/CN", 53.75, -73.6, 71823),
    ("CYBK", "Baker Lake NT/CN", 64.3, -96.0, 71926),
    ("CYCB", "Cambridge Bay NT/CN", 69.1, -105.1, 71925),
    ("CYEU", "Eureka (Man) CN/CN", 80.0, -85.8, 71917),
    ("CYEV", "Inuvik NT/CN", 68.3, -133.4, 71957),
    ("CYJT", "Stephenville NF/CN", 48.53, -58.5, 71815),
    ("CYLT", "Alert Airport NT/CN", 82.52, -62.2, 71082),
    ("CYLW", "Kelowna Apt BC/CN", 49.97, -119.3, 71203),
    ("CYMO", "Moosonee A ON/CN", 51.27, -80.6, 71836),
    ("CYPH", "Inukjuak QB/CN", 58.47, -78.0, 71907),
    ("CYQD", "The Pas MN/CN", 53.97, -101.1, 71867),
    ("CYRB", "Resolute NT/CN", 74.72, -94.9, 71924),
    ("CYSA", "Sable Island NS/CN", 43.93, -60.0, 71600),
    ("CYSM", "Fort Smith NT/CN", 60.02, -111.9, 71934),
    ("CYUX", "Hall Beach Airpo NT/CN", 68.78, -81.2, 71081),
    ("CYVN", "Cape Dyer Airpor NT/CN", 66.58, -61.6, 71094),
    ("CYVP", "Kuujjuaq QB/CN", 58.1, -68.4, 71906),
    ("CYVQ", "Norman Wells NT/CN", 65.28, -126.8, 71043),
    ("CYXY", "WHITEHORSE", 60.72, -135.0, 71964),
    ("CYYE", "Fort Nelson BC/CN", 58.83, -122.5, 71945),
    ("CYYQ", "Churchill MN/CN", 58.75, -94.0, 71913),
    ("CYYR", "Goose Bay NF/CN", 53.32, -60.4, 71816),
    ("CYZS", "Coral Harbour Ar NT/CN", 64.2, -83.3, 71915),
    ("CYZT", "Port Hardy BC/CN", 50.68, -127.3, 71109),
    ("CYZV", "Sept Iles QB/CN", 50.22, -66.2, 71811),
    ("CZXS", "Prince George (RUC) BC/CN", 53.9, -122.8, 71908),
    ("K1Y7", "Yuma Prvg Ground AZ/US", 32.5, -114.4, 74004),
    ("KABQ", "Albuquerq/Krtlnd NM/US", 35.05, -106.6, 72365),
    ("KABR", "Aberdeen", 45.45, -98.4, 72659),
    ("KALY", "Albany NY/US", 42.75, -72.2, 72518),
    ("KAMA", "Amarillo Arpt TX/US", 35.23, -101.7, 72363),
    ("KAPG", "Phillips Aaf/Abe MD/US", 39.47, -76.1, 74002),
    ("KAPX", "Alpena MI/US", 45.07, -82.4, 72634),
    ("KBIS", "Bismarck Municip ND/US", 46.77, -100.7, 72764),
    ("KBMX", "Birmingham", 33.18, -86.78, 72230),
    ("KBNA", "Nashville Metro TN/US", 36.13, -86.6, 72327),
    ("KBOI", "Boise Arpt ID/US", 43.57, -116.2, 72681),
    ("KBRO", "Brownsville Intl TX/US", 25.9, -97.4, 72250),
    ("KBUF", "Buffalo Intl Arp NY/US", 42.93, -78.7, 72528),
    ("KCAR", "Caribou ME/US", 46.87, -68.0, 72712),
    ("KCHS", "Charleston Afb SC/US", 32.9, -80.0, 72208),
    ("KCRP", "Corpus Christi I TX/US", 27.77, -97.5, 72251),
    ("KDDC", "Dodge City", 37.77, -99.9, 72451),
    ("KDNR", "Denver / Staple CO/US", 39.78, -104.8, 72469),
    ("KDRT", "Del Rio Intl TX/US", 29.37, -100.9, 72261),
    ("KDTX", "Detroit MI/US", 42.7, -83.4, 72632),
    ("KDVN", "Davenport", 41.62, -90.5, 74455),
    ("KEDW", "Edwards Afb CA/US", 34.9, -117.8, 72381),
    ("KEFD", "Ellington/Houston", 29.6044, -95.1563, 12906),
    ("KEPZ", "Santa Teresa NM/US", 31.87, -106.7, 72364),
    ("KEYW", "Key West Intl Ar FL/US", 24.55, -81.7, 72201),
    ("KFFC", "Peachtre/Fal Fld GA/US", 33.37, -84.5, 72215),
    ("KFGZ", "Flgstf/Belemont AZ/US", 35.23, -111.8, 72376),
    ("KFWD", "Fort_worth TX/US", 32.79, -96.7, 72249),
    ("KGGW", "Glasgow Intl Arp MT/US", 48.22, -106.6, 72768),
    ("KGJT", "Grand Junction CO/US", 39.13, -108.5, 72476),
    ("KGRB", "Green Bay/Straub WI/US", 44.48, -88.1, 72645),
    ("KGSO", "Greensboro/Hi Pt NC/US", 36.08, -79.9, 72317),
    ("KGYX", "Gray", 43.8927, -70.257, 74389),
    ("KIAD", "Washington/Dulle VA/US", 38.95, -77.4, 72403),
    ("KILN", "Cncnati-Daytn OH/US", 39.43, -83.8, 72426),
    ("KILX", "Lincoln IL/US", 40.15, -88.6, 74560),
    ("KINL", "International Fa MN/US", 48.57, -93.3, 72747),
    ("KJAN", "Jackson/Thompson MS/US", 32.32, -90.0, 72235),
    ("KJAX", "Jacksonville Int FL/US", 30.5, -81.7, 72206),
    ("KLBF", "N. Platte/Lee Bi NE/US", 41.13, -100.6, 72562),
    ("KLCH", "Lake Charles Mun LA/US", 30.12, -93.2, 72240),
    ("KLKN", "Elko NV/US", 40.87, -114.2, 72582),
    ("KLMN", "Lamont", 36.62, -97.48, 74646),
    ("KLZK", "North Little Roc AR/US", 34.83, -92.2, 72340),
    ("KMAF", "Midland Regional TX/US", 31.95, -102.2, 72265),
    ("KMFL", "Miami FL/US", 25.75, -79.6, 72202),
    ("KMFR", "Medford/Jackson OR/US", 42.37, -122.8, 72597),
    ("KMHX", "Newport", 34.78, -76.88, 72305),
    ("KMPX", "Chanhassen MN/US", 44.83, -93.55, 72649),
    ("KNKX", "Miramar Nas CA/US", 32.85, -117.1, 72293),
    ("KNSI", "San Nicolas Is. CA/US", 33.23, -119.4, 72291),
    ("KOAK", "Oakland CA/US", 37.73, -122.2, 72493),
    ("KOAX", "Omaha / Valley", 41.32, -95.6, 72558),
    ("KOKX", "Brookhaven", 40.8656, -72.8645, 72501),
    ("KOTX", "Spokane WA/US", 47.7, -116.4, 72786),
    ("KOUN", "Norman OK/US", 35.22, -97.4, 72357),
    ("KPIT", "Pittsburgh Intl PA/US", 40.5, -80.2, 72520),
    ("KPSR", "Phoenix", 33.45, -111.95, 74626),
    ("KREV", "Reno NV/US", 39.57, -118.2, 72489),
    ("KRIW", "Riverton WY/US", 43.07, -108.4, 72672),
    ("KRNK", "Roanoke VA/US", 37.2, -79.5, 72318),
    ("KSGF", "Springfld Muni MO/US", 37.23, -93.3, 72440),
    ("KSHV", "Shreveport Regio LA/US", 32.47, -93.8, 72248),
    ("KSIL", "Slidell Radar S LA/US", 30.25, -89.7, 72233),
    ("KSLC", "Salt Lake City I UT/US", 40.78, -111.9, 72572),
    ("KSLE", "Salem/Mcnary OR/US", 44.92, -123.0, 72694),
    ("KTBW", "Tampa Bay Area FL/US", 27.7, -82.4, 72210),
    ("KTFX", "Great_falls MT/US", 47.46, -110.6, 72776),
    ("KTLH", "Tallahassee Rgnl FL/US", 30.4, -84.3, 72214),
    ("KTOP", "Topeka/Billard M KS/US", 39.07, -95.6, 72456),
    ("KTUS", "Tucson Intl Airp AZ/US", 32.12, -110.9, 72274),
    ("KUIL", "Quillayute State WA/US", 47.95, -124.5, 72797),
    ("KUNR", "Rapid City", 44.07, -102.8, 72662),
    ("KVBG", "Vandenberg Afb CA/US", 34.73, -120.5, 72393),
    ("KVEF", "Las Vegas", 36.047, -115.1845, 72388),
    ("KVPS", "Eglin Afb/Valpar FL/US", 30.48, -86.5, 72221),
    ("KWAL", "Wallops Is Stn VA/US", 37.93, -75.4, 72402),
    ("KXMR", "Cape Canaveral FL/US", 28.47, -80.5, 74794),
    ("KYUM", "Yuma Intl Airpor AZ/US", 32.65, -114.6, 72280),
    ("MDSD", "Santo Domingo Dom. Rep.", 18.43, -69.8, 78486),
    ("MMAN", "Monterrey Intl MX", 25.87, -100.2, 76394),
    ("MMGM", "EMPALME SONORA", 27.95, -110.8, 76256),
    ("MMLP", "LA PAZ/DE LEON", 24.07, -110.3, 76405),
    ("MMMX", "Mexico City MX", 19.43, -99.0, 76679),
    ("MMMZ", "MAZATLAN SINALOA", 23.18, -106.4, 76458),
    ("MMUN", "CANCUN (*)", 21.03, -86.9, 76595),
    ("MMVR", "VERACRUZ", 19.17, -96.1, 76692),
    ("MROC", "SAN JOSE/JUAN SANTA MARIA", 9.98, -84.2, 78762),
    ("MYNN", "Nassau Intl BA", 25.05, -77.4, 78073),
    ("PADQ", "KODIAK", 57.75, -152.4, 70350),
    ("PAKN", "KING SALMON", 58.68, -156.6, 70326),
    ("PANC", "ANCHORAGE IAP/PT. CAMPBE", 61.17, -150.0, 70273),
    ("PANN", "Anette Island AK/US", 55.03, -131.5, 70398),
    ("PASN", "Saint Paul Island", 57.15, -170.2167, 70308),
    ("PASY", "Shemya AFB", 52.7167, 174.1, 70414),
    ("PBET", "BETHEL", 60.78, -161.8, 70219),
    ("PBRW", "POINT BARROW", 71.3, -156.7, 70026),
    ("PCDB", "COLD BAY", 55.2, -162.7, 70316),
    ("PFAI", "FAIRBANKS", 64.82, -147.8, 70261),
    ("PHLI", "Lihue", 21.9933, -159.3467, 91165),
    ("PITO", "HILO", 19.72, -155.0, 91285),
    ("PMCG", "MCGRATH", 62.97, -155.6, 70231),
    ("POME", "NOME AP", 64.5, -165.4, 70200),
    ("POTZ", "KOTZEBUE", 66.87, -162.6, 70133),
    ("PYAK", "YAKUTAT", 59.52, -139.6, 70361),
    ("TBPB", "SEAWELL APT", 13.07, -59.5, 78954),
    ("TJSJ", "San Juan PU", 18.43, -66.0, 78526),
    ("TNCM", "SINT MARTIN/JULIANA", 18.05, -63.1, 78866),
    ("TTPP", "TRINIDAD/PIARCO IAP", 10.58, -61.3, 78970),
    ("TXKF", "Kindley FIELD BE", 32.37, -64.6, 78016),
];

/// The embedded launch-site catalog as [`RaobSite`]s — pure data, never
/// the network, safe to walk on the UI thread every frame (the
/// `intl_static_sites` convention). The live GeoJSON fetch
/// ([`fetch_sites`]) replaces this when it lands.
/// Coordinate truth overriding IEM's RAOB metadata where it is wrong by
/// more than ~20 km, from the NOAA IGRA2 station list
/// (<https://www.ncei.noaa.gov/pub/data/igra/igra2-station-list.txt>,
/// fetched 2026-06-12), keyed by WMO number. Worst upstream error:
/// "Alpena" MI placed 184 km into Lake Huron — the launch site is
/// Gaylord. Applied to BOTH the embedded snapshot and the live catalog
/// refresh, which carries the same upstream coordinates.
const IGRA2_COORD_OVERRIDES: &[(u32, f32, f32)] = &[
    (74004, 32.8356, -114.4),    // K1Y7 Yuma Proving Ground AZ
    (72518, 42.7500, -73.8000),  // KALY Albany NY
    (72634, 44.9075, -84.7189),  // KAPX Gaylord MI
    (72249, 32.8350, -97.2986),  // KFWD Fort Worth TX
    (74560, 40.1517, -89.3383),  // KILX Lincoln IL
    (72582, 40.8600, -115.7422), // KLKN Elko NV
    (72202, 25.7500, -80.3833),  // KMFL Miami FL
    (72558, 41.3200, -96.3669),  // KOAX Omaha/Valley NE
    (72786, 47.6806, -117.6266), // KOTX Spokane WA
    (72489, 39.5681, -119.7966), // KREV Reno NV
    (72318, 37.2039, -80.4142),  // KRNK Blacksburg VA
    (72776, 47.4614, -111.3847), // KTFX Great Falls MT
    (72662, 44.0730, -103.2102), // KUNR Rapid City SD
    (76679, 19.4037, -99.1966),  // MMMX Mexico City
];

/// IEM-catalog corrections shared by the embedded table and the live
/// refresh: IGRA2 coordinate overrides, and KEFD's bogus synop (IEM
/// carries Polish-block 12906 for Ellington; Wyoming's STNM=12906 is
/// empty, so the special-launch path would silently never work — keep
/// KEFD IEM-only).
fn apply_catalog_corrections(site: &mut RaobSite) {
    if site.id == "KEFD" {
        site.synop = None;
    }
    if let Some(wmo) = site.synop
        && let Some(&(_, lat, lon)) = IGRA2_COORD_OVERRIDES
            .iter()
            .find(|(candidate, ..)| *candidate == wmo)
    {
        site.lat = lat;
        site.lon = lon;
    }
}

pub fn static_sites() -> &'static [RaobSite] {
    static SITES: OnceLock<Vec<RaobSite>> = OnceLock::new();
    SITES.get_or_init(|| {
        RAOB_STATIC_TABLE
            .iter()
            .map(|&(id, name, lat, lon, synop)| {
                let mut site = RaobSite {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    lat,
                    lon,
                    synop: Some(synop),
                };
                apply_catalog_corrections(&mut site);
                site
            })
            .collect()
    })
}

/// Fetch the live RAOB site list (cached by the caller; the embedded
/// [`static_sites`] table covers until — and if — this lands). Filters
/// to currently-online launch stations, skipping IEM's "_XXX Area"
/// pseudo-sites (they alias real stations at the same pads).
pub fn fetch_sites() -> Result<Vec<RaobSite>, String> {
    let text =
        data_source::fetch_text("https://mesonet.agron.iastate.edu/geojson/network/RAOB.geojson")
            .map_err(|e| e.to_string())?;
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut sites = Vec::new();
    if let Some(features) = root["features"].as_array() {
        for f in features {
            let props = &f["properties"];
            let id = props["sid"].as_str().unwrap_or("");
            if id.is_empty() || id.starts_with('_') || props["online"] != true {
                continue;
            }
            let coords = &f["geometry"]["coordinates"];
            let (Some(lon), Some(lat)) = (coords[0].as_f64(), coords[1].as_f64()) else {
                continue;
            };
            let name = props["sname"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let mut site = RaobSite {
                id: id.to_owned(),
                name,
                lat: lat as f32,
                lon: lon as f32,
                synop: props["synop"].as_u64().map(|n| n as u32),
            };
            // The live catalog repeats the snapshot's upstream coordinate
            // errors — correct both the same way.
            apply_catalog_corrections(&mut site);
            sites.push(site);
        }
    }
    if sites.is_empty() {
        return Err("RAOB site list empty".to_owned());
    }
    Ok(sites)
}

/// The synoptic launch (00/12z) nearest BEFORE `when` (+90 min grace for
/// data arrival), then walk back up to 4 launches if a fetch is empty.
pub fn launch_times_before(when: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let adjusted = when + Duration::minutes(90);
    let mut t = adjusted
        .date_naive()
        .and_hms_opt(if adjusted.hour() >= 12 { 12 } else { 0 }, 0, 0)
        .unwrap()
        .and_utc();
    if t > adjusted {
        t -= Duration::hours(12);
    }
    (0..4).map(|i| t - Duration::hours(12 * i)).collect()
}

/// One raw sounding level before assembly: pres/hght/temp/dew required,
/// wind optional — the shared shape both archive decoders produce.
struct RawLevel {
    pres: f64,
    hght: f64,
    tmpc: f64,
    dwpc: f64,
    /// (direction deg, speed kt) when both transmitted.
    wind: Option<(f64, f64)>,
}

/// Assemble raw levels into the native-sounding column shape: descending
/// pressure deduped, dewpoint capped at temperature, and missing winds
/// interpolated linearly between known levels (edges take the nearest
/// known value) so the hodograph stays honest without zero-wind
/// artifacts.
fn column_from_levels(
    station: &str,
    valid: DateTime<Utc>,
    raw: &[RawLevel],
) -> Result<rustwx_sounding::SoundingColumn, String> {
    let mut pres = Vec::new();
    let mut hght = Vec::new();
    let mut tmpc = Vec::new();
    let mut dwpc = Vec::new();
    let mut wind: Vec<Option<(f64, f64)>> = Vec::new();
    for level in raw {
        // Descending pressure, deduped.
        if pres.last().is_some_and(|&last: &f64| level.pres >= last) {
            continue;
        }
        pres.push(level.pres);
        hght.push(level.hght);
        tmpc.push(level.tmpc);
        dwpc.push(level.dwpc.min(level.tmpc));
        wind.push(level.wind.map(|(d, s)| {
            let speed_ms = s * 0.514_444;
            let rad = d.to_radians();
            (-speed_ms * rad.sin(), -speed_ms * rad.cos())
        }));
    }
    if pres.len() < 10 {
        return Err(format!(
            "RAOB {station} {}: too few levels",
            valid.format("%Y-%m-%d %Hz")
        ));
    }
    // Interpolate missing winds between known neighbors (index space —
    // RAOB levels are dense enough that this is equivalent to log-p).
    let known: Vec<usize> = (0..wind.len()).filter(|&i| wind[i].is_some()).collect();
    if known.is_empty() {
        return Err("RAOB has no winds".to_owned());
    }
    let (mut u_ms, mut v_ms) = (vec![0.0f64; wind.len()], vec![0.0f64; wind.len()]);
    for i in 0..wind.len() {
        let (u, v) = match wind[i] {
            Some(pair) => pair,
            None => {
                let after = known.iter().copied().find(|&k| k > i);
                let before = known.iter().copied().rev().find(|&k| k < i);
                match (before, after) {
                    (Some(b), Some(a)) => {
                        let t = (i - b) as f64 / (a - b) as f64;
                        let (ub, vb) = wind[b].unwrap();
                        let (ua, va) = wind[a].unwrap();
                        (ub + (ua - ub) * t, vb + (va - vb) * t)
                    }
                    (Some(b), None) => wind[b].unwrap(),
                    (None, Some(a)) => wind[a].unwrap(),
                    (None, None) => (0.0, 0.0),
                }
            }
        };
        u_ms[i] = u;
        v_ms[i] = v;
    }
    let n = pres.len();
    Ok(rustwx_sounding::SoundingColumn {
        pressure_hpa: pres,
        height_m_msl: hght,
        temperature_c: tmpc,
        dewpoint_c: dwpc,
        u_ms,
        v_ms,
        omega_pa_s: vec![0.0; n],
        metadata: rustwx_sounding::SoundingMetadata {
            station_id: station.to_owned(),
            valid_time: valid.format("%Y-%m-%d %Hz").to_string(),
            ..Default::default()
        },
    })
}

/// Fetch one IEM RAOB profile (synoptic hours only) into the
/// native-sounding column shape.
pub fn fetch_raob(
    station: &str,
    launch: DateTime<Utc>,
) -> Result<rustwx_sounding::SoundingColumn, String> {
    let ts = launch.format("%Y%m%d%H").to_string();
    let url = format!("https://mesonet.agron.iastate.edu/json/raob.py?ts={ts}&station={station}");
    let text = data_source::fetch_text(&url).map_err(|e| e.to_string())?;
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let profile = root["profiles"]
        .as_array()
        .and_then(|p| p.first())
        .ok_or("no profile")?;
    let levels = profile["profile"].as_array().ok_or("no levels")?;
    let mut raw = Vec::new();
    for level in levels {
        let f = |k: &str| level[k].as_f64();
        let (Some(pres), Some(hght), Some(tmpc), Some(dwpc)) =
            (f("pres"), f("hght"), f("tmpc"), f("dwpc"))
        else {
            continue;
        };
        raw.push(RawLevel {
            pres,
            hght,
            tmpc,
            dwpc,
            wind: match (f("drct"), f("sknt")) {
                (Some(d), Some(s)) => Some((d, s)),
                _ => None,
            },
        });
    }
    column_from_levels(station, launch, &raw)
}

/// The launch search window for a displayed-frame time: `when` plus the
/// 90-minute transmission grace, reaching back far enough (13 h) that the
/// previous synoptic launch is always inside.
fn launch_window(when: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let end = when + Duration::minutes(90);
    (end - Duration::hours(13), end)
}

/// One Wyoming CGI query: a (YEAR, MONTH, FROM, TO) tuple in the archive's
/// DDHH convention. A window is served by one query per calendar month it
/// touches (FROM/TO are day-hour within YEAR/MONTH), so a 13-hour window
/// yields one segment, or two across a month boundary.
fn wyoming_segments(start: DateTime<Utc>, end: DateTime<Utc>) -> Vec<(i32, u32, String, String)> {
    let ddhh = |t: &DateTime<Utc>| format!("{:02}{:02}", t.day(), t.hour());
    if (start.year(), start.month()) == (end.year(), end.month()) {
        return vec![(start.year(), start.month(), ddhh(&start), ddhh(&end))];
    }
    // Month boundary: [start ..= last hour of start's month], then
    // [first hour of end's month ..= end].
    let first_of_end_month = match Utc.with_ymd_and_hms(end.year(), end.month(), 1, 0, 0, 0) {
        chrono::LocalResult::Single(t) => t,
        _ => return vec![(end.year(), end.month(), "0100".to_owned(), ddhh(&end))],
    };
    let last_of_start_month = first_of_end_month - Duration::hours(1);
    vec![
        (
            start.year(),
            start.month(),
            ddhh(&start),
            ddhh(&last_of_start_month),
        ),
        (end.year(), end.month(), "0100".to_owned(), ddhh(&end)),
    ]
}

/// Parse a Wyoming `Observations at 21Z 11 Jun 2026` header time. Manual
/// hour split because chrono's `NaiveDateTime` parser wants minutes the
/// header doesn't carry.
fn parse_wyoming_header_time(text: &str) -> Option<DateTime<Utc>> {
    let (hour_text, date_text) = text.trim().split_once("Z ")?;
    let hour: u32 = hour_text.trim().parse().ok()?;
    let date = NaiveDate::parse_from_str(date_text.trim(), "%d %b %Y").ok()?;
    Some(date.and_hms_opt(hour, 0, 0)?.and_utc())
}

/// One fixed-width column of a Wyoming TEXT:LIST data line (7 chars per
/// column: PRES HGHT TEMP DWPT RELH MIXR DRCT SKNT THTA THTE THTV).
/// Blank columns are MISSING — whitespace splitting would shift fields,
/// so slice by position.
fn wyoming_column(line: &str, index: usize) -> Option<f64> {
    let start = index * 7;
    let end = (start + 7).min(line.len());
    line.get(start..end)?.trim().parse().ok()
}

/// Every sounding in a Wyoming TEXT:LIST response: (valid time, levels).
/// Sections are `<H2>… Observations at HHZ dd Mon yyyy</H2><PRE>table
/// </PRE>`; malformed sections are skipped, never fatal (network data).
fn parse_wyoming_listing(html: &str) -> Vec<(DateTime<Utc>, Vec<RawLevel>)> {
    let mut soundings = Vec::new();
    for section in html.split("<H2>").skip(1) {
        let Some(header_end) = section.find("</H2>") else {
            continue;
        };
        let Some(valid) = section[..header_end]
            .split("Observations at ")
            .nth(1)
            .and_then(parse_wyoming_header_time)
        else {
            continue;
        };
        // The data table is the <PRE> right after the header; the
        // station-info <PRE> further on never reaches this slice.
        let Some(pre_start) = section[header_end..].find("<PRE>") else {
            continue;
        };
        let table_start = header_end + pre_start + "<PRE>".len();
        let Some(pre_len) = section[table_start..].find("</PRE>") else {
            continue;
        };
        let mut raw = Vec::new();
        for line in section[table_start..table_start + pre_len].lines() {
            if !line.is_ascii() {
                continue;
            }
            // Header/divider lines fail the numeric parse and fall out.
            let (Some(pres), Some(hght), Some(tmpc), Some(dwpc)) = (
                wyoming_column(line, 0),
                wyoming_column(line, 1),
                wyoming_column(line, 2),
                wyoming_column(line, 3),
            ) else {
                continue;
            };
            raw.push(RawLevel {
                pres,
                hght,
                tmpc,
                dwpc,
                wind: match (wyoming_column(line, 6), wyoming_column(line, 7)) {
                    (Some(d), Some(s)) => Some((d, s)),
                    _ => None,
                },
            });
        }
        if !raw.is_empty() {
            soundings.push((valid, raw));
        }
    }
    soundings
}

/// Fetch the launch nearest BEFORE `when` (+grace) for one station,
/// specials included: one windowed Wyoming-archive query lists every
/// launch around the displayed time (a 21z special beats the 12z
/// synoptic when the loop is scrubbed to 21z); the IEM synoptic walk
/// covers stations without WMO numbers and Wyoming outages. Returns the
/// column and the launch's actual valid time.
pub fn fetch_raob_near(
    site: &RaobSite,
    when: DateTime<Utc>,
) -> Result<(rustwx_sounding::SoundingColumn, DateTime<Utc>), String> {
    let mut wyoming_err = String::new();
    if let Some(synop) = site.synop {
        let (start, end) = launch_window(when);
        let mut launches: Vec<(DateTime<Utc>, Vec<RawLevel>)> = Vec::new();
        for (year, month, from, to) in wyoming_segments(start, end) {
            let url = format!(
                "https://weather.uwyo.edu/cgi-bin/sounding?TYPE=TEXT%3ALIST\
                 &YEAR={year}&MONTH={month:02}&FROM={from}&TO={to}&STNM={synop}"
            );
            match data_source::fetch_listing_text(&url) {
                Ok(html) => launches.extend(parse_wyoming_listing(&html)),
                Err(e) => wyoming_err = e.to_string(),
            }
        }
        // Newest launch at-or-before the window end first — specials rank
        // by their actual valid hour, exactly what loop-scrubbing wants.
        // Walk DOWN the window on a thin/rejected column instead of
        // abandoning Wyoming entirely (review finding: a good 18Z special
        // must not lose to the IEM 12Z fallback because a later truncated
        // transmission failed to assemble).
        launches.retain(|(valid, _)| *valid <= end);
        launches.sort_by_key(|(valid, _)| std::cmp::Reverse(*valid));
        for (valid, raw) in launches {
            match column_from_levels(&site.id, valid, &raw) {
                Ok(column) => return Ok((column, valid)),
                Err(e) => wyoming_err = e,
            }
        }
    }
    // IEM synoptic fallback (no WMO number, Wyoming outage, or a
    // too-thin special): walk back through 00/12z launches.
    let mut last_err = wyoming_err;
    for launch in launch_times_before(when) {
        match fetch_raob(&site.id, launch) {
            Ok(column) => return Ok((column, launch)),
            Err(e) => last_err = e,
        }
    }
    Err(format!("no launch near {when} for {}: {last_err}", site.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn igra2_overrides_correct_the_known_bad_iem_coordinates() {
        // Review finding: 14/140 IEM stations sat >20 km from their true
        // launch point (KAPX 184 km offshore, KILX 63 km east). Truth =
        // the IGRA2 station list, fetched 2026-06-12; assert every
        // override landed within ~0.06° (~5 km).
        let truth: &[(&str, f32, f32)] = &[
            ("K1Y7", 32.8356, -114.4),
            ("KALY", 42.7500, -73.8000),
            ("KAPX", 44.9075, -84.7189),
            ("KFWD", 32.8350, -97.2986),
            ("KILX", 40.1517, -89.3383),
            ("KLKN", 40.8600, -115.7422),
            ("KMFL", 25.7500, -80.3833),
            ("KOAX", 41.3200, -96.3669),
            ("KOTX", 47.6806, -117.6266),
            ("KREV", 39.5681, -119.7966),
            ("KRNK", 37.2039, -80.4142),
            ("KTFX", 47.4614, -111.3847),
            ("KUNR", 44.0730, -103.2102),
            ("MMMX", 19.4037, -99.1966),
        ];
        for &(id, lat, lon) in truth {
            let site = static_sites()
                .iter()
                .find(|site| site.id == id)
                .unwrap_or_else(|| panic!("{id} missing from the static table"));
            assert!(
                (site.lat - lat).abs() < 0.06 && (site.lon - lon).abs() < 0.06,
                "{id} at {},{} but IGRA2 says {lat},{lon}",
                site.lat,
                site.lon
            );
        }
        // KEFD: IEM's Polish-block synop is bogus; must stay IEM-only.
        let kefd = static_sites().iter().find(|site| site.id == "KEFD");
        assert_eq!(kefd.and_then(|site| site.synop), None);
    }

    /// Live repro for the field report "sounding compute failed" —
    /// network test, run with --ignored.
    #[test]
    #[ignore]
    fn live_raob_roundtrip() {
        let when = Utc::now();
        for station in ["KGRB", "KDVN", "KILX", "KOAX"] {
            for launch in launch_times_before(when) {
                match fetch_raob(station, launch) {
                    Ok(column) => {
                        println!(
                            "{station} {launch}: {} levels, p {:.0}..{:.0}, h {:.0}..{:.0}",
                            column.pressure_hpa.len(),
                            column.pressure_hpa.first().unwrap(),
                            column.pressure_hpa.last().unwrap(),
                            column.height_m_msl.first().unwrap(),
                            column.height_m_msl.last().unwrap()
                        );
                        match rustwx_sounding::NativeSounding::from_column(&column) {
                            Ok(_) => println!("  from_column OK"),
                            Err(e) => println!("  from_column ERR: {e}"),
                        }
                        return;
                    }
                    Err(e) => println!("{station} {launch}: fetch {e}"),
                }
            }
        }
        panic!("no station produced a column");
    }

    /// The field request itself: the ILX 21z special launch on the
    /// 2026-06-11 derecho day, via the displayed-time targeting path.
    /// Network test, run with --ignored.
    #[test]
    #[ignore]
    fn live_ilx_21z_special() {
        use chrono::TimeZone;
        let site = static_sites()
            .iter()
            .find(|site| site.id == "KILX")
            .expect("KILX in the static table")
            .clone();
        let when = Utc.with_ymd_and_hms(2026, 6, 11, 21, 30, 0).unwrap();
        let (column, valid) = fetch_raob_near(&site, when).expect("fetch ILX near 21z");
        println!(
            "KILX launch {valid}: {} levels, p {:.0}..{:.0} hPa",
            column.pressure_hpa.len(),
            column.pressure_hpa.first().unwrap(),
            column.pressure_hpa.last().unwrap()
        );
        assert_eq!(
            (valid.hour(), valid.day()),
            (21, 11),
            "expected the 21z 11 Jun special, got {valid}"
        );
        let native = rustwx_sounding::NativeSounding::from_column(&column).expect("native compute");
        let params = &native.params;
        println!(
            "  SBCAPE {:.0} J/kg · MLCAPE {:.0} J/kg · MUCAPE {:.0} J/kg",
            params.sfcpcl.bplus, params.mlpcl.bplus, params.mupcl.bplus
        );
        println!(
            "  SRH 0-1km {:.0} · 0-3km {:.0} m2/s2 · DCAPE {:.0} J/kg",
            params.srh01.0, params.srh03.0, params.dcape.dcape
        );
    }

    #[test]
    fn launch_times_walk_synoptic_hours() {
        use chrono::TimeZone;
        let when = Utc.with_ymd_and_hms(2026, 6, 11, 18, 30, 0).unwrap();
        let times = launch_times_before(when);
        assert_eq!(times[0].hour(), 12);
        assert_eq!(times[1].hour(), 0);
        // Early morning before 00z data exists -> walks to previous day.
        let early = Utc.with_ymd_and_hms(2026, 6, 11, 0, 30, 0).unwrap();
        let times = launch_times_before(early);
        assert_eq!(times[0].hour(), 0);
    }

    #[test]
    fn static_table_covers_the_network_with_sane_coordinates() {
        let sites = static_sites();
        assert!(sites.len() >= 100, "got {}", sites.len());
        assert!(sites.iter().any(|s| s.id == "KILX"));
        assert!(sites.iter().any(|s| s.id == "KOUN"));
        for site in sites {
            assert!(!site.id.is_empty() && !site.name.is_empty(), "{site:?}");
            assert!(
                site.lat.abs() <= 90.0 && site.lon.abs() <= 180.0,
                "{site:?}"
            );
            // Every station carries a Wyoming key EXCEPT KEFD, whose IEM
            // synop (Polish-block 12906) is bogus — corrected to IEM-only
            // by apply_catalog_corrections.
            assert!(site.synop.is_some() || site.id == "KEFD", "{site:?}");
        }
        // The Wyoming STNM key for the field request's station.
        let ilx = sites.iter().find(|s| s.id == "KILX").unwrap();
        assert_eq!(ilx.synop, Some(74560));
    }

    #[test]
    fn wyoming_segments_split_only_on_month_boundaries() {
        use chrono::TimeZone;
        // Mid-month window: one segment.
        let start = Utc.with_ymd_and_hms(2026, 6, 11, 9, 30, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 11, 22, 30, 0).unwrap();
        assert_eq!(
            wyoming_segments(start, end),
            vec![(2026, 6, "1109".to_owned(), "1122".to_owned())]
        );
        // Across a year boundary: two segments, December then January.
        let start = Utc.with_ymd_and_hms(2025, 12, 31, 20, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0).unwrap();
        assert_eq!(
            wyoming_segments(start, end),
            vec![
                (2025, 12, "3120".to_owned(), "3123".to_owned()),
                (2026, 1, "0100".to_owned(), "0109".to_owned()),
            ]
        );
    }

    #[test]
    fn launch_window_reaches_past_the_previous_synoptic() {
        use chrono::TimeZone;
        let when = Utc.with_ymd_and_hms(2026, 6, 11, 21, 30, 0).unwrap();
        let (start, end) = launch_window(when);
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 6, 11, 23, 0, 0).unwrap());
        assert!(start <= Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap());
    }

    /// A clipped, real-shaped Wyoming TEXT:LIST response (ILX 11 Jun
    /// 2026): two sections — the 12z synoptic and the 21z special — with
    /// blank fixed-width columns that MUST parse as missing, not shift.
    /// (concat!, not line continuations: `\` would strip the leading
    /// spaces the 7-char columns depend on.)
    const WYOMING_FIXTURE: &str = concat!(
        "<HTML>\n<TITLE>University of Wyoming - Radiosonde Data</TITLE>\n",
        "<H2>74560 ILX Lincoln Observations at 12Z 11 Jun 2026</H2>\n<PRE>\n",
        "-----------------------------------------------------------------------------\n",
        "   PRES   HGHT   TEMP   DWPT   RELH   MIXR   DRCT   SKNT   THTA   THTE   THTV\n",
        "    hPa     m      C      C      %    g/kg    deg   knot     K      K      K \n",
        "-----------------------------------------------------------------------------\n",
        " 1000.0     56                                                               \n",
        "  986.0    178   25.6   22.1     81  17.31    180      6  300.0  350.9  303.1\n",
        "  979.0    241   25.2   20.2     74  15.47    150     16  300.2  345.7  302.9\n",
        "  972.0    305   26.0   19.6     68  15.04    175     17  301.6  346.2  304.3\n",
        "</PRE><H3>Station information and sounding indices</H3><PRE>\n",
        "                         Station identifier: ILX\n",
        "                             Station number: 74560\n",
        "</PRE>\n",
        "<H2>74560 ILX Lincoln Observations at 21Z 11 Jun 2026</H2>\n<PRE>\n",
        "-----------------------------------------------------------------------------\n",
        "   PRES   HGHT   TEMP   DWPT   RELH   MIXR   DRCT   SKNT   THTA   THTE   THTV\n",
        "    hPa     m      C      C      %    g/kg    deg   knot     K      K      K \n",
        "-----------------------------------------------------------------------------\n",
        "  984.0    178   30.8   24.8     70  20.52    155     11  305.4  367.2  309.1\n",
        "  979.0    224   30.4   24.4     70  20.13    162         305.4  366.1  309.1\n",
        "  970.1    305   29.6   24.0     72  19.80    175     23  305.4  365.0  308.9\n",
        "</PRE>\n</BODY></HTML>\n",
    );

    #[test]
    fn wyoming_listing_parses_every_section_with_fixed_columns() {
        use chrono::TimeZone;
        let soundings = parse_wyoming_listing(WYOMING_FIXTURE);
        assert_eq!(soundings.len(), 2);
        assert_eq!(
            soundings[0].0,
            Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap()
        );
        assert_eq!(
            soundings[1].0,
            Utc.with_ymd_and_hms(2026, 6, 11, 21, 0, 0).unwrap()
        );
        // The 1000.0 surface line lacks temp/dew -> dropped; three data
        // lines minus one partial = 3 levels in the 12z section.
        assert_eq!(soundings[0].1.len(), 3);
        let first = &soundings[0].1[0];
        assert_eq!(
            (first.pres, first.hght, first.tmpc, first.dwpc),
            (986.0, 178.0, 25.6, 22.1)
        );
        assert_eq!(first.wind, Some((180.0, 6.0)));
        // 21z section: the truncated-wind line keeps thermo but reports
        // no wind (blank SKNT column must not steal THTA's digits).
        let special = &soundings[1].1;
        assert_eq!(special.len(), 3);
        assert_eq!(special[1].wind, None);
        assert_eq!(special[1].tmpc, 30.4);
        // Station-info <PRE> blocks contribute nothing.
        assert!(special.iter().all(|level| level.pres > 900.0));
    }

    #[test]
    fn wyoming_header_time_parses_and_rejects_garbage() {
        use chrono::TimeZone;
        assert_eq!(
            parse_wyoming_header_time("21Z 11 Jun 2026"),
            Some(Utc.with_ymd_and_hms(2026, 6, 11, 21, 0, 0).unwrap())
        );
        assert_eq!(
            parse_wyoming_header_time("00Z 03 May 1999"),
            Some(Utc.with_ymd_and_hms(1999, 5, 3, 0, 0, 0).unwrap())
        );
        assert_eq!(parse_wyoming_header_time("not a time"), None);
        assert_eq!(parse_wyoming_header_time("99Z 11 Jun 2026"), None);
    }
}
