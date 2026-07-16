//! Public, anonymous MeteoAlarm Atom-feed support.
//!
//! The maintained country feeds expose CAP warning metadata and stable links
//! to the official CAP documents. They intentionally do *not* expose map
//! geometry: entries carry EMMA region codes, but resolving those codes to
//! polygons requires the authenticated MeteoAlarm API. BowEcho therefore
//! presents this source as an honest list/detail feed and never invents map
//! placement from an area name or country centroid.

use crate::*;
use quick_xml::events::{BytesStart, Event};

const FEED_ROOT: &str = "https://feeds.meteoalarm.org/feeds/meteoalarm-legacy-atom-";
const ENTRY_ID_PREFIX: &str = "meteoalarm:";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CountryFeed {
    pub(crate) slug: &'static str,
    pub(crate) label: &'static str,
}

pub(crate) const COUNTRY_FEEDS: &[CountryFeed] = &[
    CountryFeed {
        slug: "andorra",
        label: "Andorra",
    },
    CountryFeed {
        slug: "austria",
        label: "Austria",
    },
    CountryFeed {
        slug: "belgium",
        label: "Belgium",
    },
    CountryFeed {
        slug: "bosnia-herzegovina",
        label: "Bosnia and Herzegovina",
    },
    CountryFeed {
        slug: "bulgaria",
        label: "Bulgaria",
    },
    CountryFeed {
        slug: "croatia",
        label: "Croatia",
    },
    CountryFeed {
        slug: "cyprus",
        label: "Cyprus",
    },
    CountryFeed {
        slug: "czechia",
        label: "Czechia",
    },
    CountryFeed {
        slug: "denmark",
        label: "Denmark",
    },
    CountryFeed {
        slug: "estonia",
        label: "Estonia",
    },
    CountryFeed {
        slug: "finland",
        label: "Finland",
    },
    CountryFeed {
        slug: "france",
        label: "France",
    },
    CountryFeed {
        slug: "germany",
        label: "Germany",
    },
    CountryFeed {
        slug: "greece",
        label: "Greece",
    },
    CountryFeed {
        slug: "hungary",
        label: "Hungary",
    },
    CountryFeed {
        slug: "iceland",
        label: "Iceland",
    },
    CountryFeed {
        slug: "ireland",
        label: "Ireland",
    },
    CountryFeed {
        slug: "israel",
        label: "Israel",
    },
    CountryFeed {
        slug: "italy",
        label: "Italy",
    },
    CountryFeed {
        slug: "latvia",
        label: "Latvia",
    },
    CountryFeed {
        slug: "lithuania",
        label: "Lithuania",
    },
    CountryFeed {
        slug: "luxembourg",
        label: "Luxembourg",
    },
    CountryFeed {
        slug: "malta",
        label: "Malta",
    },
    CountryFeed {
        slug: "moldova",
        label: "Moldova",
    },
    CountryFeed {
        slug: "montenegro",
        label: "Montenegro",
    },
    CountryFeed {
        slug: "netherlands",
        label: "Netherlands",
    },
    CountryFeed {
        slug: "republic-of-north-macedonia",
        label: "North Macedonia",
    },
    CountryFeed {
        slug: "norway",
        label: "Norway",
    },
    CountryFeed {
        slug: "poland",
        label: "Poland",
    },
    CountryFeed {
        slug: "portugal",
        label: "Portugal",
    },
    CountryFeed {
        slug: "romania",
        label: "Romania",
    },
    CountryFeed {
        slug: "serbia",
        label: "Serbia",
    },
    CountryFeed {
        slug: "slovakia",
        label: "Slovakia",
    },
    CountryFeed {
        slug: "slovenia",
        label: "Slovenia",
    },
    CountryFeed {
        slug: "spain",
        label: "Spain",
    },
    CountryFeed {
        slug: "sweden",
        label: "Sweden",
    },
    CountryFeed {
        slug: "switzerland",
        label: "Switzerland",
    },
    CountryFeed {
        slug: "ukraine",
        label: "Ukraine",
    },
    CountryFeed {
        slug: "united-kingdom",
        label: "United Kingdom",
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WarningSourceMode {
    Auto,
    Nws,
    Europe,
}

impl WarningSourceMode {
    pub(crate) const ALL: [Self; 3] = [Self::Auto, Self::Nws, Self::Europe];

    pub(crate) fn from_key(key: &str) -> Self {
        match key.trim().to_ascii_lowercase().as_str() {
            "nws" => Self::Nws,
            "europe" | "meteoalarm" => Self::Europe,
            _ => Self::Auto,
        }
    }

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Nws => "nws",
            Self::Europe => "europe",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (radar country)",
            Self::Nws => "NWS (United States)",
            Self::Europe => "MeteoAlarm (Europe)",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedWarningSource {
    Nws,
    MeteoAlarm(CountryFeed),
    Unavailable(String),
}

pub(crate) fn resolve_warning_source(
    mode: WarningSourceMode,
    selected_country: Option<CountryFeed>,
    radar_country: Option<CountryFeed>,
    nws_available: bool,
) -> ResolvedWarningSource {
    match mode {
        WarningSourceMode::Nws if nws_available => ResolvedWarningSource::Nws,
        WarningSourceMode::Nws => ResolvedWarningSource::Unavailable(
            "NWS Alerts require a US NEXRAD or TDWR radar context.".to_owned(),
        ),
        WarningSourceMode::Europe => selected_country
            .or(radar_country)
            .map(ResolvedWarningSource::MeteoAlarm)
            .unwrap_or_else(|| {
                ResolvedWarningSource::Unavailable(
                    "Choose a MeteoAlarm country; the current radar has no matching country feed."
                        .to_owned(),
                )
            }),
        WarningSourceMode::Auto if nws_available => ResolvedWarningSource::Nws,
        WarningSourceMode::Auto => radar_country
            .map(ResolvedWarningSource::MeteoAlarm)
            .unwrap_or_else(|| {
                ResolvedWarningSource::Unavailable(
                    "No built-in warning feed matches this radar country. Choose NWS or a MeteoAlarm country explicitly."
                        .to_owned(),
                )
            }),
    }
}

pub(crate) fn country_feed_by_slug(slug: &str) -> Option<CountryFeed> {
    COUNTRY_FEEDS
        .iter()
        .copied()
        .find(|country| country.slug == slug.trim().to_ascii_lowercase())
}

/// Match the country labels used by BowEcho's international radar catalog to
/// one of the official MeteoAlarm country feeds. Aliases cover labels that do
/// not equal the feed's English display name verbatim.
pub(crate) fn country_feed_for_label(label: &str) -> Option<CountryFeed> {
    let normalized = label
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '.'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let slug = match normalized.as_str() {
        "bosnia & herzegovina" | "bosnia and herzegovina" => "bosnia-herzegovina",
        "czech republic" => "czechia",
        "great britain" | "uk" | "united kingdom" => "united-kingdom",
        "north macedonia" | "republic of north macedonia" => "republic-of-north-macedonia",
        _ => {
            COUNTRY_FEEDS
                .iter()
                .find(|country| country.label.eq_ignore_ascii_case(&normalized))?
                .slug
        }
    };
    country_feed_by_slug(slug)
}

pub(crate) fn feed_url(country: CountryFeed) -> String {
    format!("{FEED_ROOT}{}", country.slug)
}

pub(crate) fn is_meteoalarm_record(record: &HazardRecord) -> bool {
    record.event_id.starts_with(ENTRY_ID_PREFIX)
}

pub(crate) fn is_meteoalarm_overlay(overlay: &HazardOverlay) -> bool {
    overlay.records.iter().any(is_meteoalarm_record)
        || overlay.source_label.starts_with("MeteoAlarm ")
}

/// MeteoAlarm awareness colors are carried in the human-readable Atom title
/// (for example `Orange Thunderstorm Warning`). Prefer that official color;
/// CAP severity is a standards-level fallback and is not a one-to-one color
/// mapping for every national service.
pub(crate) fn record_accent_color(record: &HazardRecord) -> Option<egui::Color32> {
    if !is_meteoalarm_record(record) {
        return None;
    }
    let title = record.label.trim().to_ascii_lowercase();
    if title.starts_with("red ") {
        return Some(egui::Color32::from_rgb(220, 54, 54));
    }
    if title.starts_with("orange ") {
        return Some(egui::Color32::from_rgb(251, 140, 0));
    }
    if title.starts_with("yellow ") {
        return Some(egui::Color32::from_rgb(240, 205, 35));
    }
    if title.starts_with("green ") {
        return Some(egui::Color32::from_rgb(80, 185, 95));
    }
    record
        .severity
        .as_deref()
        .map(|severity| match severity_rank(severity) {
            4 => egui::Color32::from_rgb(220, 54, 54),
            3 => egui::Color32::from_rgb(251, 140, 0),
            2 => egui::Color32::from_rgb(240, 205, 35),
            1 => egui::Color32::from_rgb(80, 185, 95),
            _ => egui::Color32::GRAY,
        })
}

pub(crate) fn load_live(
    country: CountryFeed,
    query_time_utc: DateTime<Utc>,
) -> Result<HazardOverlay, String> {
    let start = Instant::now();
    let url = feed_url(country);
    let text = data_source::fetch_text(&url)
        .map_err(|error| format!("MeteoAlarm {} feed fetch failed: {error}", country.label))?;
    parse_atom_overlay(&text, country, query_time_utc, start.elapsed())
}

#[derive(Default)]
struct EntryDraft {
    atom_id: String,
    title: String,
    area: String,
    event: String,
    sent: String,
    onset: String,
    expires: String,
    certainty: String,
    severity: String,
    urgency: String,
    status: String,
    message_type: String,
    identifier: String,
    geocode_name: String,
    geocode_value: String,
    cap_url: String,
}

impl EntryDraft {
    fn finish(self, country: CountryFeed, query_time_utc: DateTime<Utc>) -> Option<HazardRecord> {
        if !self.status.is_empty() && !self.status.eq_ignore_ascii_case("actual") {
            return None;
        }
        let expires = parse_cap_time(&self.expires);
        if expires.is_some_and(|expires| expires <= query_time_utc) {
            return None;
        }
        let onset = parse_cap_time(&self.onset).or_else(|| parse_cap_time(&self.sent));
        let lifecycle_status = if onset.as_ref().is_some_and(|onset| *onset > query_time_utc) {
            "Pending"
        } else {
            "Active"
        };
        let stable_id = if self.atom_id.trim().is_empty() {
            format!(
                "{}{}:{}:{}:{}",
                ENTRY_ID_PREFIX, country.slug, self.identifier, self.geocode_value, self.onset
            )
        } else {
            format!("{ENTRY_ID_PREFIX}{}", self.atom_id.trim())
        };
        let area = nonempty(self.area);
        let event = nonempty(self.event);
        let title = nonempty(self.title).unwrap_or_else(|| match (&event, &area) {
            (Some(event), Some(area)) => format!("{event} - {area}"),
            (Some(event), None) => event.clone(),
            (None, Some(area)) => format!("Weather warning - {area}"),
            (None, None) => "Weather warning".to_owned(),
        });
        let mut details = Vec::new();
        if let Some(event) = &event {
            details.push(format!("Event: {event}"));
        }
        if !self.geocode_value.trim().is_empty() {
            let name = if self.geocode_name.trim().is_empty() {
                "Region code"
            } else {
                self.geocode_name.trim()
            };
            details.push(format!("{name}: {}", self.geocode_value.trim()));
        }
        if !self.status.trim().is_empty() {
            details.push(format!("CAP status: {}", self.status.trim()));
        }
        details.push(format!(
            "Official source: MeteoAlarm / {} national meteorological service",
            country.label
        ));
        details.push(
            "Map placement unavailable: the public anonymous feed supplies region codes, not polygon geometry."
                .to_owned(),
        );

        Some(HazardRecord {
            event_id: stable_id,
            label: title,
            event_family: "meteoalarm".to_owned(),
            action: nonempty(self.message_type).unwrap_or_else(|| "Alert".to_owned()),
            // BowEcho's list visibility contract uses Active/Pending. The
            // feed was already filtered to CAP status=Actual and non-expired;
            // retain the original CAP status in `details` above.
            lifecycle_status: Some(lifecycle_status.to_owned()),
            office: format!("MeteoAlarm · {}", country.label),
            headline: event,
            source_url: nonempty(self.cap_url).or_else(|| Some(feed_url(country))),
            area,
            motion: None,
            details,
            valid_start: onset.map(format_utc_seconds),
            valid_end: expires.map(format_utc_seconds),
            severity: nonempty(self.severity),
            certainty: nonempty(self.certainty),
            urgency: nonempty(self.urgency),
            tornado: None,
            hail_inches: None,
            wind_mph: None,
            damage_threat: None,
            points: Vec::new(),
            bbox: [0.0; 4],
        })
    }
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
}

fn parse_cap_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn attribute_value(event: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    event
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.local_name().as_ref() == name)
        .and_then(|attribute| attribute.decode_and_unescape_value(event.decoder()).ok())
        .map(|value| value.into_owned())
}

fn maybe_capture_cap_link(event: &BytesStart<'_>, draft: &mut EntryDraft) {
    let is_cap = attribute_value(event, b"type")
        .is_some_and(|value| value.eq_ignore_ascii_case("application/cap+xml"));
    if is_cap && let Some(url) = attribute_value(event, b"href") {
        draft.cap_url = url;
    }
}

fn assign_text(draft: &mut EntryDraft, path: &[Vec<u8>], text: &str) {
    // CAP geocodes are nested under `entry/cap:geocode`, while the other CAP
    // summary fields are direct entry children. A draft only exists while an
    // Atom entry is open, but still require that ancestry explicitly so a
    // similarly named feed-level element can never leak into a warning.
    if !path.iter().any(|name| name.as_slice() == b"entry") {
        return;
    }
    match path.last().map(Vec::as_slice) {
        Some(b"id") => draft.atom_id.push_str(text),
        Some(b"title") => draft.title.push_str(text),
        Some(b"areaDesc") => draft.area.push_str(text),
        Some(b"event") => draft.event.push_str(text),
        Some(b"sent") => draft.sent.push_str(text),
        Some(b"onset") => draft.onset.push_str(text),
        Some(b"expires") => draft.expires.push_str(text),
        Some(b"certainty") => draft.certainty.push_str(text),
        Some(b"severity") => draft.severity.push_str(text),
        Some(b"urgency") => draft.urgency.push_str(text),
        Some(b"status") => draft.status.push_str(text),
        Some(b"message_type") | Some(b"msgType") => draft.message_type.push_str(text),
        Some(b"identifier") => draft.identifier.push_str(text),
        Some(b"valueName") => draft.geocode_name.push_str(text),
        Some(b"value") => draft.geocode_value.push_str(text),
        _ => {}
    }
}

fn parse_atom_overlay(
    xml: &str,
    country: CountryFeed,
    query_time_utc: DateTime<Utc>,
    elapsed: Duration,
) -> Result<HazardOverlay, String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut path = Vec::<Vec<u8>>::new();
    let mut draft: Option<EntryDraft> = None;
    let mut records = Vec::new();
    let mut scanned_items = 0usize;
    let mut error_count = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                let name = event.local_name().as_ref().to_vec();
                path.push(name.clone());
                if name == b"entry" {
                    draft = Some(EntryDraft::default());
                } else if name == b"link"
                    && let Some(draft) = draft.as_mut()
                {
                    maybe_capture_cap_link(&event, draft);
                }
            }
            Ok(Event::Empty(event)) => {
                if event.local_name().as_ref() == b"link"
                    && let Some(draft) = draft.as_mut()
                {
                    maybe_capture_cap_link(&event, draft);
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(draft) = draft.as_mut() {
                    let value = text
                        .decode()
                        .map_err(|error| format!("MeteoAlarm Atom text decode failed: {error}"))?;
                    assign_text(draft, &path, value.trim());
                }
            }
            Ok(Event::End(event)) => {
                if event.local_name().as_ref() == b"entry" {
                    scanned_items += 1;
                    if let Some(draft) = draft.take() {
                        if let Some(record) = draft.finish(country, query_time_utc) {
                            records.push(record);
                        }
                    } else {
                        error_count += 1;
                    }
                }
                path.pop();
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "MeteoAlarm {} Atom parse failed: {error}",
                    country.label
                ));
            }
        }
    }

    records.sort_by(|left, right| {
        right
            .severity
            .as_deref()
            .map(severity_rank)
            .cmp(&left.severity.as_deref().map(severity_rank))
            .then_with(|| left.valid_end.cmp(&right.valid_end))
            .then_with(|| left.area.cmp(&right.area))
    });
    let parsed_items = records.len();
    Ok(HazardOverlay {
        source_label: format!(
            "MeteoAlarm {} · official public Atom/CAP · list only (no public polygon geometry)",
            country.label
        ),
        query_time_utc: Some(format_utc_seconds(query_time_utc)),
        scanned_items,
        parsed_items,
        polygon_records: 0,
        error_count,
        load_ms: elapsed.as_secs_f32() * 1000.0,
        records,
    })
}

fn severity_rank(severity: &str) -> u8 {
    match severity.trim().to_ascii_lowercase().as_str() {
        "extreme" => 4,
        "severe" => 3,
        "moderate" => 2,
        "minor" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom" xmlns:cap="urn:oasis:names:tc:emergency:cap:1.2">
  <entry>
    <cap:geocode><valueName>EMMA_ID</valueName><value>DE271</value></cap:geocode>
    <cap:areaDesc>Bodenseekreis</cap:areaDesc>
    <cap:event>heavy thunderstorms and hail</cap:event>
    <cap:sent>2026-07-15T20:00:00+00:00</cap:sent>
    <cap:onset>2026-07-15T20:05:00+00:00</cap:onset>
    <cap:expires>2026-07-15T23:00:00+00:00</cap:expires>
    <cap:certainty>Likely</cap:certainty><cap:severity>Severe</cap:severity>
    <cap:urgency>Immediate</cap:urgency><cap:status>Actual</cap:status>
    <cap:message_type>Update</cap:message_type><cap:identifier>dwd-1</cap:identifier>
    <link type="application/cap+xml" href="https://feeds.meteoalarm.org/api/v1/warnings/example"/>
    <id>https://feeds.meteoalarm.org/example?index_area=0&amp;index_info=1</id>
    <title>Orange Thunderstorm Warning - Bodenseekreis</title>
  </entry>
  <entry>
    <cap:areaDesc>Expired Area</cap:areaDesc><cap:status>Actual</cap:status>
    <cap:expires>2026-07-15T19:00:00Z</cap:expires><id>expired</id>
  </entry>
  <entry>
    <cap:areaDesc>Test Area</cap:areaDesc><cap:status>Test</cap:status>
    <cap:expires>2026-07-16T19:00:00Z</cap:expires><id>test</id>
  </entry>
</feed>"#;

    #[test]
    fn parses_active_namespaced_atom_entries_as_list_only_records() {
        let now = DateTime::parse_from_rfc3339("2026-07-15T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let overlay = parse_atom_overlay(
            FIXTURE,
            country_feed_by_slug("germany").unwrap(),
            now,
            Duration::from_millis(4),
        )
        .unwrap();
        assert_eq!(overlay.scanned_items, 3);
        assert_eq!(overlay.parsed_items, 1);
        assert_eq!(overlay.polygon_records, 0);
        assert!(is_meteoalarm_overlay(&overlay));
        let record = &overlay.records[0];
        assert_eq!(record.area.as_deref(), Some("Bodenseekreis"));
        assert_eq!(record.severity.as_deref(), Some("Severe"));
        assert_eq!(record.valid_end.as_deref(), Some("2026-07-15T23:00:00Z"));
        assert!(record.points.is_empty());
        assert_eq!(
            record.source_url.as_deref(),
            Some("https://feeds.meteoalarm.org/api/v1/warnings/example")
        );
        assert!(record.details.iter().any(|line| line == "EMMA_ID: DE271"));
        assert_eq!(
            record_accent_color(record),
            Some(egui::Color32::from_rgb(251, 140, 0))
        );
    }

    #[test]
    fn radar_country_labels_resolve_to_official_country_feeds() {
        assert_eq!(country_feed_for_label("Germany").unwrap().slug, "germany");
        assert_eq!(
            country_feed_for_label("Czech Republic").unwrap().slug,
            "czechia"
        );
        assert_eq!(
            country_feed_for_label("United Kingdom").unwrap().slug,
            "united-kingdom"
        );
        assert!(country_feed_for_label("Australia").is_none());
    }

    #[test]
    fn source_mode_unknown_values_migrate_to_auto() {
        assert_eq!(WarningSourceMode::from_key("nws"), WarningSourceMode::Nws);
        assert_eq!(
            WarningSourceMode::from_key("meteoalarm"),
            WarningSourceMode::Europe
        );
        assert_eq!(
            WarningSourceMode::from_key("future-value"),
            WarningSourceMode::Auto
        );
    }

    #[test]
    fn source_resolution_never_silently_assigns_a_country() {
        let germany = country_feed_by_slug("germany").unwrap();
        let poland = country_feed_by_slug("poland").unwrap();

        assert!(matches!(
            resolve_warning_source(WarningSourceMode::Europe, None, None, false),
            ResolvedWarningSource::Unavailable(message) if message.contains("Choose a MeteoAlarm country")
        ));
        assert_eq!(
            resolve_warning_source(WarningSourceMode::Europe, None, Some(poland), false),
            ResolvedWarningSource::MeteoAlarm(poland)
        );
        assert_eq!(
            resolve_warning_source(
                WarningSourceMode::Europe,
                Some(germany),
                Some(poland),
                false,
            ),
            ResolvedWarningSource::MeteoAlarm(germany)
        );
        assert_eq!(
            resolve_warning_source(WarningSourceMode::Auto, None, None, true),
            ResolvedWarningSource::Nws
        );
    }

    #[test]
    fn official_feed_catalog_and_attribution_contract_stay_explicit() {
        assert_eq!(COUNTRY_FEEDS.len(), 39);
        assert_eq!(
            feed_url(country_feed_by_slug("poland").unwrap()),
            "https://feeds.meteoalarm.org/feeds/meteoalarm-legacy-atom-poland"
        );
        let guide = include_str!("guide.rs");
        assert!(guide.contains("Data is provided by EUMETNET members via MeteoAlarm"));
        assert!(guide.contains("licensed CC BY 4.0"));
        assert!(guide.contains("does not invent map placement"));
    }
}
