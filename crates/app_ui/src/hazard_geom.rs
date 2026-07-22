//! Pure hazard geometry, parsing, and fill helpers moved verbatim out of
//! `main.rs` (v0.29.4 decomposition, queue item #2). Hazard types and
//! constants stay in `main.rs`; this module reaches them via `crate::`.

use crate::*;

pub(crate) fn load_live_hazard_overlay_with_preview<A, F>(
    query_time_utc: DateTime<Utc>,
    preview_records: Vec<HazardRecord>,
    custom_provider_url: Option<String>,
    on_active_alerts: A,
    mut on_preview: F,
) -> Result<HazardOverlay, String>
where
    A: FnOnce(&HazardOverlay),
    F: FnMut(HazardOverlay),
{
    // Live warning latency must be governed by the authoritative current-alert
    // endpoint, not by dozens of optional text-product enrichment requests.
    // The CAP/GeoJSON alert already carries geometry, lifecycle, VTEC,
    // observed/radar-indicated tags, damage threat, hail, wind, headline, and
    // description. Historical warning reconstruction keeps using the richer
    // text-product path below.
    let start = Instant::now();
    let load = load_weather_gov_active_alerts(query_time_utc, ZoneGeometryScope::Display)?;
    let mut scanned_items = load.scanned_items;
    let mut parsed_items = load.parsed_items;
    let mut error_count = load.error_count;
    let overlay = build_live_hazard_overlay(
        "NWS active alerts".to_owned(),
        query_time_utc,
        scanned_items,
        parsed_items,
        error_count,
        start,
        load.records,
    );
    // Hand the pure active-alerts overlay (no SPC MD / custom-feed records)
    // to the caller — the live refresh shares it with the background alert
    // watcher so the watcher can skip its own identical national load.
    on_active_alerts(&overlay);
    let mut preview_overlay = overlay.clone();
    if !preview_records.is_empty() {
        let mut preview_combined = preview_overlay.records;
        preview_combined.extend(preview_records);
        preview_overlay = build_live_hazard_overlay(
            "NWS active alerts + cached SPC mesoscale discussions".to_owned(),
            query_time_utc,
            scanned_items,
            parsed_items,
            error_count,
            start,
            preview_combined,
        );
    }
    on_preview(preview_overlay);

    let mut records = overlay.records;
    let mut source_label = match load_spc_mesoscale_discussions(query_time_utc) {
        Ok(mut md_load) => {
            scanned_items += md_load.scanned_items;
            parsed_items += md_load.parsed_items;
            error_count += md_load.error_count;
            records.append(&mut md_load.records);
            "NWS active alerts + SPC mesoscale discussions".to_owned()
        }
        Err(_) => {
            error_count += 1;
            "NWS active alerts (SPC MD unavailable)".to_owned()
        }
    };
    match wpc_mpd::load_live(query_time_utc) {
        Ok(mut mpd_load) => {
            scanned_items += mpd_load.scanned_items;
            parsed_items += mpd_load.parsed_items;
            error_count += mpd_load.error_count;
            records.append(&mut mpd_load.records);
            source_label.push_str(" + WPC precipitation discussions");
        }
        Err(_) => {
            error_count += 1;
            source_label.push_str(" (WPC MPD unavailable)");
        }
    }
    let mut overlay = build_live_hazard_overlay(
        source_label,
        query_time_utc,
        scanned_items,
        parsed_items,
        error_count,
        start,
        records,
    );

    // Optional operator-supplied warning feed (poll-URL style, like the radar
    // `poll_url`): fetch, parse, and merge into the same hazards layer. The
    // custom feed is its own authoritative source, so its records bypass the
    // NWS active-alert corroboration filter that `build_live_hazard_overlay`
    // applies to the reconstructed-text path.
    if let Some(url) = custom_provider_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        match load_custom_warning_provider(url, query_time_utc) {
            Ok(load) => {
                overlay.scanned_items += load.scanned_items;
                overlay.parsed_items += load.parsed_items;
                overlay.error_count += load.error_count;
                if merge_custom_provider_records(&mut overlay, load.records) > 0 {
                    overlay.source_label =
                        format!("{} + custom warning feed", overlay.source_label);
                }
            }
            Err(_) => {
                overlay.error_count += 1;
                overlay.source_label =
                    format!("{} (custom warning feed unavailable)", overlay.source_label);
            }
        }
        overlay.load_ms = start.elapsed().as_secs_f32() * 1000.0;
    }

    Ok(overlay)
}

/// Normalize the configured custom warning-feed URL: trim, drop empties, and
/// require an `http(s)` scheme so an accidental local path or bare word does
/// not become a network fetch.
pub(crate) fn custom_warning_provider_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

/// Fetch and parse a user-supplied warning feed. Accepts the NWS CAP/GeoJSON
/// alert `FeatureCollection` shape (identical to api.weather.gov/alerts/active)
/// first, falling back to the NWS text/VTEC + lat/lon polygon format that the
/// Local-file loader uses. Reuses the existing hazard record model and parsers
/// so a custom feed renders exactly like the built-in warnings layer.
pub(crate) fn load_custom_warning_provider(
    url: &str,
    query_time_utc: DateTime<Utc>,
) -> Result<SpcMdLoad, String> {
    let text = data_source::fetch_text(url)
        .map_err(|err| format!("Custom warning feed fetch failed: {err}"))?;
    parse_custom_warning_feed(&text, query_time_utc)
}

/// Whether a custom-feed body self-identifies as a CAP/GeoJSON
/// `FeatureCollection`: a `features` array, or the GeoJSON
/// `"type": "FeatureCollection"` marker. Needed because
/// [`WeatherAlertFeatureCollection`] itself deserializes from ANY JSON object
/// (`features` is `#[serde(default)]`), so without this probe an arbitrary
/// JSON error payload (e.g. an API `{"title": ..., "status": 429}` body)
/// would masquerade as a legitimately empty quiet feed.
fn custom_feed_body_is_feature_collection(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .is_some_and(|value| {
            value
                .get("features")
                .is_some_and(serde_json::Value::is_array)
                || value.get("type").and_then(serde_json::Value::as_str)
                    == Some("FeatureCollection")
        })
}

/// Parse an already-fetched custom warning feed body (see
/// [`load_custom_warning_provider`]).
fn parse_custom_warning_feed(
    text: &str,
    query_time_utc: DateTime<Utc>,
) -> Result<SpcMdLoad, String> {
    // CAP/GeoJSON FeatureCollection: richest form — carries geometry, VTEC,
    // lifecycle, tags, headline. A successfully parsed collection is
    // authoritative at ANY feature count: an empty `features` array is the
    // NORMAL quiet state of a healthy feed (api.weather.gov/alerts/active
    // returns exactly that when nothing is active), so it yields Ok with zero
    // records — never an error. Only a body that does not parse as a
    // FeatureCollection at all falls through to the text parser. A custom
    // feed is expected to carry its own polygon geometry, so zone lookups
    // are intentionally skipped (empty map) to keep the fetch hermetic and
    // avoid surprise api.weather.gov calls.
    if custom_feed_body_is_feature_collection(text)
        && let Ok(collection) = serde_json::from_str::<WeatherAlertFeatureCollection>(text)
    {
        let zone_geometries = HashMap::new();
        let mut records = Vec::new();
        let mut parsed_items = 0usize;
        let mut error_count = 0usize;
        for feature in &collection.features {
            match parse_weather_alert_feature_with_zones(feature, query_time_utc, &zone_geometries)
            {
                Ok(mut feature_records) => {
                    if !feature_records.is_empty() {
                        parsed_items += 1;
                        records.append(&mut feature_records);
                    }
                }
                Err(_) => error_count += 1,
            }
        }
        return Ok(SpcMdLoad {
            scanned_items: collection.features.len(),
            parsed_items,
            error_count,
            records,
        });
    }

    // Fall back to the NWS text / VTEC / lat-lon polygon format (the same
    // parser the Local-file loader uses).
    let path = Path::new("custom-warning-feed");
    let records = parse_hazard_records_from_text(path, text, Some(query_time_utc));
    if records.is_empty() {
        return Err("Custom warning feed returned no parseable warnings".to_owned());
    }
    let parsed_items = records.len();
    Ok(SpcMdLoad {
        scanned_items: parsed_items,
        parsed_items,
        error_count: 0,
        records,
    })
}

/// Merge custom-provider records into an already-built live overlay. Keeps only
/// user-enabled families and renderable geometry (mirroring the display-time
/// filter, which tolerates `None` lifecycle), then dedupes and re-sorts against
/// the existing records. Returns the number of records actually added.
pub(crate) fn merge_custom_provider_records(
    overlay: &mut HazardOverlay,
    custom_records: Vec<HazardRecord>,
) -> usize {
    let mut custom_records = custom_records;
    custom_records.retain(|record| {
        hazard_family_has_user_filter(&record.event_family)
            && hazard_points_renderable(&record.points)
    });
    if custom_records.is_empty() {
        return 0;
    }
    let before = overlay.records.len();
    let mut combined = std::mem::take(&mut overlay.records);
    combined.extend(custom_records);
    dedupe_hazard_records(&mut combined);
    sort_hazard_records(&mut combined);
    let added = combined.len().saturating_sub(before);
    overlay.polygon_records = combined.len();
    overlay.records = combined;
    added
}

pub(crate) fn load_event_loop_hazard_overlay_with_preview<F>(
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    mut on_preview: F,
) -> Result<HazardOverlay, String>
where
    F: FnMut(HazardOverlay),
{
    let start = Instant::now();
    let mut records = Vec::new();
    let mut scanned_items = 0usize;
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;
    let mut source_labels = Vec::<String>::new();
    let mut first_error = None::<String>;
    let active_alert_query_time_utc = Utc::now();
    let include_active_alerts = event_loop_window_should_merge_active_alerts(
        start_utc,
        end_utc,
        active_alert_query_time_utc,
    );

    thread::scope(|scope| {
        let (source_sender, source_receiver) = mpsc::channel::<LiveHazardSourceMessage>();

        if include_active_alerts {
            let active_sender = source_sender.clone();
            scope.spawn(move || {
                send_live_hazard_source_load(
                    active_sender,
                    "NWS active alerts".to_owned(),
                    "Event-loop NWS active alert worker panicked",
                    || {
                        load_weather_gov_active_alerts(
                            active_alert_query_time_utc,
                            ZoneGeometryScope::Display,
                        )
                    },
                );
            });
        }

        for &product_type in HOT_TEXT_PRODUCT_TYPES {
            let hot_text_sender = source_sender.clone();
            scope.spawn(move || {
                send_live_hazard_source_load(
                    hot_text_sender,
                    format!("NWS {product_type} archive text"),
                    "Event-loop NWS product worker panicked",
                    || fetch_hot_text_product_type_for_window(product_type, start_utc, end_utc),
                );
            });
        }

        drop(source_sender);

        for message in source_receiver {
            match message.result {
                Ok(mut load) => {
                    scanned_items += load.scanned_items;
                    parsed_items += load.parsed_items;
                    error_count += load.error_count;
                    if !load.records.is_empty() {
                        if !source_labels
                            .iter()
                            .any(|label| label == &message.source_label)
                        {
                            source_labels.push(message.source_label);
                        }
                        records.append(&mut load.records);
                        on_preview(build_event_loop_hazard_overlay(
                            source_labels.join(" + "),
                            (start_utc, end_utc),
                            scanned_items,
                            parsed_items,
                            error_count,
                            start,
                            records.clone(),
                        ));
                    }
                }
                Err(err) => {
                    error_count += 1;
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
    });

    if records.is_empty()
        && let Some(err) = first_error
    {
        return Err(err);
    }

    Ok(build_event_loop_hazard_overlay(
        if include_active_alerts {
            "NWS active alerts + warning text archive window".to_owned()
        } else {
            "NWS warning text archive window".to_owned()
        },
        (start_utc, end_utc),
        scanned_items,
        parsed_items,
        error_count,
        start,
        records,
    ))
}

fn event_loop_window_should_merge_active_alerts(
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    now_utc: DateTime<Utc>,
) -> bool {
    start_utc <= now_utc + chrono::Duration::minutes(5)
        && end_utc >= now_utc - chrono::Duration::minutes(EVENT_LOOP_ACTIVE_ALERT_MERGE_LAG_MINUTES)
}

fn send_live_hazard_source_load<F>(
    sender: mpsc::Sender<LiveHazardSourceMessage>,
    source_label: String,
    panic_message: &'static str,
    loader: F,
) where
    F: FnOnce() -> Result<SpcMdLoad, String>,
{
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(loader))
        .unwrap_or_else(|_| Err(panic_message.to_owned()));
    let _ = sender.send(LiveHazardSourceMessage {
        source_label,
        result,
    });
}

pub(crate) fn load_weather_gov_active_alerts(
    query_time_utc: DateTime<Utc>,
    zone_scope: ZoneGeometryScope<'_>,
) -> Result<SpcMdLoad, String> {
    let text = data_source::fetch_text(ACTIVE_ALERTS_URL)
        .map_err(|err| format!("Live hazard fetch failed: {err}"))?;
    let collection: WeatherAlertFeatureCollection = serde_json::from_str(&text)
        .map_err(|err| format!("Live hazard JSON parse failed: {err}"))?;
    let zone_geometries = fetch_weather_alert_zone_geometries(&collection.features, zone_scope);
    let mut records = Vec::new();
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;

    for feature in &collection.features {
        match parse_weather_alert_feature_with_zones(feature, query_time_utc, &zone_geometries) {
            Ok(mut feature_records) => {
                if !feature_records.is_empty() {
                    parsed_items += 1;
                    records.append(&mut feature_records);
                }
            }
            Err(_) => {
                error_count += 1;
            }
        }
    }

    Ok(SpcMdLoad {
        scanned_items: collection.features.len(),
        parsed_items,
        error_count,
        records,
    })
}

pub(crate) fn build_live_hazard_overlay(
    source_label: String,
    query_time_utc: DateTime<Utc>,
    scanned_items: usize,
    parsed_items: usize,
    error_count: usize,
    start: Instant,
    mut records: Vec<HazardRecord>,
) -> HazardOverlay {
    let active_alert_event_ids = active_alert_event_ids(&records);
    dedupe_hazard_records(&mut records);
    records.retain(|record| {
        live_hazard_record_is_current(record)
            && hazard_family_has_user_filter(&record.event_family)
            && live_hazard_record_has_authoritative_source(record, &active_alert_event_ids)
    });
    sort_hazard_records(&mut records);

    HazardOverlay {
        source_label,
        query_time_utc: Some(format_utc_seconds(query_time_utc)),
        scanned_items,
        parsed_items,
        polygon_records: records.len(),
        error_count,
        load_ms: start.elapsed().as_secs_f32() * 1000.0,
        records,
    }
}

pub(crate) fn build_event_loop_hazard_overlay(
    source_label: String,
    window_utc: (DateTime<Utc>, DateTime<Utc>),
    scanned_items: usize,
    parsed_items: usize,
    error_count: usize,
    start: Instant,
    mut records: Vec<HazardRecord>,
) -> HazardOverlay {
    let (start_utc, end_utc) = window_utc;
    let active_alert_event_ids = active_alert_event_ids(&records);
    dedupe_hazard_records(&mut records);
    records.retain(|record| {
        !matches!(record.action.as_str(), "CAN" | "EXP")
            && event_loop_hazard_family_is_supported(&record.event_family)
            && !hazard_record_shadowed_by_active_alert(record, &active_alert_event_ids)
            && event_loop_hazard_record_intersects_window(record, start_utc, end_utc)
    });
    sort_hazard_records(&mut records);

    HazardOverlay {
        source_label,
        query_time_utc: Some(format!(
            "{} to {}",
            format_utc_seconds(start_utc),
            format_utc_seconds(end_utc)
        )),
        scanned_items,
        parsed_items,
        polygon_records: records.len(),
        error_count,
        load_ms: start.elapsed().as_secs_f32() * 1000.0,
        records,
    }
}

/// Archive loads publish cumulative previews as each source finishes, so the
/// preview and final overlay must apply the same display-family contract.
/// Otherwise a generic CAP `Weather Alert` can flash as a yellow polygon/card
/// before a later product-text source supplies its canonical warning family.
/// LSRs remain valid archive content even though they do not have a sidebar
/// family toggle of their own.
fn event_loop_hazard_family_is_supported(event_family: &str) -> bool {
    hazard_family_has_user_filter(event_family) || event_family == "local storm report"
}

pub(crate) fn load_hazard_overlay_from_path(
    path: &Path,
    query_time_utc: Option<DateTime<Utc>>,
) -> Result<HazardOverlay, String> {
    let start = Instant::now();
    let files = collect_hazard_files(path)?;
    let mut records = Vec::new();
    let mut parsed_files = 0usize;
    let mut errors = 0usize;

    for file in &files {
        match std::fs::read(file) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let before = records.len();
                records.extend(parse_hazard_records_from_text(file, &text, query_time_utc));
                if records.len() > before {
                    parsed_files += 1;
                }
            }
            Err(_) => {
                errors += 1;
            }
        }
    }

    sort_hazard_records(&mut records);

    Ok(HazardOverlay {
        source_label: path.display().to_string(),
        query_time_utc: query_time_utc.map(format_utc_seconds),
        scanned_items: files.len(),
        parsed_items: parsed_files,
        polygon_records: records.len(),
        error_count: errors,
        load_ms: start.elapsed().as_secs_f32() * 1000.0,
        records,
    })
}

pub(crate) fn sort_hazard_records(records: &mut [HazardRecord]) {
    records.sort_by(|left, right| {
        hazard_family_order(&left.event_family)
            .cmp(&hazard_family_order(&right.event_family))
            .then_with(|| {
                hazard_record_threat_priority(right).cmp(&hazard_record_threat_priority(left))
            })
            .then_with(|| left.valid_end.cmp(&right.valid_end))
            .then_with(|| left.label.cmp(&right.label))
    });
}

pub(crate) fn sort_hazard_list_rows(
    rows: &mut [HazardListRow],
    records: &[HazardRecord],
    sort: HazardListSort,
) {
    if sort == HazardListSort::Priority {
        return;
    }
    rows.sort_by(|left, right| {
        let Some(left_record) = records.get(left.index) else {
            return Ordering::Greater;
        };
        let Some(right_record) = records.get(right.index) else {
            return Ordering::Less;
        };
        match sort {
            HazardListSort::Priority => Ordering::Equal,
            HazardListSort::Category => hazard_family_order(&left_record.event_family)
                .cmp(&hazard_family_order(&right_record.event_family))
                .then_with(|| {
                    hazard_record_threat_priority(right_record)
                        .cmp(&hazard_record_threat_priority(left_record))
                })
                .then_with(|| left.label.cmp(&right.label)),
            HazardListSort::Newest => compare_hazard_record_times_descending(
                &left_record.valid_start,
                &right_record.valid_start,
            )
            .then_with(|| left.label.cmp(&right.label)),
            HazardListSort::Oldest => compare_hazard_record_times_ascending(
                &left_record.valid_start,
                &right_record.valid_start,
            )
            .then_with(|| left.label.cmp(&right.label)),
            HazardListSort::Expires => compare_hazard_record_times_ascending(
                &left_record.valid_end,
                &right_record.valid_end,
            )
            .then_with(|| left.label.cmp(&right.label)),
        }
    });
}

fn compare_hazard_record_times_ascending(
    left: &Option<String>,
    right: &Option<String>,
) -> Ordering {
    match (
        parse_hazard_record_time(left),
        parse_hazard_record_time(right),
    ) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_hazard_record_times_descending(
    left: &Option<String>,
    right: &Option<String>,
) -> Ordering {
    match (
        parse_hazard_record_time(left),
        parse_hazard_record_time(right),
    ) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn hazard_record_threat_priority(record: &HazardRecord) -> u8 {
    let damage_priority = record
        .damage_threat
        .as_deref()
        .and_then(canonical_damage_threat)
        .map(|threat| damage_threat_priority(&threat))
        .unwrap_or(0);
    let tornado_priority = if record.event_family == "tornado" {
        record
            .tornado
            .as_deref()
            .map(tornado_detection_priority)
            .unwrap_or(0)
    } else {
        0
    };
    damage_priority.max(tornado_priority)
}

fn damage_threat_priority(threat: &str) -> u8 {
    match threat {
        "CATASTROPHIC" => 60,
        "DESTRUCTIVE" => 50,
        "CONSIDERABLE" => 40,
        _ => 0,
    }
}

fn tornado_detection_priority(value: &str) -> u8 {
    let upper = value.to_ascii_uppercase();
    if upper.contains("OBSERVED")
        || upper.contains("CONFIRMED")
        || upper.contains("EMERGENCY")
        || upper.contains("SPOTTER")
        || upper.contains("LAW ENFORCEMENT")
    {
        30
    } else if upper.contains("RADAR") {
        10
    } else {
        0
    }
}

pub(crate) fn selected_hazard_index_for_event_id(
    records: &[HazardRecord],
    selected_event_id: Option<&str>,
) -> Option<usize> {
    let selected_event_id = selected_event_id?;
    records
        .iter()
        .position(|record| record.event_id == selected_event_id)
}

pub(crate) fn hazard_overlay_records_match(left: &HazardOverlay, right: &HazardOverlay) -> bool {
    left.records == right.records
}

pub(crate) fn hazard_record_should_latch_attention(record: &HazardRecord) -> bool {
    hazard_record_is_active_or_pending(record)
        && hazard_points_renderable(&record.points)
        && record.event_family != "local storm report"
}

pub(crate) fn new_hazard_attention_event_ids(
    existing: &HazardOverlay,
    incoming: &HazardOverlay,
) -> Vec<String> {
    let existing_ids = existing
        .records
        .iter()
        .map(|record| record.event_id.as_str())
        .collect::<BTreeSet<_>>();
    incoming
        .records
        .iter()
        .filter(|record| !existing_ids.contains(record.event_id.as_str()))
        .filter(|record| hazard_record_should_latch_attention(record))
        .map(|record| record.event_id.clone())
        .collect()
}

pub(crate) fn prune_unacknowledged_hazard_ids(
    overlay: &HazardOverlay,
    event_ids: &mut BTreeSet<String>,
) {
    let current_ids = overlay
        .records
        .iter()
        .filter(|record| hazard_record_should_latch_attention(record))
        .map(|record| record.event_id.as_str())
        .collect::<BTreeSet<_>>();
    event_ids.retain(|event_id| current_ids.contains(event_id.as_str()));
}

pub(crate) fn hazard_overlay_change(
    left: &HazardOverlay,
    right: &HazardOverlay,
) -> HazardOverlayChange {
    let left_records = left
        .records
        .iter()
        .map(|record| (record.event_id.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let right_records = right
        .records
        .iter()
        .map(|record| (record.event_id.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    let mut change = HazardOverlayChange::default();

    for (event_id, right_record) in &right_records {
        match left_records.get(event_id) {
            Some(left_record) if hazard_record_geometry_matches(left_record, right_record) => {}
            Some(_) => change.geometry_changed += 1,
            None => change.added += 1,
        }
    }
    for event_id in left_records.keys() {
        if !right_records.contains_key(event_id) {
            change.removed += 1;
        }
    }

    change
}

fn hazard_record_geometry_matches(left: &HazardRecord, right: &HazardRecord) -> bool {
    left.bbox == right.bbox && left.points == right.points
}

pub(crate) fn dedupe_hazard_records(records: &mut Vec<HazardRecord>) {
    let mut unique = Vec::<HazardRecord>::with_capacity(records.len());
    for record in records.drain(..) {
        if let Some(existing) = unique
            .iter_mut()
            .find(|existing| existing.event_id == record.event_id)
        {
            *existing = merge_duplicate_hazard_record(existing, &record);
        } else {
            unique.push(record);
        }
    }
    *records = unique;
}

fn merge_duplicate_hazard_record(
    existing: &HazardRecord,
    candidate: &HazardRecord,
) -> HazardRecord {
    let detail_source =
        if hazard_record_detail_score(candidate) >= hazard_record_detail_score(existing) {
            candidate
        } else {
            existing
        };
    let authoritative_alert_source = authoritative_active_alert_record(existing, candidate);
    let geometry_source = if let Some(alert_source) = authoritative_alert_source {
        alert_source
    } else if existing.action == "ALERT" {
        existing
    } else if candidate.action == "ALERT" {
        candidate
    } else {
        detail_source
    };
    let fallback_source = if std::ptr::eq(detail_source, existing) {
        candidate
    } else {
        existing
    };

    let mut merged = detail_source.clone();
    merged.points = geometry_source.points.clone();
    merged.bbox = geometry_source.bbox;
    if merged.source_url.is_none() {
        merged.source_url = fallback_source.source_url.clone();
    }
    if merged.headline.is_none() {
        merged.headline = fallback_source.headline.clone();
    }
    if merged.area.is_none() {
        merged.area = fallback_source.area.clone();
    }
    for detail in &fallback_source.details {
        if !merged.details.contains(detail) {
            merged.details.push(detail.clone());
        }
    }
    if merged.motion.is_none() {
        merged.motion = fallback_source.motion.clone();
    }
    if merged.valid_start.is_none() {
        merged.valid_start = fallback_source.valid_start.clone();
    }
    if merged.valid_end.is_none() {
        merged.valid_end = fallback_source.valid_end.clone();
    }
    if merged.lifecycle_status.is_none() {
        merged.lifecycle_status = fallback_source.lifecycle_status.clone();
    }
    merged.lifecycle_status = preferred_lifecycle_status_for_records(existing, candidate);
    if merged.severity.is_none() {
        merged.severity = fallback_source.severity.clone();
    }
    if merged.certainty.is_none() {
        merged.certainty = fallback_source.certainty.clone();
    }
    if merged.urgency.is_none() {
        merged.urgency = fallback_source.urgency.clone();
    }
    if merged.tornado.is_none() {
        merged.tornado = fallback_source.tornado.clone();
    }
    if merged.hail_inches.is_none() {
        merged.hail_inches = fallback_source.hail_inches;
    }
    if merged.wind_mph.is_none() {
        merged.wind_mph = fallback_source.wind_mph;
    }
    if merged.damage_threat.is_none() {
        merged.damage_threat = fallback_source.damage_threat.clone();
    }
    if let Some(tombstone) = authoritative_alert_tombstone_record(existing, candidate) {
        merged.action = tombstone.action.clone();
    } else if let Some(alert_source) = authoritative_alert_source {
        merged.action = alert_source.action.clone();
        if alert_source.tornado.is_some() {
            merged.tornado = alert_source.tornado.clone();
        }
        if alert_source.hail_inches.is_some() {
            merged.hail_inches = alert_source.hail_inches;
        }
        if alert_source.wind_mph.is_some() {
            merged.wind_mph = alert_source.wind_mph;
        }
        if alert_source.damage_threat.is_some() {
            merged.damage_threat = alert_source.damage_threat.clone();
        }
    }
    refresh_hazard_label_from_tags(&mut merged);
    merged
}

fn authoritative_active_alert_record<'a>(
    left: &'a HazardRecord,
    right: &'a HazardRecord,
) -> Option<&'a HazardRecord> {
    [left, right]
        .into_iter()
        .filter(|record| {
            hazard_record_is_weather_gov_alert(record) && live_hazard_record_is_current(record)
        })
        .max_by_key(|record| active_alert_authority_key(record))
}

fn authoritative_alert_tombstone_record<'a>(
    left: &'a HazardRecord,
    right: &'a HazardRecord,
) -> Option<&'a HazardRecord> {
    [left, right].into_iter().find(|record| {
        hazard_record_is_weather_gov_alert(record)
            && (matches!(record.action.as_str(), "CAN" | "EXP")
                || matches!(
                    record.lifecycle_status.as_deref(),
                    Some("Canceled") | Some("Expired")
                ))
    })
}

fn active_alert_authority_key(record: &HazardRecord) -> (u8, i64) {
    (
        hazard_record_threat_priority(record),
        parse_hazard_record_time(&record.valid_start)
            .map(|time| time.timestamp_millis())
            .unwrap_or(i64::MIN),
    )
}

fn refresh_hazard_label_from_tags(record: &mut HazardRecord) {
    let Some(event_tracking_number) =
        hazard_event_tracking_number(base_hazard_event_id(&record.event_id))
    else {
        return;
    };
    if !matches!(
        record.event_family.as_str(),
        "tornado"
            | "severe thunderstorm"
            | "flash flood"
            | "flood"
            | "special marine"
            | "snow squall"
    ) {
        return;
    }
    let tags = ParsedWarningTags {
        tornado: record.tornado.clone(),
        hail_inches: record.hail_inches,
        wind_mph: record.wind_mph,
        damage_threat: record.damage_threat.clone(),
    };
    record.label = hazard_label(&record.event_family, event_tracking_number, &tags);
}

fn hazard_event_tracking_number(event_id: &str) -> Option<&str> {
    let parts = event_id.split('.').collect::<Vec<_>>();
    (parts.len() == 4).then(|| parts[3])
}

fn preferred_lifecycle_status(left: Option<&str>, right: Option<&str>) -> Option<String> {
    [left, right]
        .into_iter()
        .flatten()
        .max_by_key(|status| lifecycle_status_priority(status))
        .map(str::to_owned)
}

fn preferred_lifecycle_status_for_records(
    left: &HazardRecord,
    right: &HazardRecord,
) -> Option<String> {
    [left, right]
        .into_iter()
        .find(|record| {
            hazard_record_is_weather_gov_alert(record)
                && matches!(
                    record.lifecycle_status.as_deref(),
                    Some("Canceled") | Some("Expired")
                )
        })
        .and_then(|record| record.lifecycle_status.clone())
        .or_else(|| {
            preferred_lifecycle_status(
                left.lifecycle_status.as_deref(),
                right.lifecycle_status.as_deref(),
            )
        })
}

fn lifecycle_status_priority(status: &str) -> u8 {
    match status {
        "Active" => 4,
        "Pending" => 3,
        "Canceled" => 1,
        "Expired" => 0,
        _ => 2,
    }
}

fn hazard_record_detail_score(record: &HazardRecord) -> usize {
    usize::from(record.source_url.is_some())
        + usize::from(record.area.is_some())
        + usize::from(record.motion.is_some())
        + record.details.len()
        + usize::from(record.headline.is_some())
        + usize::from(record.tornado.is_some())
        + usize::from(record.hail_inches.is_some())
        + usize::from(record.wind_mph.is_some())
        + usize::from(record.damage_threat.is_some())
}

fn fetch_hot_text_product_type_for_window(
    product_type: &str,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
) -> Result<SpcMdLoad, String> {
    let url = format!("{NWS_PRODUCT_API_BASE_URL}/{product_type}");
    let text = data_source::fetch_text(&url)
        .map_err(|err| format!("NWS {product_type} product list fetch failed: {err}"))?;
    let collection: NwsProductCollection = serde_json::from_str(&text)
        .map_err(|err| format!("NWS {product_type} product list parse failed: {err}"))?;
    let summaries = select_hot_text_summaries_for_window(collection.products, start_utc, end_utc);
    let mut records = Vec::new();
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;

    let detail_results = thread::scope(|scope| {
        let workers = summaries
            .iter()
            .map(|summary| scope.spawn(move || fetch_nws_product_detail(summary)))
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| Err("NWS product detail worker panicked".to_owned()))
            })
            .collect::<Vec<_>>()
    });

    for (summary, detail_result) in summaries.iter().zip(detail_results) {
        match detail_result {
            Ok(detail) => {
                let before = records.len();
                let mut parsed = parse_hazard_records_from_text(
                    Path::new(product_type),
                    &detail.product_text,
                    None,
                );
                parsed.retain(|record| {
                    event_loop_hazard_record_intersects_window(record, start_utc, end_utc)
                });
                for record in &mut parsed {
                    record.source_url = Some(summary.url.clone());
                    if record.headline.is_none() {
                        record.headline = Some(detail.product_name.clone());
                    }
                    record.details.push(format!(
                        "Issued {}",
                        format_utc_seconds(detail.issuance_time)
                    ));
                }
                records.append(&mut parsed);
                if records.len() > before {
                    parsed_items += 1;
                }
            }
            Err(_) => {
                error_count += 1;
            }
        }
    }

    Ok(SpcMdLoad {
        scanned_items: summaries.len(),
        parsed_items,
        error_count,
        records,
    })
}

#[cfg(test)]
fn select_hot_text_summaries(
    mut products: Vec<NwsProductSummary>,
    query_time_utc: DateTime<Utc>,
) -> Vec<NwsProductSummary> {
    products.sort_by_key(|product| std::cmp::Reverse(product.issuance_time));
    let recent_start =
        query_time_utc - chrono::Duration::minutes(HOT_TEXT_PRODUCTS_RECENT_WINDOW_MINUTES);
    let near_future = query_time_utc + chrono::Duration::minutes(5);
    let mut selected = Vec::with_capacity(HOT_TEXT_PRODUCTS_MIN_PER_TYPE);

    for (index, summary) in products.into_iter().enumerate() {
        let is_recent =
            summary.issuance_time >= recent_start && summary.issuance_time <= near_future;
        if index < HOT_TEXT_PRODUCTS_MIN_PER_TYPE || is_recent {
            selected.push(summary);
            if selected.len() >= HOT_TEXT_PRODUCTS_MAX_PER_TYPE {
                break;
            }
        } else if summary.issuance_time < recent_start {
            break;
        }
    }

    selected
}

fn select_hot_text_summaries_for_window(
    mut products: Vec<NwsProductSummary>,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
) -> Vec<NwsProductSummary> {
    products.sort_by_key(|product| std::cmp::Reverse(product.issuance_time));
    let earliest = start_utc - chrono::Duration::minutes(EVENT_LOOP_HAZARD_PRODUCT_PAD_MINUTES);
    let latest = end_utc + chrono::Duration::minutes(EVENT_LOOP_HAZARD_PRODUCT_FUTURE_PAD_MINUTES);
    let mut selected = Vec::new();
    for summary in products {
        if summary.issuance_time > latest {
            continue;
        }
        if summary.issuance_time < earliest {
            break;
        }
        selected.push(summary);
        if selected.len() >= EVENT_LOOP_HAZARD_PRODUCTS_MAX_PER_TYPE {
            break;
        }
    }
    selected
}

fn fetch_nws_product_detail(summary: &NwsProductSummary) -> Result<NwsProductDetail, String> {
    if let Ok(cache) = nws_product_detail_cache().lock()
        && let Some(detail) = cache.get(&summary.url).cloned()
    {
        return Ok(detail);
    }

    let text = data_source::fetch_text(&summary.url)
        .map_err(|err| format!("NWS product detail fetch failed: {err}"))?;
    let detail: NwsProductDetail = serde_json::from_str(&text)
        .map_err(|err| format!("NWS product detail parse failed: {err}"))?;
    if let Ok(mut cache) = nws_product_detail_cache().lock() {
        if cache.len() >= HOT_TEXT_DETAIL_CACHE_MAX
            && let Some(first_key) = cache.keys().next().cloned()
        {
            cache.remove(&first_key);
        }
        cache.insert(summary.url.clone(), detail.clone());
    }
    Ok(detail)
}

fn nws_product_detail_cache() -> &'static Mutex<BTreeMap<String, NwsProductDetail>> {
    static CACHE: OnceLock<Mutex<BTreeMap<String, NwsProductDetail>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn load_spc_mesoscale_discussions(query_time_utc: DateTime<Utc>) -> Result<SpcMdLoad, String> {
    let index_html = data_source::fetch_text(SPC_MD_INDEX_URL)
        .map_err(|err| format!("SPC MD index fetch failed: {err}"))?;
    let links = spc_md_product_links(&index_html);
    let mut records = Vec::new();
    let mut parsed_items = 0usize;
    let mut error_count = 0usize;

    for url in &links {
        match data_source::fetch_text(url) {
            Ok(html) => {
                if let Some(record) = parse_spc_md_product_page(url, &html, query_time_utc) {
                    parsed_items += 1;
                    records.push(record);
                }
            }
            Err(_) => {
                error_count += 1;
            }
        }
    }

    Ok(SpcMdLoad {
        scanned_items: links.len(),
        parsed_items,
        error_count,
        records,
    })
}

fn spc_md_product_links(index_html: &str) -> Vec<String> {
    let mut links = Vec::new();
    for part in index_html.split("href=\"").skip(1) {
        let Some(end) = part.find('"') else {
            continue;
        };
        let href = &part[..end];
        let url = if href.starts_with("/products/md/md") && href.ends_with(".html") {
            Some(format!("{SPC_PRODUCT_BASE_URL}{href}"))
        } else if href.starts_with("md") && href.ends_with(".html") {
            Some(format!("{SPC_MD_INDEX_URL}{href}"))
        } else {
            None
        };
        if let Some(url) = url
            && !links.contains(&url)
        {
            links.push(url);
        }
    }
    links
}

fn parse_spc_md_product_page(
    source_url: &str,
    html: &str,
    query_time_utc: DateTime<Utc>,
) -> Option<HazardRecord> {
    let text = extract_preformatted_text(html).unwrap_or(html);
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let points = parse_lat_lon_points(&lines);
    if points.len() < 3 {
        return None;
    }
    let upper = text.to_ascii_uppercase();
    let number = first_number_after(&upper, "MESOSCALE DISCUSSION")?;
    let label = format!("MD {number}");
    let area = strip_prefixed_line(&lines, "Areas affected...");
    let concerning = strip_prefixed_line(&lines, "Concerning...");
    let valid = find_prefixed_line(&lines, "Valid ")?;
    let valid_period = parse_spc_md_valid_period(&valid, query_time_utc)?;
    let watch_probability = strip_prefixed_line(&lines, "Probability of Watch Issuance...");
    let peak_tornado = find_prefixed_line(&lines, "MOST PROBABLE PEAK TORNADO INTENSITY...");
    let peak_wind = find_prefixed_line(&lines, "MOST PROBABLE PEAK WIND GUST...");
    let peak_hail = find_prefixed_line(&lines, "MOST PROBABLE PEAK HAIL SIZE...");
    let mut details = Vec::new();
    details.push(valid);
    if let Some(watch_probability) = watch_probability {
        details.push(format!("Watch issuance {watch_probability}"));
    }
    if let Some(peak_tornado) = peak_tornado {
        details.push(peak_tornado);
    }
    if let Some(peak_wind) = peak_wind {
        details.push(peak_wind);
    }
    if let Some(peak_hail) = peak_hail {
        details.push(peak_hail);
    }

    Some(HazardRecord {
        event_id: format!("spc-md-{number}"),
        label,
        event_family: "mesoscale discussion".to_owned(),
        action: "SPC".to_owned(),
        lifecycle_status: hazard_lifecycle_status(
            "SPC",
            Some(valid_period.0),
            Some(valid_period.1),
            Some(query_time_utc),
        ),
        office: "SPC".to_owned(),
        headline: concerning,
        source_url: Some(source_url.to_owned()),
        area,
        motion: None,
        details,
        valid_start: Some(format_utc_seconds(valid_period.0)),
        valid_end: Some(format_utc_seconds(valid_period.1)),
        severity: None,
        certainty: None,
        urgency: None,
        tornado: None,
        hail_inches: None,
        wind_mph: None,
        damage_threat: None,
        bbox: hazard_bbox(&points),
        points,
    })
}

/// Resolve SPC's compact `Valid DDHHMMZ - DDHHMMZ` range against the live
/// query time. The product omits month and year, so consider the adjacent
/// months and choose the occurrence nearest the query. This also handles MDs
/// that cross UTC month/year boundaries.
fn parse_spc_md_valid_period(
    valid_line: &str,
    query_time_utc: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let tokens = valid_line
        .split(|character: char| character.is_ascii_whitespace() || character == '-')
        .filter_map(parse_spc_md_valid_token)
        .take(2)
        .collect::<Vec<_>>();
    let [start_token, end_token] = tokens.as_slice() else {
        return None;
    };

    let start = (-1..=1)
        .filter_map(|month_offset| utc_in_offset_month(query_time_utc, month_offset, *start_token))
        .min_by_key(|candidate| {
            candidate
                .signed_duration_since(query_time_utc)
                .num_seconds()
                .unsigned_abs()
        })?;
    let end = (0..=1)
        .filter_map(|month_offset| utc_in_offset_month(start, month_offset, *end_token))
        .filter(|candidate| *candidate >= start)
        .min()?;
    Some((start, end))
}

fn parse_spc_md_valid_token(token: &str) -> Option<(u32, u32, u32)> {
    let token = token.trim_matches(|character: char| !character.is_ascii_alphanumeric());
    let digits = token
        .strip_suffix('Z')
        .or_else(|| token.strip_suffix('z'))?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let day = digits[0..2].parse().ok()?;
    let hour = digits[2..4].parse().ok()?;
    let minute = digits[4..6].parse().ok()?;
    (day >= 1 && hour < 24 && minute < 60).then_some((day, hour, minute))
}

fn utc_in_offset_month(
    anchor: DateTime<Utc>,
    month_offset: i32,
    (day, hour, minute): (u32, u32, u32),
) -> Option<DateTime<Utc>> {
    let month_index = anchor.year() * 12 + anchor.month0() as i32 + month_offset;
    let year = month_index.div_euclid(12);
    let month = month_index.rem_euclid(12) as u32 + 1;
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
}

fn extract_preformatted_text(html: &str) -> Option<&str> {
    let start = html.find("<pre>")? + "<pre>".len();
    let end = html[start..].find("</pre>")? + start;
    Some(html[start..end].trim())
}

fn strip_prefixed_line(lines: &[&str], prefix: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        line.trim()
            .strip_prefix(prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

#[cfg(test)]
pub(crate) fn parse_weather_alert_feature(
    feature: &WeatherAlertFeature,
    query_time_utc: DateTime<Utc>,
) -> Result<Vec<HazardRecord>, String> {
    parse_weather_alert_feature_with_zones(feature, query_time_utc, &HashMap::new())
}

pub(crate) fn parse_weather_alert_feature_with_zones(
    feature: &WeatherAlertFeature,
    query_time_utc: DateTime<Utc>,
    zone_geometries: &HashMap<String, Vec<Vec<HazardPoint>>>,
) -> Result<Vec<HazardRecord>, String> {
    let event = feature
        .properties
        .event
        .as_deref()
        .unwrap_or("Weather Alert");
    let parsed_vtec = weather_alert_parameter(&feature.properties.parameters, "VTEC")
        .and_then(|vtec| parse_vtec_alert(&vtec));
    let event_family = weather_alert_family_with_vtec(event, parsed_vtec.as_ref());
    let rings = weather_alert_feature_rings(feature, zone_geometries)?;
    let tags = parse_weather_alert_tags(&feature.properties.parameters);
    let valid_start = parse_alert_time(
        feature
            .properties
            .onset
            .as_deref()
            .or(feature.properties.effective.as_deref()),
    );
    let valid_end = parse_alert_time(
        feature
            .properties
            .ends
            .as_deref()
            .or(feature.properties.expires.as_deref()),
    );
    let action = parsed_vtec
        .as_ref()
        .map(|vtec| vtec.action.as_str())
        .or_else(|| cap_message_type_action(feature.properties.message_type.as_deref()))
        .unwrap_or("ALERT");
    let lifecycle_status =
        hazard_lifecycle_status(action, valid_start, valid_end, Some(query_time_utc));
    let valid_start_text = valid_start.map(format_utc_seconds);
    let valid_end_text = valid_end.map(format_utc_seconds);
    let label = weather_alert_label(event, &event_family, &feature.properties.parameters, &tags);
    let event_id = parsed_vtec
        .as_ref()
        .map(vtec_alert_event_id)
        .or_else(|| {
            feature
                .properties
                .id
                .as_deref()
                .or(feature.id.as_deref())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| event.to_owned());
    let office = feature
        .properties
        .sender_name
        .clone()
        .or_else(|| weather_alert_parameter(&feature.properties.parameters, "AWIPSidentifier"))
        .unwrap_or_else(|| "NWS".to_owned());
    let headline = feature
        .properties
        .headline
        .clone()
        .or_else(|| weather_alert_parameter(&feature.properties.parameters, "NWSheadline"))
        .or_else(|| feature.properties.area_desc.clone())
        .or_else(|| feature.properties.description.clone());
    let source_url = weather_alert_source_url(feature);
    let area = feature.properties.area_desc.clone();
    let description = feature.properties.description.clone();
    let motion = weather_alert_parameter(&feature.properties.parameters, "eventMotionDescription");
    let label_count = rings.len();

    Ok(rings
        .into_iter()
        .enumerate()
        .filter_map(|(index, points)| {
            let points = sanitize_weather_alert_ring(points, &event_family);
            (points.len() >= 3).then(|| HazardRecord {
                event_id: if label_count > 1 {
                    format!("{event_id}#{index}")
                } else {
                    event_id.clone()
                },
                label: if label_count > 1 {
                    format!("{} {}", label, index + 1)
                } else {
                    label.clone()
                },
                event_family: event_family.clone(),
                action: action.to_owned(),
                lifecycle_status: lifecycle_status.clone(),
                office: office.clone(),
                headline: headline.clone(),
                source_url: source_url.clone(),
                area: area.clone(),
                motion: motion.clone(),
                details: description
                    .as_ref()
                    .filter(|description| headline.as_ref() != Some(description))
                    .cloned()
                    .into_iter()
                    .collect(),
                valid_start: valid_start_text.clone(),
                valid_end: valid_end_text.clone(),
                severity: feature.properties.severity.clone(),
                certainty: feature.properties.certainty.clone(),
                urgency: feature.properties.urgency.clone(),
                tornado: tags.tornado.clone(),
                hail_inches: tags.hail_inches,
                wind_mph: tags.wind_mph,
                damage_threat: tags.damage_threat.clone(),
                bbox: hazard_bbox(&points),
                points,
            })
        })
        .collect())
}

fn weather_alert_feature_rings(
    feature: &WeatherAlertFeature,
    zone_geometries: &HashMap<String, Vec<Vec<HazardPoint>>>,
) -> Result<Vec<Vec<HazardPoint>>, String> {
    let mut rings = match &feature.geometry {
        Some(geometry) => weather_alert_geometry_rings(geometry)?,
        None => Vec::new(),
    };
    if rings.is_empty() {
        for zone_url in &feature.properties.affected_zones {
            if let Some(zone_rings) = zone_geometries.get(zone_url) {
                rings.extend(zone_rings.iter().cloned());
            }
        }
    }
    Ok(rings)
}

/// Whether `event_family` is enriched with zone geometry under `scope`.
fn zone_geometry_scope_includes(scope: ZoneGeometryScope<'_>, event_family: &str) -> bool {
    let display = matches!(
        event_family,
        "tornado"
            | "severe thunderstorm"
            | "flash flood"
            | "flood"
            | "fire weather"
            | "special marine"
            | "snow squall"
            | "watch"
            | "special weather"
    );
    match scope {
        ZoneGeometryScope::Display => display,
        ZoneGeometryScope::AlertSound(families) => {
            display && alert_family_enabled(families, event_family)
        }
    }
}

/// The deduped, capped list of zone URLs a load must resolve: zones of
/// geometry-less alerts whose family is in `scope`.
fn weather_alert_zone_urls(
    features: &[WeatherAlertFeature],
    scope: ZoneGeometryScope<'_>,
) -> Vec<String> {
    let mut zone_urls = BTreeSet::new();
    for feature in features {
        if feature.geometry.is_some() {
            continue;
        }
        let event = feature
            .properties
            .event
            .as_deref()
            .unwrap_or("Weather Alert");
        let parsed_vtec = weather_alert_parameter(&feature.properties.parameters, "VTEC")
            .and_then(|vtec| parse_vtec_alert(&vtec));
        let event_family = weather_alert_family_with_vtec(event, parsed_vtec.as_ref());
        if !zone_geometry_scope_includes(scope, &event_family) {
            continue;
        }
        for zone_url in &feature.properties.affected_zones {
            if zone_url.starts_with("https://api.weather.gov/zones/") {
                zone_urls.insert(zone_url.clone());
            }
        }
    }
    zone_urls
        .into_iter()
        .take(MAX_ACTIVE_ALERT_ZONE_GEOMETRIES)
        .collect()
}

fn zone_geometry_memo() -> &'static Mutex<HashMap<String, Vec<Vec<HazardPoint>>>> {
    ZONE_GEOMETRY_MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve `zone_urls` to geometry through the process-wide memo, calling
/// `fetch` only for never-seen zones. Successful lookups are memoized forever
/// (INCLUDING legitimately empty geometries, so a geometry-less zone is not
/// re-fetched every poll); failures are NOT memoized, so a transient network
/// error retries on the next poll. The returned map carries only non-empty
/// geometries, matching what ring assembly can use.
fn resolve_zone_geometries<F>(
    zone_urls: &[String],
    fetch: F,
) -> HashMap<String, Vec<Vec<HazardPoint>>>
where
    F: Fn(&str) -> Result<Vec<Vec<HazardPoint>>, String>,
{
    let mut resolved = HashMap::new();
    for zone_url in zone_urls {
        let memoized = zone_geometry_memo()
            .lock()
            .ok()
            .and_then(|memo| memo.get(zone_url).cloned());
        let rings = match memoized {
            Some(rings) => rings,
            None => match fetch(zone_url) {
                Ok(rings) => {
                    if let Ok(mut memo) = zone_geometry_memo().lock()
                        && memo.len() < ZONE_GEOMETRY_MEMO_MAX_ZONES
                    {
                        memo.insert(zone_url.clone(), rings.clone());
                    }
                    rings
                }
                Err(_) => continue,
            },
        };
        if !rings.is_empty() {
            resolved.insert(zone_url.clone(), rings);
        }
    }
    resolved
}

fn fetch_weather_alert_zone_geometries(
    features: &[WeatherAlertFeature],
    scope: ZoneGeometryScope<'_>,
) -> HashMap<String, Vec<Vec<HazardPoint>>> {
    let zone_urls = weather_alert_zone_urls(features, scope);
    resolve_zone_geometries(&zone_urls, fetch_weather_alert_zone_geometry)
}

fn fetch_weather_alert_zone_geometry(zone_url: &str) -> Result<Vec<Vec<HazardPoint>>, String> {
    let text = data_source::fetch_text(zone_url)
        .map_err(|err| format!("NWS alert zone geometry fetch failed for {zone_url}: {err}"))?;
    let zone: WeatherZoneFeature = serde_json::from_str(&text)
        .map_err(|err| format!("NWS alert zone geometry parse failed for {zone_url}: {err}"))?;
    let Some(geometry) = zone.geometry else {
        return Ok(Vec::new());
    };
    weather_alert_geometry_rings(&geometry)
}

pub(crate) fn weather_alert_geometry_rings(
    geometry: &WeatherAlertGeometry,
) -> Result<Vec<Vec<HazardPoint>>, String> {
    match geometry.geometry_type.as_str() {
        "Polygon" => Ok(parse_polygon_coordinate_value(&geometry.coordinates)
            .into_iter()
            .take(1)
            .collect()),
        "MultiPolygon" => {
            let mut polygons = Vec::new();
            let Some(multi_polygon) = geometry.coordinates.as_array() else {
                return Err("multipolygon coordinates are not an array".to_owned());
            };
            for polygon in multi_polygon {
                if let Some(outer_ring) = parse_polygon_coordinate_value(polygon).into_iter().next()
                {
                    polygons.push(outer_ring);
                }
            }
            Ok(polygons)
        }
        _ => Ok(Vec::new()),
    }
}

fn parse_polygon_coordinate_value(value: &serde_json::Value) -> Vec<Vec<HazardPoint>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ring| {
            let mut points = ring
                .as_array()?
                .iter()
                .filter_map(|coordinate| {
                    let pair = coordinate.as_array()?;
                    let lon = pair.first()?.as_f64()? as f32;
                    let lat = pair.get(1)?.as_f64()? as f32;
                    Some(HazardPoint { lon, lat })
                })
                .collect::<Vec<_>>();
            if points.len() > 1
                && let (Some(first), Some(last)) = (points.first(), points.last())
                && (first.lon - last.lon).abs() <= f32::EPSILON
                && (first.lat - last.lat).abs() <= f32::EPSILON
            {
                points.pop();
            }
            (points.len() >= 3).then_some(points)
        })
        .collect()
}

fn weather_alert_family(event: &str) -> String {
    let upper = event.to_ascii_uppercase();
    if upper.contains("RED FLAG") || upper.contains("FIRE WEATHER") {
        "fire weather".to_owned()
    } else if upper.contains("WATCH") {
        "watch".to_owned()
    } else if upper.contains("TORNADO") {
        "tornado".to_owned()
    } else if upper.contains("SEVERE THUNDERSTORM") {
        "severe thunderstorm".to_owned()
    } else if upper.contains("FLASH FLOOD") {
        "flash flood".to_owned()
    } else if upper.contains("FLOOD") {
        "flood".to_owned()
    } else if upper.contains("TROPICAL STORM") || upper.contains("INLAND TROPICAL") {
        "tropical storm".to_owned()
    } else if upper.contains("HURRICANE") || upper.contains("TYPHOON") {
        "hurricane".to_owned()
    } else if upper.contains("SPECIAL MARINE") {
        "special marine".to_owned()
    } else if upper.contains("SNOW SQUALL") {
        "snow squall".to_owned()
    } else if upper.contains("SPECIAL WEATHER") {
        "special weather".to_owned()
    } else {
        "alert".to_owned()
    }
}

/// Classify CAP alerts from their canonical VTEC identity when available.
/// The free-form CAP `event` text is not reliable enough to drive rendering:
/// some feeds temporarily publish a generic `Weather Alert` even though the
/// VTEC already identifies a Flood Warning. Watches intentionally stay in the
/// shared watch family, and unknown VTEC phenomena fall back to the useful CAP
/// event name instead of discarding a recognized non-VTEC family.
fn weather_alert_family_with_vtec(event: &str, parsed_vtec: Option<&ParsedWarningVtec>) -> String {
    let Some(vtec) = parsed_vtec else {
        return weather_alert_family(event);
    };
    if vtec.significance == "A" {
        return "watch".to_owned();
    }
    let vtec_family = hazard_family_from_phenomenon(&vtec.phenomenon);
    if vtec_family == "warning" {
        weather_alert_family(event)
    } else {
        vtec_family.to_owned()
    }
}

fn parse_weather_alert_tags(parameters: &BTreeMap<String, Vec<String>>) -> ParsedWarningTags {
    ParsedWarningTags {
        tornado: preferred_weather_alert_tornado_detection(parameters),
        hail_inches: max_weather_alert_float_parameter(parameters, "maxHailSize"),
        wind_mph: max_weather_alert_u16_parameter(parameters, "maxWindGust"),
        damage_threat: preferred_weather_alert_damage_threat(parameters),
    }
}

fn weather_alert_parameter_values<'a>(
    parameters: &'a BTreeMap<String, Vec<String>>,
    key: &str,
) -> impl Iterator<Item = &'a str> {
    parameters
        .get(key)
        .into_iter()
        .flatten()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
}

fn weather_alert_parameter(
    parameters: &BTreeMap<String, Vec<String>>,
    key: &str,
) -> Option<String> {
    weather_alert_parameter_values(parameters, key)
        .next()
        .map(str::to_owned)
}

fn preferred_weather_alert_tornado_detection(
    parameters: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    weather_alert_parameter_values(parameters, "tornadoDetection")
        .max_by_key(|value| tornado_detection_priority(value))
        .map(str::to_owned)
}

fn preferred_weather_alert_damage_threat(
    parameters: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    [
        "tornadoDamageThreat",
        "thunderstormDamageThreat",
        "flashFloodDamageThreat",
    ]
    .into_iter()
    .flat_map(|key| weather_alert_parameter_values(parameters, key))
    .filter_map(canonical_damage_threat)
    .max_by_key(|threat| damage_threat_priority(threat))
}

fn max_weather_alert_float_parameter(
    parameters: &BTreeMap<String, Vec<String>>,
    key: &str,
) -> Option<f32> {
    weather_alert_parameter_values(parameters, key)
        .filter_map(parse_leading_float)
        .reduce(f32::max)
}

fn max_weather_alert_u16_parameter(
    parameters: &BTreeMap<String, Vec<String>>,
    key: &str,
) -> Option<u16> {
    weather_alert_parameter_values(parameters, key)
        .filter_map(parse_leading_u16)
        .max()
}

fn weather_alert_source_url(feature: &WeatherAlertFeature) -> Option<String> {
    [
        feature.properties.at_id.as_deref(),
        feature.at_id.as_deref(),
        feature.id.as_deref(),
        feature.properties.id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|value| value.starts_with("http://") || value.starts_with("https://"))
    .map(str::to_owned)
}

fn parse_alert_time(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|time| time.with_timezone(&Utc))
}

pub(crate) fn format_utc_seconds(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn cap_message_type_action(message_type: Option<&str>) -> Option<&'static str> {
    match message_type?
        .trim()
        .to_ascii_uppercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "ALERT" | "NEW" => Some("NEW"),
        "UPDATE" | "UPDATED" | "CONTINUE" | "CONTINUED" => Some("CON"),
        "CANCEL" | "CANCELED" | "CANCELLED" | "CANCELATION" | "CANCELLATION" => Some("CAN"),
        "EXPIRE" | "EXPIRED" => Some("EXP"),
        _ => None,
    }
}

fn weather_alert_label(
    _event: &str,
    event_family: &str,
    parameters: &BTreeMap<String, Vec<String>>,
    tags: &ParsedWarningTags,
) -> String {
    if let Some(vtec) = weather_alert_parameter(parameters, "VTEC")
        && let Some((phenomenon, event_tracking_number)) = parse_vtec_alert_identity(&vtec)
    {
        return hazard_label(
            hazard_family_from_phenomenon(&phenomenon),
            &event_tracking_number,
            tags,
        );
    }
    let prefix = match event_family {
        "tornado" => "TOR",
        "severe thunderstorm" => "SVR",
        "flash flood" => "FFW",
        "flood" => "FLW",
        "fire weather" => "FIRE",
        "special marine" => "SMW",
        "snow squall" => "SQW",
        "watch" => "WATCH",
        "special weather" => "SPS",
        _ => "ALERT",
    };
    if let Some(tornado) = &tags.tornado {
        format!("{prefix} {tornado}")
    } else {
        prefix.to_owned()
    }
}

fn parse_vtec_alert_identity(vtec: &str) -> Option<(String, String)> {
    let parsed = parse_vtec_alert(vtec)?;
    Some((parsed.phenomenon, parsed.event_tracking_number))
}

fn vtec_alert_event_id(vtec: &ParsedWarningVtec) -> String {
    format!(
        "{}.{}.{}.{}",
        vtec.office, vtec.phenomenon, vtec.significance, vtec.event_tracking_number
    )
}

fn parse_vtec_alert(value: &str) -> Option<ParsedWarningVtec> {
    let token = value
        .split_whitespace()
        .find(|token| token.trim().starts_with("/O.") && token.trim().ends_with('/'))?;
    let parts = token
        .trim()
        .trim_matches('/')
        .split('.')
        .collect::<Vec<_>>();
    if parts.len() < 7 || parts.first().copied() != Some("O") {
        return None;
    }
    let times = parts[6].split('-').collect::<Vec<_>>();
    Some(ParsedWarningVtec {
        action: parts[1].to_owned(),
        office: parts[2].to_owned(),
        phenomenon: parts[3].to_owned(),
        significance: parts[4].to_owned(),
        event_tracking_number: parts[5].to_owned(),
        start_time: times.first().and_then(|value| parse_vtec_time(value)),
        end_time: times.get(1).and_then(|value| parse_vtec_time(value)),
    })
}

fn collect_hazard_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if !path.is_dir() {
        return Err(format!("Hazard path not found: {}", path.display()));
    }

    let mut files = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("Cannot read hazard dir {}: {err}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|err| err.to_string())?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn parse_hazard_records_from_text(
    path: &Path,
    text: &str,
    query_time_utc: Option<DateTime<Utc>>,
) -> Vec<HazardRecord> {
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let heading = lines
        .iter()
        .find(|line| looks_like_wmo_heading(line.trim()))
        .map(|line| line.trim().to_owned());
    let awips_id = heading
        .as_deref()
        .and_then(|heading| lines.iter().position(|line| line.trim() == heading))
        .and_then(|index| lines.get(index + 1))
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty());

    let mut records = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let Some(vtec) = parse_warning_vtec_line(line) else {
            continue;
        };
        let segment_end = lines
            .iter()
            .enumerate()
            .skip(line_index + 1)
            .find_map(|(index, candidate)| (candidate.trim() == "$$").then_some(index))
            .unwrap_or(lines.len());
        let segment = &lines[line_index..segment_end];
        let points = parse_lat_lon_points(segment);
        if points.len() < 3 {
            continue;
        }
        let bbox = hazard_bbox(&points);
        let tags = parse_warning_tags(segment);
        let event_family = hazard_family_from_phenomenon(&vtec.phenomenon).to_owned();
        let lifecycle_status =
            hazard_lifecycle_status(&vtec.action, vtec.start_time, vtec.end_time, query_time_utc);
        let label = hazard_label(&event_family, &vtec.event_tracking_number, &tags);
        records.push(HazardRecord {
            event_id: format!(
                "{}.{}.{}.{}",
                vtec.office, vtec.phenomenon, vtec.significance, vtec.event_tracking_number
            ),
            label,
            event_family,
            action: vtec.action,
            lifecycle_status,
            office: vtec.office,
            headline: find_warning_headline(segment)
                .or(awips_id.clone())
                .or(heading.clone()),
            source_url: None,
            area: None,
            motion: find_prefixed_line(segment, "TIME...MOT...LOC"),
            details: Vec::new(),
            valid_start: vtec.start_time.map(format_utc_seconds),
            valid_end: vtec.end_time.map(format_utc_seconds),
            severity: None,
            certainty: None,
            urgency: None,
            tornado: tags.tornado,
            hail_inches: tags.hail_inches,
            wind_mph: tags.wind_mph,
            damage_threat: tags.damage_threat,
            points,
            bbox,
        });
    }
    if records.is_empty()
        && let Some(record) = parse_generic_lat_lon_hazard(path, &lines, heading, awips_id)
    {
        records.push(record);
    }
    records
}

fn parse_generic_lat_lon_hazard(
    path: &Path,
    lines: &[&str],
    heading: Option<String>,
    awips_id: Option<String>,
) -> Option<HazardRecord> {
    let points = parse_lat_lon_points(lines);
    if points.len() < 3 {
        return None;
    }
    let text = lines.join("\n").to_ascii_uppercase();
    let event_family = classify_generic_hazard_family(&text, awips_id.as_deref());
    let label = generic_hazard_label(&event_family, &text, awips_id.as_deref(), path);
    let headline = find_generic_headline(lines, &event_family)
        .or(awips_id)
        .or(heading);
    Some(HazardRecord {
        event_id: format!(
            "{}:{}",
            event_family.replace(' ', "-"),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("text-polygon")
        ),
        label,
        event_family,
        action: "TEXT".to_owned(),
        lifecycle_status: None,
        office: generic_office_from_heading(lines).unwrap_or_else(|| "NWS".to_owned()),
        headline,
        source_url: None,
        area: None,
        motion: find_prefixed_line(lines, "TIME...MOT...LOC"),
        details: Vec::new(),
        valid_start: None,
        valid_end: None,
        severity: None,
        certainty: None,
        urgency: None,
        tornado: None,
        hail_inches: None,
        wind_mph: None,
        damage_threat: None,
        bbox: hazard_bbox(&points),
        points,
    })
}

fn parse_warning_vtec_line(line: &str) -> Option<ParsedWarningVtec> {
    let trimmed = line.trim();
    let parsed = parse_vtec_alert(trimmed)?;
    if parsed.significance != "W" {
        return None;
    }
    Some(parsed)
}

fn parse_vtec_time(value: &str) -> Option<DateTime<Utc>> {
    let datetime = NaiveDateTime::parse_from_str(value, "%y%m%dT%H%MZ").ok()?;
    Some(Utc.from_utc_datetime(&datetime))
}

fn parse_lat_lon_points(lines: &[&str]) -> Vec<HazardPoint> {
    let Some(start_index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("LAT...LON"))
    else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    for line in &lines[start_index..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "$$" {
            break;
        }
        if trimmed.contains("...") && !trimmed.starts_with("LAT...LON") {
            break;
        }
        let body = trimmed.strip_prefix("LAT...LON").unwrap_or(trimmed);
        for token in body.split_whitespace() {
            if token.as_bytes().iter().all(u8::is_ascii_digit) {
                tokens.push(token);
            }
        }
    }
    if tokens.iter().all(|token| token.len() >= 8) {
        tokens
            .iter()
            .filter_map(|token| parse_compact_lat_lon_token(token))
            .collect()
    } else {
        tokens
            .chunks_exact(2)
            .filter_map(|pair| {
                let lat = parse_coordinate_hundredths(pair[0], false)?;
                let lon = parse_coordinate_hundredths(pair[1], true)?;
                Some(HazardPoint { lon, lat })
            })
            .collect()
    }
}

fn parse_coordinate_hundredths(value: &str, west_longitude: bool) -> Option<f32> {
    let number = value.parse::<i32>().ok()?;
    let coordinate = number as f32 / 100.0;
    Some(if west_longitude {
        -coordinate
    } else {
        coordinate
    })
}

fn parse_compact_lat_lon_token(value: &str) -> Option<HazardPoint> {
    if value.len() < 8 || !value.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    let lat = parse_coordinate_hundredths(&value[..4], false)?;
    let mut longitude_abs = value[4..].parse::<i32>().ok()? as f32 / 100.0;
    // SPC/NWS compact LAT...LON tokens use four longitude digits even west
    // of 100W, omitting the leading "1" (e.g. 35880436 = 35.88N 104.36W).
    // U.S. products should never legitimately land near 0-60W, so restore
    // the missing hundreds digit while keeping eastern CONUS tokens like
    // 36377580 (= 75.80W) unchanged.
    if longitude_abs < 60.0 {
        longitude_abs += 100.0;
    }
    let lon = -longitude_abs;
    Some(HazardPoint { lon, lat })
}

fn parse_warning_tags(lines: &[&str]) -> ParsedWarningTags {
    let mut tags = ParsedWarningTags::default();
    for line in lines {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("TORNADO...") {
            tags.tornado = Some(value.trim().to_owned());
        } else if let Some(value) = trimmed.strip_prefix("MAX HAIL SIZE...") {
            tags.hail_inches = parse_leading_float(value);
        } else if let Some(value) = trimmed.strip_prefix("MAX WIND GUST...") {
            tags.wind_mph = parse_leading_u16(value);
        } else if let Some(value) = trimmed
            .strip_prefix("TORNADO DAMAGE THREAT...")
            .or_else(|| trimmed.strip_prefix("THUNDERSTORM DAMAGE THREAT..."))
            .or_else(|| trimmed.strip_prefix("TSTM DAMAGE THREAT..."))
            .or_else(|| trimmed.strip_prefix("FLASH FLOOD DAMAGE THREAT..."))
        {
            tags.damage_threat = canonical_damage_threat(value);
        }
    }
    tags
}

fn canonical_damage_threat(value: &str) -> Option<String> {
    let upper = value.trim().to_ascii_uppercase();
    if upper.is_empty() {
        None
    } else if upper.contains("CATASTROPHIC") {
        Some("CATASTROPHIC".to_owned())
    } else if upper.contains("CONSIDERABLE") {
        Some("CONSIDERABLE".to_owned())
    } else if upper.contains("DESTRUCTIVE") {
        Some("DESTRUCTIVE".to_owned())
    } else {
        Some(upper)
    }
}

fn parse_leading_float(value: &str) -> Option<f32> {
    value
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<f32>().ok())
}

fn parse_leading_u16(value: &str) -> Option<u16> {
    value
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<u16>().ok())
}

fn find_warning_headline(lines: &[&str]) -> Option<String> {
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        ((trimmed.ends_with("Warning") || trimmed.ends_with("Statement"))
            && !trimmed.starts_with('*'))
        .then(|| trimmed.to_owned())
    })
}

fn find_generic_headline(lines: &[&str], event_family: &str) -> Option<String> {
    let needle = event_family.to_ascii_uppercase();
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        let upper = trimmed.to_ascii_uppercase();
        (!trimmed.is_empty() && upper.contains(&needle)).then(|| trimmed.to_owned())
    })
}

fn find_prefixed_line(lines: &[&str], prefix: &str) -> Option<String> {
    lines.iter().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .starts_with(prefix)
            .then(|| trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

fn generic_office_from_heading(lines: &[&str]) -> Option<String> {
    lines
        .iter()
        .find(|line| looks_like_wmo_heading(line.trim()))
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_owned)
}

fn looks_like_wmo_heading(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(ttaaii) = parts.next() else {
        return false;
    };
    let Some(cccc) = parts.next() else {
        return false;
    };
    let Some(time) = parts.next() else {
        return false;
    };
    ttaaii.len() == 6
        && cccc.len() == 4
        && time.len() == 6
        && ttaaii.as_bytes().iter().all(u8::is_ascii_alphanumeric)
        && cccc.as_bytes().iter().all(u8::is_ascii_alphabetic)
        && time.as_bytes().iter().all(u8::is_ascii_digit)
}

fn classify_generic_hazard_family(text: &str, awips_id: Option<&str>) -> String {
    let awips_id = awips_id.unwrap_or_default().to_ascii_uppercase();
    if text.contains("MESOSCALE DISCUSSION") || awips_id.contains("MCD") {
        "mesoscale discussion".to_owned()
    } else if text.contains("TORNADO WATCH")
        || text.contains("SEVERE THUNDERSTORM WATCH")
        || text.contains("WATCH OUTLINE UPDATE")
        || awips_id.starts_with("SEL")
        || awips_id.starts_with("SAW")
    {
        "watch".to_owned()
    } else if text.contains("LOCAL STORM REPORT") || awips_id.starts_with("LSR") {
        "local storm report".to_owned()
    } else {
        "text polygon".to_owned()
    }
}

fn generic_hazard_label(
    event_family: &str,
    text: &str,
    awips_id: Option<&str>,
    path: &Path,
) -> String {
    match event_family {
        "mesoscale discussion" => first_number_after(text, "MESOSCALE DISCUSSION")
            .map(|number| format!("MD {number}"))
            .unwrap_or_else(|| "MD".to_owned()),
        "watch" => first_number_after(text, "WATCH NUMBER")
            .or_else(|| first_number_after(text, "WATCH OUTLINE UPDATE FOR WS"))
            .map(|number| format!("WATCH {number}"))
            .unwrap_or_else(|| "WATCH".to_owned()),
        "local storm report" => "LSR".to_owned(),
        _ => awips_id
            .map(str::to_owned)
            .or_else(|| {
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "POLYGON".to_owned()),
    }
}

fn first_number_after(text: &str, marker: &str) -> Option<String> {
    let offset = text.find(marker)? + marker.len();
    text[offset..]
        .split(|character: char| !character.is_ascii_digit())
        .find(|token| !token.is_empty())
        .map(|token| {
            let trimmed = token.trim_start_matches('0');
            if trimmed.is_empty() { "0" } else { trimmed }.to_owned()
        })
}

fn hazard_lifecycle_status(
    action: &str,
    start_time: Option<DateTime<Utc>>,
    end_time: Option<DateTime<Utc>>,
    query_time_utc: Option<DateTime<Utc>>,
) -> Option<String> {
    if matches!(action, "CAN" | "EXP") {
        return Some(
            if action == "CAN" {
                "Canceled"
            } else {
                "Expired"
            }
            .to_owned(),
        );
    }
    let query_time_utc = query_time_utc?;
    if let Some(start_time) = start_time
        && query_time_utc < start_time
    {
        return Some("Pending".to_owned());
    }
    if let Some(end_time) = end_time
        && query_time_utc >= end_time
    {
        return Some("Expired".to_owned());
    }
    Some("Active".to_owned())
}

fn hazard_family_from_phenomenon(phenomenon: &str) -> &'static str {
    match phenomenon {
        "TO" => "tornado",
        "SV" => "severe thunderstorm",
        "FF" => "flash flood",
        "MA" => "special marine",
        "SQ" => "snow squall",
        "FW" => "fire weather",
        "FL" | "FA" => "flood",
        // Coastal tropical-cyclone products overlap flood polygons heavily.
        // Keep them canonical so they never fall through to the yellow
        // generic-alert style while archive/live feeds converge.
        "TR" | "TI" => "tropical storm",
        "HU" | "HI" | "TY" | "HF" => "hurricane",
        _ => "warning",
    }
}

pub(crate) fn hazard_family_order(family: &str) -> u8 {
    match family {
        "tornado" => 0,
        "severe thunderstorm" => 1,
        "flash flood" => 2,
        "flood" => 3,
        "tropical storm" => 4,
        "hurricane" => 5,
        "fire weather" => 6,
        "special marine" => 7,
        "snow squall" => 8,
        "watch" => 9,
        "mesoscale discussion" => 10,
        "local storm report" => 11,
        "special weather" => 12,
        _ => 11,
    }
}

pub(crate) fn hazard_family_menu_label(family: &str) -> String {
    if family == "meteoalarm" {
        return "EU".to_owned();
    }
    HAZARD_FILTER_FAMILIES
        .iter()
        .find_map(|(known_family, label)| (*known_family == family).then_some((*label).to_owned()))
        .unwrap_or_else(|| family.to_owned())
}

pub(crate) fn hazard_family_has_user_filter(family: &str) -> bool {
    HAZARD_FILTER_FAMILIES
        .iter()
        .any(|(known_family, _)| *known_family == family)
}

fn hazard_label(
    event_family: &str,
    event_tracking_number: &str,
    tags: &ParsedWarningTags,
) -> String {
    // IBW damage-threat escalations get said OUT LOUD: catastrophic
    // tornado = Tornado Emergency, considerable = PDS.
    let threat = tags
        .damage_threat
        .as_deref()
        .and_then(canonical_damage_threat)
        .unwrap_or_default();
    let prefix = match event_family {
        "tornado" => match threat.as_str() {
            "CATASTROPHIC" => "TOR EMERGENCY",
            "CONSIDERABLE" => "PDS TOR",
            _ => "TOR",
        },
        "severe thunderstorm" => match threat.as_str() {
            "DESTRUCTIVE" => "SVR DESTRUCTIVE",
            _ => "SVR",
        },
        "flash flood" => match threat.as_str() {
            "CATASTROPHIC" => "FF EMERGENCY",
            _ => "FFW",
        },
        "flood" => "FLW",
        "tropical storm" => "TRW",
        "hurricane" => "HUW",
        "fire weather" => "FIRE",
        "special marine" => "SMW",
        "snow squall" => "SQW",
        _ => "WRN",
    };
    if let Some(tornado) = &tags.tornado {
        format!("{prefix} {event_tracking_number} {tornado}")
    } else {
        format!("{prefix} {event_tracking_number}")
    }
}

/// [r,g,b,a] style color (toolkit-agnostic `styles` crate) → egui.
pub(crate) fn style_color32(rgba: styles::Rgba) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(rgba[0], rgba[1], rgba[2], rgba[3])
}

pub(crate) fn hazard_style_keys() -> impl Iterator<Item = &'static str> {
    styles::HAZARD_FAMILIES
        .iter()
        .copied()
        .chain(styles::HAZARD_ESCALATIONS.iter().copied())
}

pub(crate) fn hazard_style_key_known(key: &str) -> bool {
    hazard_style_keys().any(|known| known == key)
}

pub(crate) fn hazard_style_label(key: &str) -> String {
    match key {
        "tornado" => "Tornado warning".to_owned(),
        "tornado/catastrophic" => "Tornado emergency".to_owned(),
        "tornado/considerable" => "PDS tornado".to_owned(),
        "tornado/observed" => "Observed tornado warning".to_owned(),
        "severe-thunderstorm" => "Severe thunderstorm warning".to_owned(),
        "severe-thunderstorm/considerable" => "Considerable severe thunderstorm".to_owned(),
        "severe-thunderstorm/destructive" => "Destructive severe thunderstorm".to_owned(),
        "flash-flood" => "Flash flood warning".to_owned(),
        "flash-flood/considerable" => "Considerable flash flood".to_owned(),
        "flash-flood/catastrophic" => "Flash flood emergency".to_owned(),
        "flood" => "Flood warning".to_owned(),
        "flood/considerable" => "Considerable flood".to_owned(),
        "flood/catastrophic" => "Catastrophic flood".to_owned(),
        "tropical-storm" => "Tropical storm warning".to_owned(),
        "hurricane" => "Hurricane / typhoon warning".to_owned(),
        "fire-weather" => "Fire weather warning/watch".to_owned(),
        "special-marine" => "Special marine warning".to_owned(),
        "snow-squall" => "Snow squall warning".to_owned(),
        "watch" => "Watch polygons".to_owned(),
        "watch/tornado" => "Tornado watch".to_owned(),
        "watch/severe-thunderstorm" => "Severe thunderstorm watch".to_owned(),
        "mesoscale-discussion" => "Mesoscale discussions".to_owned(),
        "local-storm-report" => "Local storm reports".to_owned(),
        "special-weather" => "Special weather statements".to_owned(),
        "text-polygon" => "Text-product polygons".to_owned(),
        "other" => "Other polygons".to_owned(),
        _ => key.replace(['-', '/'], " "),
    }
}

pub(crate) fn hazard_style_resolved_polygon(
    registry: &styles::StyleRegistry,
    key: &str,
) -> styles::PolygonStyle {
    if let Some((family, threat)) = key.split_once('/') {
        registry.hazard_polygon(family, Some(threat)).clone()
    } else {
        registry.hazard_polygon(key, None).clone()
    }
}

pub(crate) fn hazard_record_style_threat(record: &HazardRecord) -> Option<&str> {
    if record.event_family != "watch" {
        // Keep IBW escalations (PDS / emergency) authoritative. Ordinary
        // observed TORs get their own independently customizable border.
        if record.damage_threat.is_some() {
            return record.damage_threat.as_deref();
        }
        if record.event_family == "tornado"
            && record
                .tornado
                .as_deref()
                .is_some_and(|tag| tag.eq_ignore_ascii_case("OBSERVED"))
        {
            return Some("observed");
        }
        return None;
    }
    if hazard_record_text_contains(record, "TORNADO WATCH")
        || record.event_id.to_ascii_uppercase().contains(".TO.A.")
    {
        return Some("tornado");
    }
    if hazard_record_text_contains(record, "SEVERE THUNDERSTORM WATCH")
        || record.event_id.to_ascii_uppercase().contains(".SV.A.")
    {
        return Some("severe-thunderstorm");
    }
    record.damage_threat.as_deref()
}

/// Alerts-tab watch subtype. PDS is intentionally a distinct bucket rather
/// than double-counting as TOR/SVR, so each chip has unambiguous behavior.
pub(crate) fn hazard_watch_filter_key(record: &HazardRecord) -> &'static str {
    if record.event_family != "watch" {
        return "other";
    }
    if hazard_record_text_contains(record, "PARTICULARLY DANGEROUS SITUATION")
        || hazard_record_text_has_token(record, "PDS")
    {
        return "pds";
    }
    match hazard_record_style_threat(record) {
        Some("tornado") => "tornado",
        Some("severe-thunderstorm") => "severe-thunderstorm",
        _ => "other",
    }
}

fn hazard_record_text_has_token(record: &HazardRecord, token: &str) -> bool {
    [
        Some(record.label.as_str()),
        record.headline.as_deref(),
        record.area.as_deref(),
        record.motion.as_deref(),
    ]
    .into_iter()
    .chain(record.details.iter().map(|detail| Some(detail.as_str())))
    .flatten()
    .flat_map(|value| value.split(|character: char| !character.is_ascii_alphanumeric()))
    .any(|word| word.eq_ignore_ascii_case(token))
}

fn hazard_record_text_contains(record: &HazardRecord, needle: &str) -> bool {
    let needle = needle.to_ascii_uppercase();
    [
        Some(record.label.as_str()),
        record.headline.as_deref(),
        record.area.as_deref(),
        record.motion.as_deref(),
    ]
    .into_iter()
    .chain(record.details.iter().map(|detail| Some(detail.as_str())))
    .flatten()
    .any(|value| value.to_ascii_uppercase().contains(&needle))
}

pub(crate) fn hazard_dash_label(dash: styles::DashPattern) -> &'static str {
    match dash {
        styles::DashPattern::Solid => "Solid",
        styles::DashPattern::Dashed { .. } => "Dashed",
        styles::DashPattern::Dotted => "Dotted",
    }
}

/// Hazard stroke color from the style registry (built-in defaults are the
/// operational color language — Tornado Emergency purple, PDS tornado
/// magenta, destructive SVR deep orange; see styles crate defaults).
/// Free-function wrapper kept so tests pin colors mechanically.
#[cfg(test)]
pub(crate) fn hazard_color(
    registry: &styles::StyleRegistry,
    record: &HazardRecord,
) -> egui::Color32 {
    style_color32(
        registry
            .hazard_polygon(&record.event_family, hazard_record_style_threat(record))
            .stroke_color,
    )
}

/// Dashed outline over a CLOSED polyline: `Shape::dashed_line` over the
/// ring with the first point appended (egui's dashed line is open-ended).
pub(crate) fn push_dashed_closed_line(
    shapes: &mut Vec<egui::Shape>,
    points: &[egui::Pos2],
    stroke: egui::Stroke,
    dash: f32,
    gap: f32,
    rect: egui::Rect,
    min_limit_px: f32,
) {
    if points.len() < 2 {
        return;
    }
    if screen_polyline_has_jump(points, true, rect, min_limit_px) {
        for chunk in screen_polyline_chunks(points, true, rect, min_limit_px) {
            shapes.extend(egui::Shape::dashed_line(
                &chunk,
                stroke,
                dash.max(0.5),
                gap.max(0.5),
            ));
        }
        return;
    }
    let mut closed = Vec::with_capacity(points.len() + 1);
    closed.extend_from_slice(points);
    closed.push(points[0]);
    shapes.extend(egui::Shape::dashed_line(
        &closed,
        stroke,
        dash.max(0.5),
        gap.max(0.5),
    ));
}

pub(crate) fn push_solid_closed_line(
    shapes: &mut Vec<egui::Shape>,
    points: &[egui::Pos2],
    stroke: egui::Stroke,
    rect: egui::Rect,
    min_limit_px: f32,
) {
    if points.len() < 2 {
        return;
    }
    if screen_polyline_has_jump(points, true, rect, min_limit_px) {
        shapes.extend(
            screen_polyline_chunks(points, true, rect, min_limit_px)
                .into_iter()
                .filter(|chunk| chunk.len() >= 2)
                .map(|chunk| egui::Shape::line(chunk, stroke)),
        );
    } else {
        shapes.push(egui::Shape::closed_line(points.to_vec(), stroke));
    }
}

pub(crate) fn push_solid_open_line(
    shapes: &mut Vec<egui::Shape>,
    points: &[egui::Pos2],
    stroke: egui::Stroke,
    rect: egui::Rect,
    min_limit_px: f32,
) {
    if points.len() < 2 {
        return;
    }
    if screen_polyline_has_jump(points, false, rect, min_limit_px) {
        shapes.extend(
            screen_polyline_chunks(points, false, rect, min_limit_px)
                .into_iter()
                .filter(|chunk| chunk.len() >= 2)
                .map(|chunk| egui::Shape::line(chunk, stroke)),
        );
    } else {
        shapes.push(egui::Shape::line(points.to_vec(), stroke));
    }
}

/// `min_limit_px` raises the jump limit for shapes whose LEGITIMATE
/// edges can dwarf the viewport (a warning polygon zoomed far in);
/// pass 0.0 for the plain viewport-relative limit. Callers with geo
/// geometry derive it from the shape's own geographic size — a real
/// edge can never out-run its bbox at the current scale, while a
/// projection teleport (AEQD antipode wrap at world zoom) still
/// exceeds it and trips the cull.
fn screen_polyline_segment_limit_sq(rect: egui::Rect, min_limit_px: f32) -> f32 {
    let diagonal = rect.width().hypot(rect.height());
    let limit = (diagonal * SCREEN_POLYGON_MAX_SEGMENT_DIAGONAL_FRACTION)
        .max(SCREEN_POLYGON_MIN_MAX_SEGMENT_PX)
        .max(min_limit_px);
    limit * limit
}

fn screen_point_valid(point: egui::Pos2) -> bool {
    point.x.is_finite() && point.y.is_finite()
}

pub(crate) fn screen_polyline_has_jump(
    points: &[egui::Pos2],
    closed: bool,
    rect: egui::Rect,
    min_limit_px: f32,
) -> bool {
    if points.iter().any(|point| !screen_point_valid(*point)) {
        return true;
    }
    let limit_sq = screen_polyline_segment_limit_sq(rect, min_limit_px);
    if points
        .windows(2)
        .any(|pair| pair[0].distance_sq(pair[1]) > limit_sq)
    {
        return true;
    }
    closed
        && points
            .first()
            .zip(points.last())
            .is_some_and(|(first, last)| first.distance_sq(*last) > limit_sq)
}

fn screen_polyline_chunks(
    points: &[egui::Pos2],
    closed: bool,
    rect: egui::Rect,
    min_limit_px: f32,
) -> Vec<Vec<egui::Pos2>> {
    let limit_sq = screen_polyline_segment_limit_sq(rect, min_limit_px);
    let mut chunks = Vec::<Vec<egui::Pos2>>::new();
    let mut current = Vec::<egui::Pos2>::new();
    for point in points
        .iter()
        .copied()
        .filter(|point| screen_point_valid(*point))
    {
        if let Some(previous) = current.last().copied()
            && previous.distance_sq(point) > limit_sq
        {
            if current.len() >= 2 {
                chunks.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        current.push(point);
    }
    if current.len() >= 2 {
        chunks.push(current);
    }
    if closed
        && chunks.len() == 1
        && let Some(chunk) = chunks.first_mut()
        && let (Some(first), Some(last)) = (chunk.first().copied(), chunk.last().copied())
        && last.distance_sq(first) <= limit_sq
        && last.distance_sq(first) > 0.01
    {
        chunk.push(first);
    }
    chunks
}

pub(crate) fn hazard_fill_alpha(base_alpha: u8, selected: bool) -> u8 {
    // Zero is an explicit master disable. Selection still gets its thicker
    // outline, but must not resurrect a fill the operator turned off.
    if base_alpha == 0 {
        0
    } else if selected {
        base_alpha.saturating_add(20).min(100)
    } else {
        base_alpha
    }
}

/// Upper bound (px) on a LEGITIMATE hazard edge's on-screen length:
/// the record's geographic bbox diagonal at the current scale plus
/// slack for AEQD's local distortion. Feeds the jump cull's
/// `min_limit_px` so zooming far into a warning polygon never falsely
/// flags its edges as projection jumps (field report: outlines broke
/// at extreme zoom), while true teleports still exceed it.
pub(crate) fn hazard_bbox_segment_allowance_px(bbox: [f32; 4], map_scale: f32) -> f32 {
    let mid_lat_cos = (((bbox[1] + bbox[3]) * 0.5).to_radians().cos()).max(0.2);
    let width_km = (bbox[2] - bbox[0]).abs() * 111.32 * mid_lat_cos;
    let height_km = (bbox[3] - bbox[1]).abs() * 111.32;
    let px_per_km = map_scale / 111.32;
    width_km.hypot(height_km) * px_per_km * 1.25 + 8.0
}

/// Jump limit (px) for a tropical cone-of-uncertainty's edges. A legitimate
/// cone edge can never out-run the cone's own geographic bbox at the current
/// scale, so derive the limit from that bbox (the same allowance hazard
/// polygons use) rather than the plain viewport. Without this a wide,
/// partly-off-screen cone (e.g. Super Typhoon BAVI-26's 27°-of-longitude cone
/// viewed zoomed-in near Guam) trips the all-or-nothing jump cull the instant
/// one coarse far-end edge projects longer than the viewport, and the ENTIRE
/// cone vanishes — fill and outline both. A genuine AEQD antimeridian/antipode
/// teleport still dwarfs the bbox allowance, so it is still culled. The
/// viewport floor keeps a small near-center cone free to span the view.
pub(crate) fn cone_segment_jump_limit_px(
    cone_bbox_deg: [f32; 4],
    map_scale: f32,
    rect: egui::Rect,
) -> f32 {
    hazard_bbox_segment_allowance_px(cone_bbox_deg, map_scale).max(rect.width().max(rect.height()))
}

/// Build a storm cone's translucent fill mesh + outline from its already-
/// projected screen ring. Split out of `draw_tropical` so the wide /
/// partly-off-screen culling behavior is unit-testable without a live painter.
/// `cone_bbox_deg` is the cone ring's geographic bbox `[west, south, east,
/// north]`; it (with `map_scale`) sets the per-edge jump limit so a real cone
/// survives at any zoom while a true projection teleport is still dropped.
pub(crate) fn cone_overlay_shapes(
    ring: &[egui::Pos2],
    cone_bbox_deg: [f32; 4],
    map_scale: f32,
    rect: egui::Rect,
    fill: egui::Color32,
    outline: egui::Stroke,
) -> Vec<egui::Shape> {
    let mut shapes = Vec::new();
    if ring.len() < 3 {
        return shapes;
    }
    let jump_px = cone_segment_jump_limit_px(cone_bbox_deg, map_scale, rect);
    if !screen_polyline_has_jump(ring, true, rect, jump_px)
        && let Some(mesh) = filled_polygon_mesh(ring, fill)
    {
        shapes.push(egui::Shape::mesh(mesh));
    }
    push_solid_closed_line(&mut shapes, ring, outline, rect, jump_px);
    shapes
}

pub(crate) fn hazard_bbox(points: &[HazardPoint]) -> [f32; 4] {
    let mut west = f32::INFINITY;
    let mut south = f32::INFINITY;
    let mut east = f32::NEG_INFINITY;
    let mut north = f32::NEG_INFINITY;
    for point in points {
        west = west.min(point.lon);
        east = east.max(point.lon);
        south = south.min(point.lat);
        north = north.max(point.lat);
    }
    [west, south, east, north]
}

pub(crate) fn hazard_points_renderable(points: &[HazardPoint]) -> bool {
    if points.len() < 3 {
        return false;
    }
    if points.iter().any(|point| {
        !point.lon.is_finite()
            || !point.lat.is_finite()
            || point.lon < -180.0
            || point.lon > 180.0
            || point.lat < -90.0
            || point.lat > 90.0
    }) {
        return false;
    }

    let bbox = hazard_bbox(points);
    if bbox[2] - bbox[0] > HAZARD_MAX_RENDER_LON_SPAN_DEG
        || bbox[3] - bbox[1] > HAZARD_MAX_RENDER_LAT_SPAN_DEG
    {
        return false;
    }

    let mut previous = points[points.len() - 1];
    for current in points {
        if hazard_point_distance_km(previous, *current) > HAZARD_MAX_RENDER_EDGE_KM {
            return false;
        }
        previous = *current;
    }
    true
}

fn sanitize_weather_alert_ring(points: Vec<HazardPoint>, event_family: &str) -> Vec<HazardPoint> {
    if !weather_alert_family_uses_spiky_ring_sanitize(event_family)
        || !generic_alert_ring_needs_hull(&points)
    {
        return points;
    }
    let hull = hazard_convex_hull(&points);
    if hull.len() >= 3 { hull } else { points }
}

fn weather_alert_family_uses_spiky_ring_sanitize(event_family: &str) -> bool {
    matches!(event_family, "alert" | "watch" | "flood" | "fire weather")
}

fn generic_alert_ring_needs_hull(points: &[HazardPoint]) -> bool {
    if points.len() < HAZARD_GENERIC_ALERT_SPIKY_MIN_POINTS {
        return false;
    }
    let hull = hazard_convex_hull(points);
    if hull.len() < 3 || hull.len().saturating_add(2) >= points.len() {
        return false;
    }
    let hull_perimeter = hazard_ring_perimeter_km(&hull);
    hull_perimeter > f32::EPSILON
        && hazard_ring_perimeter_km(points) / hull_perimeter
            >= HAZARD_GENERIC_ALERT_SPIKY_PATH_RATIO
}

fn hazard_ring_perimeter_km(points: &[HazardPoint]) -> f32 {
    if points.len() < 2 {
        return 0.0;
    }
    let mut previous = points[points.len() - 1];
    let mut total = 0.0;
    for point in points {
        total += hazard_point_distance_km(previous, *point);
        previous = *point;
    }
    total
}

fn hazard_convex_hull(points: &[HazardPoint]) -> Vec<HazardPoint> {
    let mut unique = points.to_vec();
    unique.sort_by(|left, right| {
        left.lon
            .total_cmp(&right.lon)
            .then_with(|| left.lat.total_cmp(&right.lat))
    });
    unique.dedup_by(|left, right| {
        (left.lon - right.lon).abs() <= f32::EPSILON && (left.lat - right.lat).abs() <= f32::EPSILON
    });
    if unique.len() <= 3 {
        return unique;
    }

    let mut lower = Vec::<HazardPoint>::new();
    for point in &unique {
        while lower.len() >= 2
            && hazard_point_cross(lower[lower.len() - 2], lower[lower.len() - 1], *point) <= 0.0
        {
            lower.pop();
        }
        lower.push(*point);
    }

    let mut upper = Vec::<HazardPoint>::new();
    for point in unique.iter().rev() {
        while upper.len() >= 2
            && hazard_point_cross(upper[upper.len() - 2], upper[upper.len() - 1], *point) <= 0.0
        {
            upper.pop();
        }
        upper.push(*point);
    }

    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

fn hazard_point_cross(origin: HazardPoint, a: HazardPoint, b: HazardPoint) -> f32 {
    (a.lon - origin.lon) * (b.lat - origin.lat) - (a.lat - origin.lat) * (b.lon - origin.lon)
}

fn hazard_point_distance_km(a: HazardPoint, b: HazardPoint) -> f32 {
    const EARTH_RADIUS_KM: f32 = 6_371.0;
    let lat1 = a.lat.to_radians();
    let lat2 = b.lat.to_radians();
    let dlat = (b.lat - a.lat).to_radians();
    let dlon = (b.lon - a.lon).to_radians();
    let half_dlat = (dlat * 0.5).sin();
    let half_dlon = (dlon * 0.5).sin();
    let h = half_dlat * half_dlat + lat1.cos() * lat2.cos() * half_dlon * half_dlon;
    2.0 * EARTH_RADIUS_KM * h.clamp(0.0, 1.0).sqrt().asin()
}

pub(crate) fn bbox_contains(bbox: [f32; 4], lon: f32, lat: f32) -> bool {
    lon >= bbox[0] && lon <= bbox[2] && lat >= bbox[1] && lat <= bbox[3]
}

pub(crate) fn hazard_polygon_contains_point(points: &[HazardPoint], point: HazardPoint) -> bool {
    if points.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = points[points.len() - 1];
    for current in points {
        let crosses = (current.lat > point.lat) != (previous.lat > point.lat);
        if crosses {
            let lon_at_lat = (previous.lon - current.lon) * (point.lat - current.lat)
                / (previous.lat - current.lat)
                + current.lon;
            if point.lon < lon_at_lat {
                inside = !inside;
            }
        }
        previous = *current;
    }
    inside
}

pub(crate) fn is_convex_screen_polygon(points: &[egui::Pos2]) -> bool {
    if points.len() < 4 {
        return true;
    }
    let mut sign = 0.0f32;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        let c = points[(index + 2) % points.len()];
        let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if cross.abs() <= f32::EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if sign != cross.signum() {
            return false;
        }
    }
    true
}

pub(crate) fn filled_polygon_mesh(
    points: &[egui::Pos2],
    fill: egui::Color32,
) -> Option<egui::epaint::Mesh> {
    if fill == egui::Color32::TRANSPARENT {
        return None;
    }
    let points = cleaned_screen_polygon(points);
    if points.len() < 3 {
        return None;
    }
    // A ring that still self-intersects after cleaning has no meaningful
    // ear-clip triangulation: any ear-based tessellator bridges the crossing
    // and paints outside the ring. Real inputs do this — screen quantization
    // folds dense coastline-traced zone rings at low zoom (Cape May NJC009
    // at map_scale 60-120), and CAP polygons have shipped crossed vertex
    // orders — so those rings take the exact even-odd scanline fill instead,
    // and when even the scanline bails (degenerate band structure, the
    // vertex-limit valve) the fill is dropped outright: outline-only beats
    // handing a known-crossed ring to earcut and painting wedges outside it.
    if screen_ring_self_intersects(&points) {
        return scanline_fill_mesh(std::slice::from_ref(&points), fill);
    }
    let triangles = triangulate_screen_polygon(&points)?;
    let mut mesh = egui::epaint::Mesh::default();
    for point in &points {
        mesh.colored_vertex(*point, fill);
    }
    for [a, b, c] in triangles {
        mesh.add_triangle(a as u32, b as u32, c as u32);
    }
    Some(mesh)
}

pub(crate) fn filled_polygon_with_holes_mesh(
    outer: &[egui::Pos2],
    holes: &[Vec<egui::Pos2>],
    fill: egui::Color32,
) -> Option<egui::epaint::Mesh> {
    if fill == egui::Color32::TRANSPARENT {
        return None;
    }
    let outer = cleaned_screen_polygon(outer);
    if outer.len() < 3 {
        return None;
    }
    let mut rings = Vec::with_capacity(1 + holes.len());
    rings.push(outer);
    for hole in holes {
        let hole = cleaned_screen_polygon(hole);
        if hole.len() >= 3 {
            rings.push(hole);
        }
    }

    let mut vertices = Vec::new();
    let mut hole_indices = Vec::new();
    let mut points = Vec::new();
    for (ring_index, ring) in rings.iter().enumerate() {
        if ring_index > 0 {
            hole_indices.push(points.len());
        }
        for point in ring {
            vertices.push(point.x as f64);
            vertices.push(point.y as f64);
            points.push(*point);
        }
    }
    let triangles = earcutr::earcut(&vertices, &hole_indices, 2).ok()?;
    if triangles.len() < 3 {
        return None;
    }
    let mut mesh = egui::epaint::Mesh::default();
    for point in &points {
        mesh.colored_vertex(*point, fill);
    }
    for triangle in triangles.chunks_exact(3) {
        mesh.add_triangle(triangle[0] as u32, triangle[1] as u32, triangle[2] as u32);
    }
    Some(mesh)
}

fn screen_points_bbox(points: &[egui::Pos2]) -> egui::Rect {
    let mut rect = egui::Rect::NOTHING;
    for point in points {
        rect.extend_with(*point);
    }
    rect
}

/// The legacy per-record fill shape: convex fast path (feathered by epaint)
/// or the robust mesh tessellation.
fn single_hazard_fill_shape(candidate: &HazardFillCandidate) -> Option<egui::Shape> {
    if is_convex_screen_polygon(&candidate.points) {
        Some(egui::Shape::convex_polygon(
            candidate.points.clone(),
            candidate.fill,
            egui::Stroke::NONE,
        ))
    } else {
        filled_polygon_mesh(&candidate.points, candidate.fill).map(egui::Shape::mesh)
    }
}

/// Emit hazard fill shapes, flattening fills of the SAME family and color so
/// overlapping warnings paint their overlap once at the family alpha instead
/// of stacking toward opaque — the reported "glow blobs" (the real July 5
/// 2026 KCTP scene stacks three successive SVR fills over central PA).
/// Cross-family and cross-threat overlap still layers: those are distinct
/// hazards deliberately drawn over each other. Within a group only records
/// whose fills can touch (transitively bbox-overlapping components) share a
/// union mesh; isolated records — and components too big to flatten within
/// [`HAZARD_FLATTEN_COMPONENT_EDGE_BUDGET`] — keep the legacy per-record
/// path.
pub(crate) fn append_flattened_hazard_fill_shapes(
    candidates: Vec<HazardFillCandidate>,
    fill_shapes: &mut Vec<egui::Shape>,
) {
    let mut groups: Vec<((&str, egui::Color32), Vec<usize>)> = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let key = (candidate.family.as_str(), candidate.fill);
        if let Some((_, members)) = groups.iter_mut().find(|(existing, _)| *existing == key) {
            members.push(index);
        } else {
            groups.push((key, vec![index]));
        }
    }
    for (_, members) in groups {
        let bboxes = members
            .iter()
            .map(|&member| screen_points_bbox(&candidates[member].points))
            .collect::<Vec<_>>();
        // Transitive bbox-overlap components within the group (indices into
        // `members`).
        let mut components: Vec<Vec<usize>> = Vec::new();
        for local in 0..members.len() {
            let mut merged = vec![local];
            let mut keep = Vec::with_capacity(components.len());
            for component in components.drain(..) {
                if component
                    .iter()
                    .any(|&other| bboxes[local].intersects(bboxes[other]))
                {
                    merged.extend(component);
                } else {
                    keep.push(component);
                }
            }
            keep.push(merged);
            components = keep;
        }
        for mut component in components {
            component.sort_unstable();
            // Cheap pre-flight bound BEFORE the flatten's quadratic edge
            // scan (see HAZARD_FLATTEN_COMPONENT_EDGE_BUDGET): an oversized
            // component falls back to per-record fills, like a singleton.
            let component_edges: usize = component
                .iter()
                .map(|&local| candidates[members[local]].points.len())
                .sum();
            if component.len() == 1 || component_edges > HAZARD_FLATTEN_COMPONENT_EDGE_BUDGET {
                for &local in &component {
                    if let Some(shape) = single_hazard_fill_shape(&candidates[members[local]]) {
                        fill_shapes.push(shape);
                    }
                }
                continue;
            }
            let rings = component
                .iter()
                .map(|&local| candidates[members[local]].points.clone())
                .collect::<Vec<_>>();
            let fill = candidates[members[component[0]]].fill;
            if let Some(mesh) = scanline_fill_mesh(&rings, fill) {
                fill_shapes.push(egui::Shape::mesh(mesh));
            } else {
                // Safety-valve fallback: paint per record as before.
                for &local in &component {
                    if let Some(shape) = single_hazard_fill_shape(&candidates[members[local]]) {
                        fill_shapes.push(shape);
                    }
                }
            }
        }
    }
}

pub(crate) fn screen_polygon_bbox_intersects(points: &[egui::Pos2], rect: egui::Rect) -> bool {
    let Some(first) = points.first() else {
        return false;
    };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.x, first.y, first.x, first.y);
    for point in points.iter().skip(1) {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    egui::Rect::from_min_max(egui::pos2(min_x, min_y), egui::pos2(max_x, max_y)).intersects(rect)
}

pub(crate) fn draw_outlook_ring(
    painter: &egui::Painter,
    screen: &[egui::Pos2],
    stroke: egui::Stroke,
    rect: egui::Rect,
) {
    if screen.len() < 2 {
        return;
    }
    if draw_clipped_outlook_ring(painter, screen, stroke, rect) {
        return;
    }
    if screen_polyline_has_jump(screen, true, rect, 0.0) {
        for chunk in screen_polyline_chunks(screen, true, rect, 0.0) {
            painter.add(egui::Shape::line(chunk, stroke));
        }
        return;
    }
    let ring_is_closed = screen
        .first()
        .zip(screen.last())
        .map(|(first, last)| first.distance(*last) <= 0.5)
        .unwrap_or(false);
    if ring_is_closed {
        painter.add(egui::Shape::closed_line(screen.to_vec(), stroke));
    } else {
        painter.add(egui::Shape::line(screen.to_vec(), stroke));
    }
}

fn draw_clipped_outlook_ring(
    painter: &egui::Painter,
    screen: &[egui::Pos2],
    stroke: egui::Stroke,
    rect: egui::Rect,
) -> bool {
    if screen.len() < 2
        || !screen
            .iter()
            .any(|point| !rect.expand(2.0).contains(*point))
    {
        return false;
    }
    for (ca, cb) in clipped_outlook_ring_segments(screen, rect) {
        painter.line_segment([ca, cb], stroke);
    }
    true
}

/// Clip each ring edge to the viewport, keeping every plausible
/// neighbor-to-neighbor edge and culling only projection wraparound chords.
/// The cull threshold is the ring's OWN typical spacing (same length-outlier
/// idea as `screen_polyline_has_jump`, but ring-relative instead of
/// viewport-relative) so a legitimate outlook border edge that crosses the
/// whole viewport at storm zoom is never dropped — only a far-hemisphere
/// flip, enormous next to its ring's spacing, gets skipped.
fn clipped_outlook_ring_segments(
    screen: &[egui::Pos2],
    rect: egui::Rect,
) -> Vec<(egui::Pos2, egui::Pos2)> {
    let clip_rect = rect.expand(3.0);
    let segment_count = screen.len().saturating_sub(1);
    let mut spacings: Vec<f32> = (0..segment_count)
        .filter_map(|index| {
            let (a, b) = (screen[index], screen[index + 1]);
            (screen_point_valid(a) && screen_point_valid(b)).then(|| a.distance(b))
        })
        .collect();
    spacings.sort_by(f32::total_cmp);
    let typical_spacing = spacings
        .get(spacings.len() / 2)
        .copied()
        .unwrap_or_default();
    let diagonal = rect.width().hypot(rect.height());
    let wraparound_len = (typical_spacing * OUTLOOK_RING_WRAPAROUND_SPACING_MULTIPLE)
        .max(screen_polyline_segment_limit_sq(rect, 0.0).sqrt())
        .min(diagonal * OUTLOOK_RING_WRAPAROUND_MAX_DIAGONAL_MULTIPLE);
    let mut segments = Vec::new();
    for index in 0..segment_count {
        let a = screen[index];
        let b = screen[index + 1];
        if !screen_point_valid(a) || !screen_point_valid(b) {
            continue;
        }
        let Some((ca, cb)) = clip_segment_to_rect(a, b, clip_rect) else {
            continue;
        };
        if a.distance(b) > wraparound_len && !rect.contains(a) && !rect.contains(b) {
            continue;
        }
        segments.push((ca, cb));
    }
    segments
}

fn clip_segment_to_rect(
    a: egui::Pos2,
    b: egui::Pos2,
    rect: egui::Rect,
) -> Option<(egui::Pos2, egui::Pos2)> {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let mut t0: f32 = 0.0;
    let mut t1: f32 = 1.0;
    for (p, q) in [
        (-dx, a.x - rect.left()),
        (dx, rect.right() - a.x),
        (-dy, a.y - rect.top()),
        (dy, rect.bottom() - a.y),
    ] {
        if p.abs() <= f32::EPSILON {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let r = q / p;
        if p < 0.0 {
            if r > t1 {
                return None;
            }
            t0 = t0.max(r);
        } else {
            if r < t0 {
                return None;
            }
            t1 = t1.min(r);
        }
    }
    Some((
        egui::pos2(a.x + t0 * dx, a.y + t0 * dy),
        egui::pos2(a.x + t1 * dx, a.y + t1 * dy),
    ))
}

pub(crate) fn cleaned_screen_polygon(points: &[egui::Pos2]) -> Vec<egui::Pos2> {
    let mut cleaned = Vec::<egui::Pos2>::with_capacity(points.len());
    for point in points {
        if cleaned
            .last()
            .is_none_or(|previous| previous.distance_sq(*point) > 0.01)
        {
            cleaned.push(*point);
        }
    }
    if cleaned.len() > 1
        && cleaned
            .first()
            .zip(cleaned.last())
            .is_some_and(|(first, last)| first.distance_sq(*last) <= 0.01)
    {
        cleaned.pop();
    }
    cleaned
}

/// Triangulate a cleaned, non-self-intersecting screen ring via earcut (the
/// tessellator the outlook holes path already uses). Earcut tolerates the
/// degenerate vertex patterns real CAP rings carry — repeated vertices,
/// collinear runs, zero-area out-and-back excursions — where the previous
/// hand-rolled O(n^2) ear clip either found no ear (the whole fill silently
/// vanished) or emitted spike triangles across the defect.
fn triangulate_screen_polygon(points: &[egui::Pos2]) -> Option<Vec<[usize; 3]>> {
    if points.len() < 3 || points.len() > u32::MAX as usize {
        return None;
    }
    let mut vertices = Vec::with_capacity(points.len() * 2);
    for point in points {
        vertices.push(point.x as f64);
        vertices.push(point.y as f64);
    }
    let triangles = earcutr::earcut(&vertices, &[], 2).ok()?;
    if triangles.len() < 3 {
        return None;
    }
    Some(
        triangles
            .chunks_exact(3)
            .map(|triangle| [triangle[0], triangle[1], triangle[2]])
            .collect(),
    )
}

/// True when two segments properly cross (shared endpoints and collinear
/// overlaps do not count — earcut copes with those benign contacts).
fn segments_properly_cross(a1: egui::Pos2, a2: egui::Pos2, b1: egui::Pos2, b2: egui::Pos2) -> bool {
    if a1.x.max(a2.x) < b1.x.min(b2.x)
        || b1.x.max(b2.x) < a1.x.min(a2.x)
        || a1.y.max(a2.y) < b1.y.min(b2.y)
        || b1.y.max(b2.y) < a1.y.min(a2.y)
    {
        return false;
    }
    let d1 = cross_points(b1, b2, a1);
    let d2 = cross_points(b1, b2, a2);
    let d3 = cross_points(a1, a2, b1);
    let d4 = cross_points(a1, a2, b2);
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// O(n^2) proper-crossing scan over a ring's non-adjacent edge pairs, with a
/// bbox precheck per pair. Runs only when a fill is (re)tessellated, and
/// hazard rings are capped by [`HAZARD_FILL_VERTEX_LIMIT`].
pub(crate) fn screen_ring_self_intersects(points: &[egui::Pos2]) -> bool {
    let count = points.len();
    if count < 4 {
        return false;
    }
    for first in 0..count {
        let a1 = points[first];
        let a2 = points[(first + 1) % count];
        for second in (first + 2)..count {
            if first == 0 && second == count - 1 {
                continue;
            }
            let b1 = points[second];
            let b2 = points[(second + 1) % count];
            if segments_properly_cross(a1, a2, b1, b2) {
                return true;
            }
        }
    }
    false
}

/// Scanline y where two fill edges properly cross, if they do.
fn scanline_edge_crossing_y(a: &ScanlineFillEdge, b: &ScanlineFillEdge) -> Option<f32> {
    let d1x = a.x1 - a.x0;
    let d1y = a.y1 - a.y0;
    let d2x = b.x1 - b.x0;
    let d2y = b.y1 - b.y0;
    let denominator = d1x * d2y - d1y * d2x;
    if denominator == 0.0 {
        return None;
    }
    let t = ((b.x0 - a.x0) * d2y - (b.y0 - a.y0) * d2x) / denominator;
    let u = ((b.x0 - a.x0) * d1y - (b.y0 - a.y0) * d1x) / denominator;
    (t > 0.0 && t < 1.0 && u > 0.0 && u < 1.0).then_some(a.y0 + t * d1y)
}

/// Exact fill tessellation for possibly-degenerate screen rings: even-odd
/// inside each ring, union across rings, emitted as trapezoid bands between
/// consecutive vertex/crossing scanlines. Within a band no edge starts, ends,
/// or crosses another, so the interval structure at the band's midline holds
/// across it and the trapezoids tile the region EXACTLY ONCE — a translucent
/// fill never double-blends no matter how the rings overlap or self-cross.
/// This is both the self-intersecting-ring fill and the same-family hazard
/// overlap flattener.
pub(crate) fn scanline_fill_mesh(
    rings: &[Vec<egui::Pos2>],
    fill: egui::Color32,
) -> Option<egui::epaint::Mesh> {
    let mut edges = Vec::new();
    for (ring_index, ring) in rings.iter().enumerate() {
        if ring.len() < 3 {
            continue;
        }
        for index in 0..ring.len() {
            let a = ring[index];
            let b = ring[(index + 1) % ring.len()];
            if a.y != b.y && screen_point_valid(a) && screen_point_valid(b) {
                edges.push(ScanlineFillEdge {
                    x0: a.x,
                    y0: a.y,
                    x1: b.x,
                    y1: b.y,
                    ring: ring_index,
                });
            }
        }
    }
    if edges.is_empty() {
        return None;
    }

    let mut events = Vec::with_capacity(edges.len() * 2);
    for edge in &edges {
        events.push(edge.y0);
        events.push(edge.y1);
    }
    for first in 0..edges.len() {
        for second in (first + 1)..edges.len() {
            if let Some(y) = scanline_edge_crossing_y(&edges[first], &edges[second]) {
                events.push(y);
            }
        }
    }
    events.sort_by(f32::total_cmp);
    events.dedup();

    let x_at = |edge: &ScanlineFillEdge, y: f32| {
        edge.x0 + (y - edge.y0) * (edge.x1 - edge.x0) / (edge.y1 - edge.y0)
    };
    let mut mesh = egui::epaint::Mesh::default();
    let mut crossings = Vec::<(f32, usize)>::new();
    let mut intervals = Vec::<(usize, usize)>::new();
    for band in events.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        let midline = 0.5 * (y0 + y1);
        if !(y0 < midline && midline < y1) {
            continue;
        }
        intervals.clear();
        for ring_index in 0..rings.len() {
            crossings.clear();
            for (edge_index, edge) in edges.iter().enumerate() {
                if edge.ring == ring_index && (edge.y0 > midline) != (edge.y1 > midline) {
                    crossings.push((x_at(edge, midline), edge_index));
                }
            }
            crossings.sort_by(|left, right| left.0.total_cmp(&right.0));
            for pair in crossings.chunks_exact(2) {
                intervals.push((pair[0].1, pair[1].1));
            }
        }
        intervals.sort_by(|left, right| {
            x_at(&edges[left.0], midline).total_cmp(&x_at(&edges[right.0], midline))
        });
        let mut merged = Vec::<(usize, usize)>::new();
        for &(left, right) in &intervals {
            if let Some(active) = merged.last_mut()
                && x_at(&edges[left], midline) <= x_at(&edges[active.1], midline)
            {
                if x_at(&edges[right], midline) > x_at(&edges[active.1], midline) {
                    active.1 = right;
                }
            } else {
                merged.push((left, right));
            }
        }
        for (left, right) in merged {
            let left_edge = &edges[left];
            let right_edge = &edges[right];
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(egui::pos2(x_at(left_edge, y0), y0), fill);
            mesh.colored_vertex(egui::pos2(x_at(right_edge, y0), y0), fill);
            mesh.colored_vertex(egui::pos2(x_at(right_edge, y1), y1), fill);
            mesh.colored_vertex(egui::pos2(x_at(left_edge, y1), y1), fill);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
        if mesh.vertices.len() > SCANLINE_FILL_VERTEX_LIMIT {
            return None;
        }
    }
    (!mesh.indices.is_empty()).then_some(mesh)
}

pub(crate) fn cross_points(a: egui::Pos2, b: egui::Pos2, c: egui::Pos2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Test-only since the ear clip's removal: the runtime fill paths tessellate
/// via earcut or the scanline bands and never point-test triangles.
#[cfg(test)]
pub(crate) fn point_in_triangle(
    point: egui::Pos2,
    a: egui::Pos2,
    b: egui::Pos2,
    c: egui::Pos2,
) -> bool {
    let ab = cross_points(a, b, point);
    let bc = cross_points(b, c, point);
    let ca = cross_points(c, a, point);
    let has_negative = ab < -f32::EPSILON || bc < -f32::EPSILON || ca < -f32::EPSILON;
    let has_positive = ab > f32::EPSILON || bc > f32::EPSILON || ca > f32::EPSILON;
    !(has_negative && has_positive)
}

pub(crate) fn screen_polygon_contains_point(points: &[egui::Pos2], point: egui::Pos2) -> bool {
    if points.len() < 3 || !screen_point_valid(point) {
        return false;
    }
    let mut inside = false;
    let mut previous = points[points.len() - 1];
    for current in points.iter().copied() {
        if !screen_point_valid(current) || !screen_point_valid(previous) {
            previous = current;
            continue;
        }
        let crosses = (current.y > point.y) != (previous.y > point.y);
        if crosses {
            let x_at_y = (previous.x - current.x) * (point.y - current.y)
                / (previous.y - current.y)
                + current.x;
            if point.x < x_at_y {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

pub(crate) fn hazard_visible_label_anchor(
    points: &[egui::Pos2],
    rect: egui::Rect,
) -> Option<egui::Pos2> {
    if points.len() < 3 {
        return None;
    }
    let label_bounds = if rect.width() > 64.0 && rect.height() > 64.0 {
        rect.shrink(16.0)
    } else {
        rect
    };
    let centroid = polygon_screen_centroid(points);
    if label_bounds.contains(centroid) {
        return Some(centroid);
    }
    let center = label_bounds.center();
    if screen_polygon_contains_point(points, center) {
        return Some(center);
    }

    let mut visible_sum = egui::Vec2::ZERO;
    let mut visible_count = 0usize;
    for point in points.iter().copied() {
        if label_bounds.contains(point) {
            visible_sum += point.to_vec2();
            visible_count += 1;
        }
    }
    if visible_count > 0 {
        let scale = 1.0 / visible_count as f32;
        return Some(egui::pos2(visible_sum.x * scale, visible_sum.y * scale));
    }

    let expanded = label_bounds.expand(8.0);
    let mut best = None::<(egui::Pos2, f32)>;
    let mut previous = points[points.len() - 1];
    for current in points.iter().copied() {
        if screen_point_valid(previous) && screen_point_valid(current) {
            let candidate = closest_point_on_segment(center, previous, current);
            let distance_sq = candidate.distance_sq(center);
            if expanded.contains(candidate)
                && best
                    .as_ref()
                    .is_none_or(|(_, best_distance)| distance_sq < *best_distance)
            {
                best = Some((candidate, distance_sq));
            }
        }
        previous = current;
    }
    best.map(|(point, _)| point)
}

pub(crate) fn closest_point_on_segment(
    point: egui::Pos2,
    start: egui::Pos2,
    end: egui::Pos2,
) -> egui::Pos2 {
    let segment = end - start;
    let length_sq = segment.length_sq();
    if length_sq <= f32::EPSILON {
        return start;
    }
    let t = ((point - start).dot(segment) / length_sq).clamp(0.0, 1.0);
    start + segment * t
}

pub(crate) fn hazard_label_screen_rect(
    center: egui::Pos2,
    label: &str,
    selected: bool,
    font_px: f32,
    selected_font_px: f32,
) -> egui::Rect {
    let font_px = if selected { selected_font_px } else { font_px };
    let width = label.chars().count() as f32 * font_px * 0.58 + 8.0;
    let height = font_px + 6.0;
    egui::Rect::from_center_size(center, egui::vec2(width, height))
}

pub(crate) fn hazard_map_label(record: &HazardRecord) -> String {
    if !record.event_id.contains('#') {
        return record.label.clone();
    }
    if let Some((base, suffix)) = record.label.rsplit_once(' ')
        && suffix.chars().all(|character| character.is_ascii_digit())
        && !base.trim().is_empty()
    {
        return base.to_owned();
    }
    record.label.clone()
}

pub(crate) fn polygon_screen_centroid(points: &[egui::Pos2]) -> egui::Pos2 {
    let mut sum = egui::Vec2::ZERO;
    for point in points {
        sum += point.to_vec2();
    }
    let scale = 1.0 / points.len().max(1) as f32;
    egui::pos2(sum.x * scale, sum.y * scale)
}

pub(crate) fn point_segment_distance_sq(
    point: egui::Pos2,
    start: egui::Pos2,
    end: egui::Pos2,
) -> f32 {
    let segment = end - start;
    let length_sq = segment.length_sq();
    if length_sq <= f32::EPSILON {
        return point.distance_sq(start);
    }
    let t = ((point - start).dot(segment) / length_sq).clamp(0.0, 1.0);
    point.distance_sq(start + segment * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hazard_fill_alpha_is_product_independent() {
        assert_eq!(hazard_fill_alpha(50, false), 50);
        assert_eq!(hazard_fill_alpha(50, true), 70);
        assert_eq!(hazard_fill_alpha(90, true), 100);
        assert_eq!(hazard_fill_alpha(0, true), 0);
    }

    #[test]
    fn event_loop_product_window_selector_keeps_range_products() {
        let start = Utc.with_ymd_and_hms(2026, 6, 14, 14, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 14, 18, 0, 0).unwrap();
        let products = [
            (
                "future",
                Utc.with_ymd_and_hms(2026, 6, 14, 19, 0, 0).unwrap(),
            ),
            (
                "inside",
                Utc.with_ymd_and_hms(2026, 6, 14, 16, 0, 0).unwrap(),
            ),
            ("pad", Utc.with_ymd_and_hms(2026, 6, 14, 12, 0, 0).unwrap()),
            ("old", Utc.with_ymd_and_hms(2026, 6, 14, 9, 0, 0).unwrap()),
        ]
        .into_iter()
        .map(|(id, issuance_time)| NwsProductSummary {
            issuance_time,
            url: format!("https://example.test/{id}"),
        })
        .collect::<Vec<_>>();

        let selected = select_hot_text_summaries_for_window(products, start, end)
            .into_iter()
            .map(|summary| summary.url.rsplit('/').next().unwrap().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(selected, ["inside", "pad"]);
    }

    /// Golden parity: default SPC outlook styling (fill 58, stroke 230,
    /// width 2.0, published colors) and report markers (tornado red 5 px
    /// w/ black outline, wind blue 3.5, hail green 3.5, no outline).
    #[test]
    fn default_styles_pin_spc_constants() {
        let registry = styles::StyleRegistry::default();
        let spc = registry.spc();
        assert_eq!(spc.outlook_fill_alpha, 58);
        assert_eq!(spc.outlook_stroke_alpha, 230);
        assert_eq!(spc.outlook_stroke_width, 2.0);
        assert!(spc.use_spc_published_colors);

        let tornado = registry.report_marker(spc_layers::ReportKind::Tornado.style_key());
        assert_eq!(
            style_color32(tornado.color),
            egui::Color32::from_rgb(235, 60, 60)
        );
        assert_eq!(tornado.size_px, 5.0);
        assert_eq!(tornado.outline_width, 1.0);
        let wind = registry.report_marker(spc_layers::ReportKind::Wind.style_key());
        assert_eq!(
            style_color32(wind.color),
            egui::Color32::from_rgb(90, 140, 245)
        );
        assert_eq!(wind.size_px, 3.5);
        assert_eq!(wind.outline_width, 0.0);
        let hail = registry.report_marker(spc_layers::ReportKind::Hail.style_key());
        assert_eq!(
            style_color32(hail.color),
            egui::Color32::from_rgb(80, 200, 100)
        );
        assert_eq!(hail.size_px, 3.5);
        assert_eq!(hail.outline_width, 0.0);
    }

    /// Golden parity: obs plot, range ring, site marker, label, placefile
    /// and drape defaults all pin the legacy hard-coded constants.
    #[test]
    fn default_styles_pin_obs_ring_label_constants() {
        let registry = styles::StyleRegistry::default();
        let obs = registry.obs();
        assert_eq!(
            style_color32(obs.metar_dot),
            egui::Color32::from_rgb(210, 214, 220)
        );
        assert_eq!(
            style_color32(obs.mesonet_dot),
            egui::Color32::from_rgb(214, 176, 96)
        );
        assert_eq!(
            style_color32(obs.temp_color),
            egui::Color32::from_rgb(255, 120, 110)
        );
        assert_eq!(
            style_color32(obs.dewpoint_color),
            egui::Color32::from_rgb(120, 235, 130)
        );
        assert_eq!(
            style_color32(obs.gust_color),
            egui::Color32::from_rgb(255, 196, 110)
        );
        assert_eq!(
            style_color32(obs.station_id_color),
            egui::Color32::from_rgba_unmultiplied(190, 196, 204, 180)
        );
        assert_eq!(
            style_color32(obs.barb_color),
            egui::Color32::from_rgb(205, 212, 222)
        );
        assert_eq!(obs.barb_width, 1.2);
        assert_eq!(obs.value_font_px, 11.0);
        assert_eq!(obs.small_font_px, 9.0);
        assert_eq!(obs.declutter_cell_px, 88.0);

        let rings = registry.range_rings();
        assert_eq!(rings.primary_width, 1.8);
        assert_eq!(rings.overlay_width, 1.5);
        assert_eq!(rings.color_mode, styles::RingColorMode::Age);
        assert_eq!(
            style_color32(rings.site_selected_color),
            egui::Color32::from_rgb(88, 210, 245)
        );
        assert_eq!(
            style_color32(rings.site_idle_color),
            egui::Color32::from_rgb(106, 132, 154)
        );

        let labels = registry.labels();
        assert_eq!(labels.town_font_scale, 1.0);
        assert_eq!(
            style_color32(labels.warning_halo_color),
            egui::Color32::from_rgba_unmultiplied(0, 0, 0, 210)
        );
        let global = registry.hazard_global();
        assert_eq!(global.label_font_px, 11.0);
        assert_eq!(global.label_font_selected_px, 12.0);

        let placefiles = registry.placefiles();
        assert_eq!(placefiles.line_width_scale, 1.0);
        assert_eq!(placefiles.icon_scale, 1.0);
        assert_eq!(placefiles.text_size_scale, 1.0);
        assert!(placefiles.default_show_text);

        let drapes = registry.drapes();
        assert_eq!(drapes.radar_opacity, 1.0);
        assert_eq!(drapes.goes_opacity, 0.85);
        assert_eq!(drapes.model_opacity, 0.65);
        assert_eq!(drapes.min_overlay_alpha, 48);

        let age = registry.radar_age();
        assert_eq!(
            (age.green_seconds, age.yellow_seconds, age.red_seconds),
            (360, 600, 900)
        );
        assert!(age.ring_enabled);
    }

    #[test]
    fn hot_text_summary_selection_keeps_recent_bursts_bounded() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 21, 10, 0)
            .single()
            .expect("valid query time");
        let recent_burst = (0..(HOT_TEXT_PRODUCTS_MAX_PER_TYPE + 4))
            .map(|index| {
                test_nws_product_summary(
                    index,
                    query_time - chrono::Duration::minutes(index as i64),
                )
            })
            .collect::<Vec<_>>();
        let selected = select_hot_text_summaries(recent_burst, query_time);

        assert_eq!(selected.len(), HOT_TEXT_PRODUCTS_MAX_PER_TYPE);
        assert_eq!(selected.first().unwrap().url, "https://example.test/0");
        assert_eq!(
            selected.last().unwrap().url,
            format!(
                "https://example.test/{}",
                HOT_TEXT_PRODUCTS_MAX_PER_TYPE - 1
            )
        );
    }

    #[test]
    fn hot_text_summary_selection_keeps_minimum_for_quiet_types() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 21, 10, 0)
            .single()
            .expect("valid query time");
        let quiet_type = (0..12)
            .map(|index| {
                test_nws_product_summary(
                    index,
                    query_time - chrono::Duration::minutes(180 + index as i64),
                )
            })
            .collect::<Vec<_>>();
        let selected = select_hot_text_summaries(quiet_type, query_time);

        assert_eq!(selected.len(), HOT_TEXT_PRODUCTS_MIN_PER_TYPE);
    }

    #[test]
    fn hazard_parser_extracts_warning_polygon_and_tags() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 4, 21, 16, 25, 0)
            .single()
            .expect("valid query time");
        let records = parse_hazard_records_from_text(
            Path::new("tor.txt"),
            SAMPLE_TORNADO_WARNING,
            Some(query_time),
        );

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_family, "tornado");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert_eq!(record.tornado.as_deref(), Some("RADAR INDICATED"));
        assert_eq!(record.hail_inches, Some(1.0));
        assert_eq!(record.points.len(), 6);
        assert_eq!(record.points[0].lat, 42.15);
        assert_eq!(record.points[0].lon, -88.50);
        assert!(hazard_polygon_contains_point(
            &record.points,
            HazardPoint {
                lon: -88.20,
                lat: 42.03
            }
        ));
    }

    #[test]
    fn weather_gov_alert_parser_extracts_live_polygon_shape() {
        let collection: WeatherAlertFeatureCollection =
            serde_json::from_str(SAMPLE_ACTIVE_ALERT_GEOJSON).expect("active alert sample");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 19, 30, 0)
            .single()
            .expect("valid query time");
        let records = parse_weather_alert_feature(&collection.features[0], query_time)
            .expect("weather alert feature parse");

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_family, "tornado");
        assert_eq!(record.label, "TOR 0045 RADAR INDICATED");
        assert_eq!(record.event_id, "KSGF.TO.W.0045");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert_eq!(record.severity.as_deref(), Some("Extreme"));
        assert_eq!(record.certainty.as_deref(), Some("Observed"));
        assert_eq!(record.urgency.as_deref(), Some("Immediate"));
        assert_eq!(record.points.len(), 4);
        assert_eq!(record.points[0].lon, -94.10);
        assert_eq!(record.points[0].lat, 37.40);
        assert!(hazard_polygon_contains_point(
            &record.points,
            HazardPoint {
                lon: -94.00,
                lat: 37.33
            }
        ));
    }

    #[test]
    fn weather_gov_alert_parser_classifies_fire_weather_alerts() {
        let feature: WeatherAlertFeature = serde_json::from_value(serde_json::json!({
            "id": "https://api.weather.gov/alerts/urn:oid:red-flag",
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [-104.0, 36.0],
                    [-103.0, 36.0],
                    [-103.0, 37.0],
                    [-104.0, 36.0]
                ]]
            },
            "properties": {
                "id": "urn:oid:red-flag",
                "event": "Red Flag Warning",
                "headline": "Red Flag Warning issued June 27",
                "senderName": "NWS Amarillo TX",
                "onset": "2026-06-27T18:00:00+00:00",
                "expires": "2026-06-28T01:00:00+00:00",
                "parameters": {
                    "VTEC": ["/O.NEW.KAMA.FW.W.0007.260627T1800Z-260628T0100Z/"]
                }
            }
        }))
        .expect("fire weather alert feature");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 27, 19, 0, 0)
            .single()
            .expect("valid query time");

        let records =
            parse_weather_alert_feature(&feature, query_time).expect("weather alert feature parse");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_family, "fire weather");
        assert_eq!(records[0].label, "FIRE 0007");
    }

    #[test]
    fn weather_gov_alert_parser_marks_can_vtec_as_canceled() {
        let feature: WeatherAlertFeature = serde_json::from_value(serde_json::json!({
            "id": "https://api.weather.gov/alerts/urn:oid:flood-watch-cancel",
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [-87.0, 39.0],
                    [-86.0, 39.0],
                    [-86.0, 40.0],
                    [-87.0, 40.0],
                    [-87.0, 39.0]
                ]]
            },
            "properties": {
                "id": "urn:oid:flood-watch-cancel",
                "event": "Flood Watch",
                "headline": "The Flood Watch has been cancelled.",
                "description": "The Flood Watch has been cancelled and is no longer in effect.",
                "senderName": "NWS Indianapolis IN",
                "messageType": "Cancel",
                "onset": "2026-06-27T16:00:00+00:00",
                "expires": "2026-06-28T01:00:00+00:00",
                "parameters": {
                    "VTEC": ["/O.CAN.KIND.FA.A.0009.260627T1600Z-260628T0100Z/"]
                }
            }
        }))
        .expect("canceled flood watch feature");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 27, 18, 0, 0)
            .single()
            .expect("valid query time");

        let records =
            parse_weather_alert_feature(&feature, query_time).expect("weather alert feature parse");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_id, "KIND.FA.A.0009");
        assert_eq!(records[0].action, "CAN");
        assert_eq!(records[0].lifecycle_status.as_deref(), Some("Canceled"));

        let overlay = build_live_hazard_overlay(
            "NWS active alerts".to_owned(),
            query_time,
            1,
            1,
            0,
            Instant::now(),
            records,
        );
        assert!(overlay.records.is_empty());
    }

    #[test]
    fn weather_gov_alert_parser_uses_cap_message_type_cancel_without_vtec() {
        let feature: WeatherAlertFeature = serde_json::from_value(serde_json::json!({
            "id": "https://api.weather.gov/alerts/urn:oid:cancel-message-type",
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [-97.0, 35.0],
                    [-96.0, 35.0],
                    [-96.0, 36.0],
                    [-97.0, 36.0],
                    [-97.0, 35.0]
                ]]
            },
            "properties": {
                "id": "urn:oid:cancel-message-type",
                "event": "Flood Watch",
                "headline": "The Flood Watch has been cancelled.",
                "senderName": "NWS Test",
                "messageType": "Cancel",
                "onset": "2026-06-27T16:00:00+00:00",
                "expires": "2026-06-28T01:00:00+00:00",
                "parameters": {}
            }
        }))
        .expect("cancel message-type feature");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 27, 18, 0, 0)
            .single()
            .expect("valid query time");

        let records =
            parse_weather_alert_feature(&feature, query_time).expect("weather alert feature parse");

        assert_eq!(records[0].action, "CAN");
        assert_eq!(records[0].lifecycle_status.as_deref(), Some("Canceled"));
    }

    #[test]
    fn weather_gov_alert_parser_sanitizes_spiky_generic_alert_geometry() {
        let feature: WeatherAlertFeature = serde_json::from_value(serde_json::json!({
            "id": "https://api.weather.gov/alerts/urn:oid:marine-spike",
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [-97.39, 26.90],
                    [-97.36, 27.24],
                    [-96.95, 26.79],
                    [-97.30, 26.63],
                    [-97.38, 26.89],
                    [-97.31, 26.65],
                    [-97.35, 26.70],
                    [-97.32, 26.62],
                    [-97.33, 26.62],
                    [-97.33, 26.63],
                    [-97.44, 26.59],
                    [-97.58, 26.85],
                    [-97.56, 26.84],
                    [-97.57, 26.98],
                    [-97.42, 27.25],
                    [-97.39, 26.90]
                ]]
            },
            "properties": {
                "id": "urn:oid:marine-spike",
                "event": "Marine Weather Statement",
                "headline": "Marine Weather Statement issued June 15 at 10:28PM CDT by NWS Brownsville TX",
                "areaDesc": "Laguna Madre From 5 nm North Of Port Mansfield To Baffin Bay TX",
                "senderName": "NWS Brownsville TX",
                "severity": "Minor",
                "certainty": "Observed",
                "urgency": "Expected",
                "onset": "2026-06-16T03:28:00+00:00",
                "expires": "2026-06-16T04:30:00+00:00",
                "parameters": {}
            }
        }))
        .expect("spiky marine alert feature");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 16, 4, 20, 19)
            .single()
            .expect("valid query time");

        let records =
            parse_weather_alert_feature(&feature, query_time).expect("weather alert feature parse");

        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_family, "alert");
        assert_eq!(record.label, "ALERT");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert!(record.points.len() < 15);
        assert_eq!(record.bbox, hazard_bbox(&record.points));
        assert!(hazard_points_renderable(&record.points));
        assert!(!generic_alert_ring_needs_hull(&record.points));
    }

    #[test]
    fn weather_gov_alert_parser_sanitizes_spiky_watch_geometry() {
        let feature: WeatherAlertFeature = serde_json::from_value(serde_json::json!({
            "id": "https://api.weather.gov/alerts/urn:oid:watch-spike",
            "geometry": {
                "type": "Polygon",
                "coordinates": [[
                    [-97.39, 26.90],
                    [-97.36, 27.24],
                    [-96.95, 26.79],
                    [-97.30, 26.63],
                    [-97.38, 26.89],
                    [-97.31, 26.65],
                    [-97.35, 26.70],
                    [-97.32, 26.62],
                    [-97.33, 26.62],
                    [-97.33, 26.63],
                    [-97.44, 26.59],
                    [-97.58, 26.85],
                    [-97.56, 26.84],
                    [-97.57, 26.98],
                    [-97.42, 27.25],
                    [-97.39, 26.90]
                ]]
            },
            "properties": {
                "id": "urn:oid:watch-spike",
                "event": "Flood Watch",
                "headline": "Flood Watch issued June 27",
                "senderName": "NWS Test",
                "severity": "Moderate",
                "certainty": "Likely",
                "urgency": "Expected",
                "onset": "2026-06-27T18:00:00+00:00",
                "expires": "2026-06-28T01:00:00+00:00",
                "parameters": {
                    "VTEC": ["/O.NEW.KBRO.FA.A.0009.260627T1800Z-260628T0100Z/"]
                }
            }
        }))
        .expect("spiky watch feature");
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 27, 19, 0, 0)
            .single()
            .expect("valid query time");

        let records =
            parse_weather_alert_feature(&feature, query_time).expect("weather alert feature parse");

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_family, "watch");
        assert!(records[0].points.len() < 15);
        assert!(hazard_points_renderable(&records[0].points));
        assert!(!generic_alert_ring_needs_hull(&records[0].points));
    }

    #[test]
    fn damage_threat_escalates_label() {
        let emergency = ParsedWarningTags {
            tornado: Some("OBSERVED".into()),
            damage_threat: Some("CATASTROPHIC".into()),
            ..Default::default()
        };
        assert_eq!(
            hazard_label("tornado", "0088", &emergency),
            "TOR EMERGENCY 0088 OBSERVED"
        );
        // Case-insensitive: text products and CAP parameters both feed this.
        let pds = ParsedWarningTags {
            damage_threat: Some("considerable damage threat".into()),
            ..Default::default()
        };
        assert_eq!(hazard_label("tornado", "0001", &pds), "PDS TOR 0001");
        let plain = ParsedWarningTags::default();
        assert_eq!(hazard_label("tornado", "0002", &plain), "TOR 0002");
        assert_eq!(
            hazard_label(
                "severe thunderstorm",
                "0300",
                &ParsedWarningTags {
                    damage_threat: Some("DESTRUCTIVE DAMAGE THREAT".into()),
                    ..Default::default()
                }
            ),
            "SVR DESTRUCTIVE 0300"
        );
    }

    #[test]
    fn weather_gov_alert_tags_prefer_strongest_repeated_values() {
        let mut parameters = BTreeMap::new();
        parameters.insert(
            "tornadoDetection".to_owned(),
            vec!["RADAR INDICATED".to_owned(), "OBSERVED".to_owned()],
        );
        parameters.insert(
            "tornadoDamageThreat".to_owned(),
            vec!["base".to_owned(), "considerable damage threat".to_owned()],
        );
        parameters.insert(
            "maxHailSize".to_owned(),
            vec!["0.75".to_owned(), "1.50".to_owned()],
        );
        parameters.insert(
            "maxWindGust".to_owned(),
            vec!["60 MPH".to_owned(), "80 MPH".to_owned()],
        );

        let tags = parse_weather_alert_tags(&parameters);

        assert_eq!(tags.tornado.as_deref(), Some("OBSERVED"));
        assert_eq!(tags.damage_threat.as_deref(), Some("CONSIDERABLE"));
        assert_eq!(tags.hail_inches, Some(1.5));
        assert_eq!(tags.wind_mph, Some(80));
        assert_eq!(
            hazard_label("tornado", "0075", &tags),
            "PDS TOR 0075 OBSERVED"
        );
    }

    #[test]
    fn watcher_zone_scope_defaults_to_full_display_scope() {
        // Empty alert_sound_families means "every family sounds"
        // (alert_family_enabled), so the watcher's zone-geometry scope must
        // match the display scope EXACTLY — anything narrower could silence
        // a zone-based alert while the window is minimized.
        let families: Vec<String> = Vec::new();
        for family in [
            "tornado",
            "severe thunderstorm",
            "flash flood",
            "flood",
            "fire weather",
            "special marine",
            "snow squall",
            "watch",
            "special weather",
            "mesoscale discussion",
            "local storm report",
            "alert",
        ] {
            assert_eq!(
                zone_geometry_scope_includes(ZoneGeometryScope::AlertSound(&families), family),
                zone_geometry_scope_includes(ZoneGeometryScope::Display, family),
                "watcher/display zone scope parity broken for family {family}"
            );
        }
    }

    #[test]
    fn watcher_zone_scope_narrows_with_explicit_sound_families() {
        let tor_only = vec!["tornado".to_owned()];
        let scope = ZoneGeometryScope::AlertSound(&tor_only);
        assert!(zone_geometry_scope_includes(scope, "tornado"));
        assert!(!zone_geometry_scope_includes(scope, "watch"));
        assert!(!zone_geometry_scope_includes(scope, "flood"));
        assert!(!zone_geometry_scope_includes(scope, "special weather"));

        // Every family the alert-sound settings can enable keeps its zone
        // enrichment when selected — the sound gate
        // (hazard_record_should_latch_attention) needs renderable points.
        for (family, _) in ALERT_SOUND_FAMILY_OPTIONS.iter().copied() {
            let selected = vec![family.to_owned()];
            assert!(
                zone_geometry_scope_includes(ZoneGeometryScope::AlertSound(&selected), family),
                "sound-enabled family {family} lost its zone enrichment"
            );
        }
    }

    fn zoneless_alert_feature(event: &str, zone_url: &str) -> WeatherAlertFeature {
        WeatherAlertFeature {
            id: None,
            at_id: None,
            geometry: None,
            properties: WeatherAlertProperties {
                event: Some(event.to_owned()),
                affected_zones: vec![zone_url.to_owned()],
                ..Default::default()
            },
        }
    }

    #[test]
    fn weather_alert_zone_urls_respects_scope() {
        let features = vec![
            zoneless_alert_feature(
                "Tornado Watch",
                "https://api.weather.gov/zones/forecast/OKZ031",
            ),
            zoneless_alert_feature(
                "Tornado Warning",
                "https://api.weather.gov/zones/county/OKC109",
            ),
        ];

        let display = weather_alert_zone_urls(&features, ZoneGeometryScope::Display);
        assert_eq!(
            display.len(),
            2,
            "display scope enriches watches and warnings"
        );

        let all_families: Vec<String> = Vec::new();
        let watcher_default =
            weather_alert_zone_urls(&features, ZoneGeometryScope::AlertSound(&all_families));
        assert_eq!(
            watcher_default, display,
            "default (empty-families) watcher scope must equal the display scope"
        );

        let tor_only = vec!["tornado".to_owned()];
        let narrowed = weather_alert_zone_urls(&features, ZoneGeometryScope::AlertSound(&tor_only));
        assert_eq!(
            narrowed,
            vec!["https://api.weather.gov/zones/county/OKC109".to_owned()],
            "a watch cannot sound when only tornado is enabled, so its zone fetch is shed"
        );
    }

    #[test]
    fn zone_geometry_resolution_memoizes_successes_and_retries_failures() {
        let ring = vec![vec![
            HazardPoint {
                lon: -98.0,
                lat: 35.0,
            },
            HazardPoint {
                lon: -97.0,
                lat: 35.0,
            },
            HazardPoint {
                lon: -97.0,
                lat: 36.0,
            },
        ]];
        // URLs unique to this test so the process-wide memo cannot be
        // pre-seeded by other tests.
        let urls = vec![
            "https://api.weather.gov/zones/forecast/TEST-MEMO-OK".to_owned(),
            "https://api.weather.gov/zones/forecast/TEST-MEMO-EMPTY".to_owned(),
            "https://api.weather.gov/zones/forecast/TEST-MEMO-FAIL".to_owned(),
        ];
        let calls = std::cell::RefCell::new(Vec::<String>::new());
        let fetch = |url: &str| {
            calls.borrow_mut().push(url.to_owned());
            if url.ends_with("OK") {
                Ok(ring.clone())
            } else if url.ends_with("EMPTY") {
                Ok(Vec::new())
            } else {
                Err("transient network error".to_owned())
            }
        };

        // The closure only captures shared references, so it is Copy and can
        // be passed by value to both passes.
        let first = resolve_zone_geometries(&urls, fetch);
        assert_eq!(first.len(), 1, "only non-empty geometry is usable");
        assert!(first.contains_key(&urls[0]));
        assert_eq!(calls.borrow().len(), 3, "cold pass fetches every zone");

        calls.borrow_mut().clear();
        let second = resolve_zone_geometries(&urls, fetch);
        assert_eq!(second.len(), 1, "memoized geometry still resolves");
        assert!(second.contains_key(&urls[0]));
        assert_eq!(
            calls.borrow().as_slice(),
            std::slice::from_ref(&urls[2]),
            "successes (including the legitimately empty zone) come from the \
             process-wide memo; only the FAILED zone is retried"
        );
    }

    #[test]
    fn nonconvex_warning_polygon_can_be_filled() {
        let points = vec![
            egui::pos2(0.0, 0.0),
            egui::pos2(4.0, 0.0),
            egui::pos2(4.0, 4.0),
            egui::pos2(2.0, 2.0),
            egui::pos2(0.0, 4.0),
        ];

        let mesh = filled_polygon_mesh(&points, egui::Color32::from_rgb(255, 200, 0))
            .expect("nonconvex polygon triangulates");

        assert_eq!(mesh.indices.len(), 9);
        assert_eq!(mesh.vertices.len(), 5);
    }

    #[test]
    fn screen_polyline_chunks_do_not_connect_projection_jumps() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(500.0, 400.0));
        let points = vec![
            egui::pos2(20.0, 20.0),
            egui::pos2(40.0, 28.0),
            egui::pos2(460.0, 360.0),
            egui::pos2(480.0, 370.0),
        ];

        assert!(screen_polyline_has_jump(&points, false, rect, 0.0));
        let chunks = screen_polyline_chunks(&points, false, rect, 0.0);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], vec![points[0], points[1]]);
        assert_eq!(chunks[1], vec![points[2], points[3]]);
    }

    #[test]
    fn clipped_outlook_segments_stay_inside_view() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let (a, b) = clip_segment_to_rect(egui::pos2(-50.0, 50.0), egui::pos2(150.0, 50.0), rect)
            .expect("segment crosses rect");

        assert_eq!(a, egui::pos2(0.0, 50.0));
        assert_eq!(b, egui::pos2(100.0, 50.0));
        assert!(
            clip_segment_to_rect(egui::pos2(-50.0, -50.0), egui::pos2(-10.0, -10.0), rect)
                .is_none()
        );
    }

    #[test]
    fn clipped_outlook_ring_keeps_full_viewport_crossing_edges() {
        // Storm zoom inside a huge outlook ring: one border edge crosses the
        // whole viewport with both endpoints far offscreen. The old
        // clipped-length-vs-viewport cull (0.62 x diagonal) dropped it.
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let screen = vec![
            egui::pos2(-300.0, 50.0),
            egui::pos2(400.0, 50.0),
            egui::pos2(400.0, 700.0),
            egui::pos2(-300.0, 700.0),
            egui::pos2(-300.0, 50.0),
        ];

        let segments = clipped_outlook_ring_segments(&screen, rect);

        assert_eq!(segments.len(), 1, "the crossing edge must be drawn");
        let (a, b) = segments[0];
        assert_eq!(a.y, 50.0);
        assert_eq!(b.y, 50.0);
        assert!(a.x <= rect.left() && b.x >= rect.right());
    }

    #[test]
    fn clipped_outlook_ring_culls_projection_wraparound_chords() {
        // Densely spaced offscreen ring plus one bogus far-hemisphere chord
        // through the viewport: the chord is an enormous multiple of the
        // ring's own spacing and must be skipped.
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let mut screen: Vec<egui::Pos2> = (0..40)
            .map(|step| egui::pos2(-1000.0 + step as f32 * 10.0, 50.0))
            .collect();
        screen.push(egui::pos2(200_000.0, 50.0));

        let segments = clipped_outlook_ring_segments(&screen, rect);

        assert!(
            segments.is_empty(),
            "wraparound chord must not be drawn: {segments:?}"
        );
    }

    #[test]
    fn hazard_visible_label_anchor_uses_view_center_inside_partial_polygon() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let points = vec![
            egui::pos2(-300.0, 0.0),
            egui::pos2(70.0, 0.0),
            egui::pos2(70.0, 100.0),
            egui::pos2(-300.0, 100.0),
        ];

        let anchor = hazard_visible_label_anchor(&points, rect).expect("visible label anchor");

        assert_eq!(anchor, egui::pos2(50.0, 50.0));
    }

    #[test]
    fn outlook_polygon_mesh_keeps_interior_ring_unfilled() {
        let outer = vec![
            egui::pos2(0.0, 0.0),
            egui::pos2(10.0, 0.0),
            egui::pos2(10.0, 10.0),
            egui::pos2(0.0, 10.0),
            egui::pos2(0.0, 0.0),
        ];
        let holes = vec![vec![
            egui::pos2(3.0, 3.0),
            egui::pos2(7.0, 3.0),
            egui::pos2(7.0, 7.0),
            egui::pos2(3.0, 7.0),
            egui::pos2(3.0, 3.0),
        ]];
        let mesh =
            filled_polygon_with_holes_mesh(&outer, &holes, egui::Color32::from_rgb(255, 255, 0))
                .expect("donut triangulates");
        let contains = |point: egui::Pos2| {
            mesh.indices.chunks_exact(3).any(|triangle| {
                let a = mesh.vertices[triangle[0] as usize].pos;
                let b = mesh.vertices[triangle[1] as usize].pos;
                let c = mesh.vertices[triangle[2] as usize].pos;
                point_in_triangle(point, a, b, c)
            })
        };

        assert!(contains(egui::pos2(1.0, 1.0)));
        assert!(!contains(egui::pos2(5.0, 5.0)));
    }

    #[test]
    fn self_intersection_scan_flags_only_proper_crossings() {
        let bowtie = vec![
            egui::pos2(0.0, 0.0),
            egui::pos2(100.0, 0.0),
            egui::pos2(20.0, 60.0),
            egui::pos2(80.0, 60.0),
        ];
        assert!(screen_ring_self_intersects(&bowtie));
        let square = vec![
            egui::pos2(0.0, 0.0),
            egui::pos2(10.0, 0.0),
            egui::pos2(10.0, 10.0),
            egui::pos2(0.0, 10.0),
        ];
        assert!(!screen_ring_self_intersects(&square));
        // Collinear out-and-back overlap touches but never properly crosses.
        let spike = vec![
            egui::pos2(0.0, 0.0),
            egui::pos2(40.0, 0.0),
            egui::pos2(40.0, 40.0),
            egui::pos2(20.0, 40.0),
            egui::pos2(20.0, 70.0),
            egui::pos2(20.0, 40.0),
            egui::pos2(0.0, 40.0),
        ];
        assert!(!screen_ring_self_intersects(&spike));
    }

    #[test]
    fn spc_md_product_parser_extracts_compact_polygon_and_click_details() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 6, 7, 19, 30, 0)
            .single()
            .expect("valid query time");
        let record = parse_spc_md_product_page(
            "https://www.spc.noaa.gov/products/md/md1015.html",
            SAMPLE_SPC_MD_HTML,
            query_time,
        )
        .expect("spc md record");

        assert_eq!(record.event_family, "mesoscale discussion");
        assert_eq!(record.label, "MD 1015");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert_eq!(record.valid_start.as_deref(), Some("2026-06-07T18:59:00Z"));
        assert_eq!(record.valid_end.as_deref(), Some("2026-06-07T21:00:00Z"));
        assert_eq!(
            record.headline.as_deref(),
            Some("Severe potential...Watch unlikely")
        );
        assert_eq!(record.area.as_deref(), Some("portions of the Mid-Atlantic"));
        assert_eq!(
            record.source_url.as_deref(),
            Some("https://www.spc.noaa.gov/products/md/md1015.html")
        );
        assert!(
            record
                .details
                .iter()
                .any(|line| line.contains("Watch issuance 5 percent"))
        );
        assert!(
            record
                .details
                .iter()
                .any(|line| line.contains("MOST PROBABLE PEAK TORNADO INTENSITY...85-110 MPH"))
        );
        assert_eq!(record.points[0].lat, 36.37);
        assert_eq!(record.points[0].lon, -75.80);
        assert!(hazard_polygon_contains_point(
            &record.points,
            HazardPoint {
                lon: -77.2,
                lat: 37.0
            }
        ));
    }

    #[test]
    fn spc_md_product_parser_expires_after_compact_valid_end() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 7, 13, 4, 0, 0)
            .single()
            .expect("valid query time");
        let record = parse_spc_md_product_page(
            "https://www.spc.noaa.gov/products/md/md1610.html",
            SAMPLE_EXPIRED_SPC_MD_HTML,
            query_time,
        )
        .expect("spc md record");

        assert_eq!(record.label, "MD 1610");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Expired"));
        assert_eq!(record.valid_start.as_deref(), Some("2026-07-13T00:27:00Z"));
        assert_eq!(record.valid_end.as_deref(), Some("2026-07-13T02:30:00Z"));
    }

    #[test]
    fn spc_md_valid_period_handles_year_rollover() {
        let query_time = Utc
            .with_ymd_and_hms(2027, 1, 1, 0, 30, 0)
            .single()
            .expect("valid query time");

        assert_eq!(
            parse_spc_md_valid_period("Valid 312330Z - 010130Z", query_time),
            Some((
                Utc.with_ymd_and_hms(2026, 12, 31, 23, 30, 0).unwrap(),
                Utc.with_ymd_and_hms(2027, 1, 1, 1, 30, 0).unwrap(),
            ))
        );
    }

    #[test]
    fn spc_md_compact_polygon_restores_implied_west_100_longitudes() {
        let points = parse_lat_lon_points(&[
            "LAT...LON   35880436 36100370 36470288 36870233 37260186 37400156",
            "            37460100 37350044 37200025 36940013 36640013 36330021",
            "            36020049 35840086 35680131 35530192 35450269 35380335",
            "            35330382 35420425 35690439 35740442 35880436",
            "",
        ]);

        assert_eq!(points.len(), 23);
        assert_eq!(points[0].lat, 35.88);
        assert_eq!(points[0].lon, -104.36);
        assert_eq!(points[6].lon, -101.00);
        assert_eq!(points[9].lon, -100.13);
        assert_eq!(hazard_bbox(&points), [-104.42, 35.33, -100.13, 37.46]);
        assert!(hazard_polygon_contains_point(
            &points,
            HazardPoint {
                lon: -102.25,
                lat: 36.5
            }
        ));
    }

    #[test]
    fn hazard_parser_marks_expired_against_query_time() {
        let query_time = Utc
            .with_ymd_and_hms(2026, 4, 21, 17, 0, 0)
            .single()
            .expect("valid query time");
        let records = parse_hazard_records_from_text(
            Path::new("tor.txt"),
            SAMPLE_TORNADO_WARNING,
            Some(query_time),
        );

        assert_eq!(records[0].lifecycle_status.as_deref(), Some("Expired"));
    }

    #[test]
    fn hazard_parser_extracts_mesoscale_discussion_polygon() {
        let records =
            parse_hazard_records_from_text(Path::new("mcd.txt"), SAMPLE_MESOSCALE_DISCUSSION, None);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_family, "mesoscale discussion");
        assert_eq!(records[0].label, "MD 123");
        assert_eq!(records[0].office, "KWNS");
        assert_eq!(records[0].points.len(), 4);
    }

    #[test]
    fn hazard_parser_extracts_watch_polygon() {
        let records = parse_hazard_records_from_text(Path::new("watch.txt"), SAMPLE_WATCH, None);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event_family, "watch");
        assert_eq!(records[0].label, "WATCH 44");
        assert_eq!(records[0].points[0].lon, -97.50);
    }

    #[test]
    fn custom_warning_provider_url_requires_http_scheme() {
        assert_eq!(
            custom_warning_provider_url("  https://host/a.geojson  ").as_deref(),
            Some("https://host/a.geojson")
        );
        assert_eq!(
            custom_warning_provider_url("http://host/a").as_deref(),
            Some("http://host/a")
        );
        assert!(custom_warning_provider_url("").is_none());
        assert!(custom_warning_provider_url("   ").is_none());
        assert!(custom_warning_provider_url("C:/warnings.json").is_none());
        assert!(custom_warning_provider_url("ftp://host/a").is_none());
    }

    #[test]
    fn custom_warning_feed_empty_feature_collection_is_quiet_success() {
        // The NORMAL quiet state of a healthy CAP relay: a valid
        // FeatureCollection with zero active warnings — exactly what
        // api.weather.gov/alerts/active returns on a calm day. This must be
        // Ok with zero records; historically it fell through to the VTEC
        // text parser and was flagged "(custom warning feed unavailable)"
        // with error_count += 1 on EVERY poll.
        let load =
            parse_custom_warning_feed(r#"{"type":"FeatureCollection","features":[]}"#, Utc::now())
                .expect("an empty-but-valid FeatureCollection is not an error");
        assert_eq!(load.scanned_items, 0);
        assert_eq!(load.parsed_items, 0);
        assert_eq!(load.error_count, 0);
        assert!(load.records.is_empty());

        // Same quiet state without the GeoJSON `type` marker.
        let load = parse_custom_warning_feed(r#"{"features":[]}"#, Utc::now())
            .expect("a bare features array is still a FeatureCollection");
        assert_eq!(load.error_count, 0);
        assert!(load.records.is_empty());
    }

    #[test]
    fn custom_warning_feed_non_collection_bodies_still_error() {
        // An API error payload deserializes into WeatherAlertFeatureCollection
        // (`features` is #[serde(default)]) but is NOT a FeatureCollection —
        // it must keep reporting as a feed problem, never as a quiet day.
        assert!(
            parse_custom_warning_feed(r#"{"title":"rate limited","status":429}"#, Utc::now())
                .is_err()
        );
        assert!(parse_custom_warning_feed("<html>gateway timeout</html>", Utc::now()).is_err());
        assert!(parse_custom_warning_feed("", Utc::now()).is_err());
    }

    #[test]
    fn custom_warning_feed_parses_populated_feature_collection() {
        let body = r#"{
            "type": "FeatureCollection",
            "features": [{
                "id": "custom-tor-1",
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[[-98.0, 35.0], [-97.0, 35.0], [-97.0, 36.0],
                                     [-98.0, 36.0], [-98.0, 35.0]]]
                },
                "properties": {
                    "event": "Tornado Warning",
                    "messageType": "Alert"
                }
            }]
        }"#;
        let load = parse_custom_warning_feed(body, Utc::now())
            .expect("populated FeatureCollection parses");
        assert_eq!(load.scanned_items, 1);
        assert_eq!(load.parsed_items, 1);
        assert_eq!(load.error_count, 0);
        assert_eq!(load.records.len(), 1);
        assert_eq!(load.records[0].event_family, "tornado");
        assert!(hazard_points_renderable(&load.records[0].points));
    }

    fn test_nws_product_summary(index: usize, issuance_time: DateTime<Utc>) -> NwsProductSummary {
        NwsProductSummary {
            url: format!("https://example.test/{index}"),
            issuance_time,
        }
    }

    #[test]
    fn radar_operational_status_parser_reads_nws_alarm_messages() {
        let station_json = r#"{
            "properties": {
                "id": "KUDX",
                "name": "Rapid City",
                "rda": {
                    "timestamp": "2026-06-27T22:57:20+00:00",
                    "properties": {
                        "alarmSummary": "Tower/Utilities|Transmitter|Communication",
                        "mode": "Operational",
                        "status": "Standby",
                        "operabilityStatus": "RDA - Inoperable"
                    }
                }
            }
        }"#;
        let alarms_json = r#"{
            "@graph": [
                {
                    "@type": "wx:RadarStationAlarm",
                    "stationId": "KUDX",
                    "status": "mandatory",
                    "message": "TPS IS OFF-LINE",
                    "timestamp": "2026-06-27T21:45:48+00:00"
                },
                {
                    "@type": "wx:RadarStationAlarm",
                    "stationId": "KUDX",
                    "status": "cleared",
                    "message": "GEN STARTING BATTERY VOLTAGE LOW",
                    "timestamp": "2026-06-26T14:43:37+00:00"
                }
            ]
        }"#;

        let status =
            parse_radar_operational_status("kudx", None, station_json, alarms_json).unwrap();

        assert_eq!(status.site_id, "KUDX");
        assert_eq!(status.site_name, "Rapid City");
        assert_eq!(
            status.alarm_summary.as_deref(),
            Some("Tower/Utilities|Transmitter|Communication")
        );
        assert_eq!(status.mode.as_deref(), Some("Operational"));
        assert_eq!(status.rda_status.as_deref(), Some("Standby"));
        assert_eq!(
            status.operability_status.as_deref(),
            Some("RDA - Inoperable")
        );
        assert_eq!(status.alarms.len(), 2);
        assert_eq!(status.alarms[0].message, "TPS IS OFF-LINE");
        assert_eq!(status.alarms[0].status, "mandatory");
        assert_eq!(
            status.alarms[0]
                .timestamp
                .map(format_utc_seconds)
                .as_deref(),
            Some("2026-06-27T21:45:48Z")
        );
    }

    #[test]
    fn hazard_polygon_style_keys_cover_families_and_escalations() {
        assert!(hazard_style_key_known("tornado"));
        assert!(hazard_style_key_known("tornado/catastrophic"));
        assert!(hazard_style_key_known("severe-thunderstorm/considerable"));
        assert!(hazard_style_key_known("flash-flood/considerable"));
        assert!(hazard_style_key_known("flood/catastrophic"));
        assert!(hazard_style_key_known("fire-weather"));
        assert!(hazard_style_key_known("watch/tornado"));
        assert!(hazard_style_key_known("watch/severe-thunderstorm"));
        assert!(!hazard_style_key_known("not-a-real-polygon-family"));
        assert!(hazard_style_label("tornado/catastrophic").contains("emergency"));
        assert!(hazard_style_label("flood/considerable").contains("Considerable"));
        assert!(hazard_style_label("watch/tornado").contains("Tornado watch"));
        assert_eq!(
            hazard_dash_label(styles::DashPattern::Dashed {
                dash: 9.0,
                gap: 6.0
            }),
            "Dashed"
        );
    }

    const SAMPLE_TORNADO_WARNING: &str = r#"401
WUUS53 KLOT 211600
TORLOT
ILC031-043-197-211630-
/O.NEW.KLOT.TO.W.0001.260421T1600Z-260421T1630Z/

BULLETIN - EAS ACTIVATION REQUESTED
Tornado Warning
National Weather Service Chicago IL
1100 AM CDT Tue Apr 21 2026

LAT...LON 4215 8850 4203 8820 4194 8810 4198 8786 4213 8784 4222 8839
TIME...MOT...LOC 1600Z 265DEG 31KT 4208 8837
TORNADO...RADAR INDICATED
MAX HAIL SIZE...1.00 IN

$$
"#;

    const SAMPLE_MESOSCALE_DISCUSSION: &str = r#"ACUS11 KWNS 211600
SWOMCD
SPC MCD 211600

Mesoscale Discussion 0123
NWS Storm Prediction Center Norman OK
1100 AM CDT Tue Apr 21 2026

Areas affected...northern Illinois

LAT...LON 4215 8850 4194 8810 4198 8786 4222 8839

$$
"#;

    const SAMPLE_WATCH: &str = r#"WWUS20 KWNS 211600
SEL4
SPC WW 211600

URGENT - IMMEDIATE BROADCAST REQUESTED
Tornado Watch Number 44
NWS Storm Prediction Center Norman OK
1100 AM CDT Tue Apr 21 2026

WATCH OUTLINE UPDATE FOR WS 44
LAT...LON 3500 9750 3520 9500 3350 9440 3320 9700

$$
"#;

    const SAMPLE_ACTIVE_ALERT_GEOJSON: &str = r#"{
  "features": [
    {
      "id": "urn:test:tor",
      "geometry": {
        "type": "Polygon",
        "coordinates": [[
          [-94.10, 37.40],
          [-93.90, 37.38],
          [-93.92, 37.25],
          [-94.12, 37.26],
          [-94.10, 37.40]
        ]]
      },
      "properties": {
        "id": "urn:test:tor",
        "event": "Tornado Warning",
        "senderName": "NWS Springfield MO",
        "headline": "Tornado Warning issued June 7 at 2:09PM CDT until June 7 at 3:00PM CDT by NWS Springfield MO",
        "effective": "2026-06-07T14:09:00-05:00",
        "expires": "2026-06-07T15:00:00-05:00",
        "ends": "2026-06-07T15:00:00-05:00",
        "severity": "Extreme",
        "certainty": "Observed",
        "urgency": "Immediate",
        "parameters": {
          "VTEC": ["/O.NEW.KSGF.TO.W.0045.260607T1909Z-260607T2000Z/"],
          "tornadoDetection": ["RADAR INDICATED"],
          "maxHailSize": ["0.00"]
        }
      }
    }
  ]
}"#;

    const SAMPLE_SPC_MD_HTML: &str = r#"<html><body><pre>
   Mesoscale Discussion 1015
   NWS Storm Prediction Center Norman OK
   0159 PM CDT Sun Jun 07 2026

   Areas affected...portions of the Mid-Atlantic

   Concerning...Severe potential...Watch unlikely

   Valid 071859Z - 072100Z
   Probability of Watch Issuance...5 percent

   SUMMARY...Widely scattered thunderstorms may pose a localized risk
   for strong/damaging wind gusts and perhaps small hail this
   afternoon. Watch issuance is not expected.

   LAT...LON   36377580 36277612 36247691 36287734 36357769 36547819
               36707854 36887880 37087908 37467941 37857947 38347939
               38487907 38427845 38227760 38097690 37967599 37867534
               37727542 37317567 36987586 36747583 36497571 36377580

   MOST PROBABLE PEAK TORNADO INTENSITY...85-110 MPH
   MOST PROBABLE PEAK WIND GUST...UP TO 60 MPH
</pre></body></html>"#;

    const SAMPLE_EXPIRED_SPC_MD_HTML: &str = r#"<html><body><pre>
   Mesoscale Discussion 1610
   NWS Storm Prediction Center Norman OK
   0727 PM CDT Sun Jul 12 2026

   Areas affected...portions of northeastern and eastern Montana
   Concerning...Severe potential...Watch unlikely
   Valid 130027Z - 130230Z
   Probability of Watch Issuance...5 percent

   LAT...LON   47351203 48931016 49070973 49040499 47930620 47100805
               46491028 46251125 46591184 47351203
</pre></body></html>"#;
}
