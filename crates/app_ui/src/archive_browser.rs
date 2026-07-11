//! ONE archive world — the unified archive browser (v0.29 spec §5,
//! `docs/v029-engine-spec.md`, Phase 2).
//!
//! US archive listings are verified-good `S3Object`s with a battle-tested
//! chunked download path; international archives are provider
//! [`FramePlan`]s behind [`IntlProvider::archive_source`]. This module
//! unifies them at the ROW level ([`ArchiveScanRow`]) — never at the byte
//! level: the US loader stays unwrapped (spec §9 DO-NOT-TOUCH), US rows
//! wrap `S3Object`s, intl rows wrap `FramePlan`s, and the ONE
//! browse/load surface (`archive_panel`) dispatches on the arm.
//!
//! The browser targets [`ViewerApp::display_owner_site`] — the Phase-2
//! shim over `PollSource` that Phase 3 replaces with the real site model.
//! That kills the old dishonesty where the DATA-tab archive panel
//! silently listed a stale US site while an international source owned
//! the primary display: an intl owner now gets its own provider archive
//! browser (EUMETNET ORD, SMHI), or an honest greyed reason
//! ([`ArchiveAccess::None`]) when the provider has no archive adapter.
//!
//! [`FramePlan`]: data_source::international::FramePlan
//! [`IntlProvider::archive_source`]: data_source::international::IntlProvider::archive_source

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use data_source::international::FramePlan;
use data_source::sites::SiteRef;
use eframe::egui;
use ui_core::worker_slot::SlotPoll;

use crate::{
    ACTIVE_LOAD_POLL_MS, ArchiveLoadProgress, AsyncLoadResult, AsyncLoadUpdate, DecodedLoadBatch,
    FeedSource, MAX_ARCHIVE_FRAME_COUNT, MAX_HISTORY_FRAME_LIMIT, SpcReport, ViewerApp,
    archive_load_progress_row, archive_object_scan_time_utc, cache_dir,
    decode_archive_history_object, fetch_intl_frame_plan_batch, intl_provider_label, panel_kit,
    send_archive_progress,
};

/// Capability answers are values, not missing branches (spec §1.3): the
/// gate and its greyed-UI explanation derive from the same call, so they
/// cannot drift.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveAccess {
    /// Full US Level-II archive (`level2_objects_for_date`, the
    /// verified-good loader — wrapped, never re-expressed).
    Level2S3,
    /// The provider implements [`ArchiveFrames`] and hands it back from
    /// `archive_source()`.
    ///
    /// [`ArchiveFrames`]: data_source::international::ArchiveFrames
    Provider,
    /// No archive path exists; `reason` IS the greyed-UI hover text.
    None { reason: &'static str },
}

const REASON_NO_PROVIDER_ARCHIVE: &str = "This provider has no archive adapter yet — recent \
     scans only. Its capability card in Data \u{25b8} Radar coverage names the next unlock.";
const REASON_UNKNOWN_PROVIDER: &str =
    "Unknown international provider — pick a site in Data > Radar coverage.";

/// THE one archive-capability question (spec §1.3): the archive browser,
/// the Event Loop Builder's Build button, and the Unified Player archive
/// arms all gate through this.
pub(crate) fn archive_access(site: &SiteRef) -> ArchiveAccess {
    match site {
        SiteRef::Us { .. } => ArchiveAccess::Level2S3,
        SiteRef::Intl { provider_id, .. } => data_source::international::intl_providers()
            .iter()
            .find(|provider| provider.id() == provider_id)
            .map(|provider| {
                if provider.supports_archive() {
                    ArchiveAccess::Provider
                } else {
                    ArchiveAccess::None {
                        reason: REASON_NO_PROVIDER_ARCHIVE,
                    }
                }
            })
            .unwrap_or(ArchiveAccess::None {
                reason: REASON_UNKNOWN_PROVIDER,
            }),
    }
}

/// One archive browse target — the row-level union of the two archive
/// worlds (spec §5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ArchiveLister {
    /// Wraps `level2_objects_for_date`/`_for_window` verbatim.
    Us { level2_id: String },
    /// Routes through `IntlProvider::archive_source()`.
    Intl {
        provider_id: String,
        site_id: String,
    },
}

/// One listed archive scan. `time_utc` orders and hour-groups the chips;
/// the load payload stays world-specific so each arm keeps its own
/// verified loader.
#[derive(Clone, Debug)]
pub(crate) struct ArchiveScanRow {
    pub(crate) time_utc: DateTime<Utc>,
    pub(crate) load: ArchiveScanLoad,
}

#[derive(Clone, Debug)]
pub(crate) enum ArchiveScanLoad {
    UsObject(data_source::S3Object),
    IntlPlan(FramePlan),
}

impl ArchiveLister {
    /// Every listed scan on the UTC date, in the upstream catalog's order
    /// (both worlds list oldest-first). Blocking — run on a worker.
    pub(crate) fn list_day(&self, date: NaiveDate) -> Result<Vec<ArchiveScanRow>, String> {
        match self {
            ArchiveLister::Us { level2_id } => {
                let objects = data_source::level2_objects_for_date(level2_id, date)
                    .map_err(|err| err.to_string())?;
                Ok(us_rows_from_objects(objects, date))
            }
            ArchiveLister::Intl {
                provider_id,
                site_id,
            } => {
                let plans =
                    intl_archive_source(provider_id, |archive| archive.day_plans(site_id, date))?;
                Ok(intl_rows_from_plans(plans, date))
            }
        }
    }

    /// Scans inside `[start, end]`, thinned to `max`, oldest first. The US
    /// arm wraps `level2_objects_for_window` (anchor = the window end —
    /// the loop-ending-at-window shape, edge-preserving upstream); the
    /// intl arm lists everything through `ArchiveFrames::window_plans` and
    /// then applies [`limit_intl_rows_for_window`] — census D15's ONE
    /// thinning rule (evenly-spaced, both edges kept) wherever frame
    /// stamps parse. Blocking — run on a worker.
    pub(crate) fn list_window(
        &self,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max: usize,
    ) -> Result<Vec<ArchiveScanRow>, String> {
        match self {
            ArchiveLister::Us { level2_id } => {
                let request = data_source::Level2ArchiveWindowRequest {
                    start_utc,
                    end_utc,
                    anchor_utc: end_utc,
                    pad_scans: 0,
                    extra_start_scans: 0,
                    extra_end_scans: 0,
                    max_objects: max,
                };
                let selection = data_source::level2_objects_for_window(level2_id, &request)
                    .map_err(|err| err.to_string())?;
                Ok(us_rows_from_objects(
                    selection.objects,
                    start_utc.date_naive(),
                ))
            }
            ArchiveLister::Intl {
                provider_id,
                site_id,
            } => {
                // Uncapped listing (catalog probes only, so this is cheap):
                // the provider default would newest-tail-cap at `max` and
                // drop the window's START edge first — the thinning rule
                // belongs here, where identity stamps can be parsed.
                let plans = intl_archive_source(provider_id, |archive| {
                    archive.window_plans(site_id, start_utc, end_utc, usize::MAX)
                })?;
                let rows = intl_rows_from_plans(plans, start_utc.date_naive());
                Ok(limit_intl_rows_for_window(rows, start_utc, end_utc, max))
            }
        }
    }
}

/// Census D15 (owner decision, 2026-07-02): **even sampling is the one
/// rule** for over-cap archive windows. Wherever every row's identity
/// stamp parses ([`intl_plan_time_utc`] — the ORD/SMHI/NCI grammars), the
/// rows are retained to the exact `[start, end]` window and thinned by
/// evenly-spaced sampling that always keeps BOTH window edges — the same
/// `slot * last / (max - 1)` shape as the US event-loop limiter and the
/// ORD Unified-Player limiter (`limit_ord_archive_plans_for_window`).
///
/// A provider whose stamps do NOT all parse falls back to the legacy
/// newest-tail cap (`window_plans`' documented day-granular shape) — a
/// DOCUMENTED capability limit for stampless grammars, not silent drift.
fn limit_intl_rows_for_window(
    mut rows: Vec<ArchiveScanRow>,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    max: usize,
) -> Vec<ArchiveScanRow> {
    let max = max.max(1);
    let all_stamps_parse = rows.iter().all(|row| match &row.load {
        ArchiveScanLoad::IntlPlan(plan) => intl_plan_time_utc(&plan.identity).is_some(),
        // US rows cannot appear on the intl arm; treat them as parseable
        // rather than degrading the whole listing.
        ArchiveScanLoad::UsObject(_) => true,
    });
    if !all_stamps_parse {
        // Newest-tail fallback: keep the last `max`, oldest first.
        if rows.len() > max {
            rows.drain(..rows.len() - max);
        }
        return rows;
    }
    rows.retain(|row| row.time_utc >= start_utc && row.time_utc <= end_utc);
    rows.sort_by(|left, right| left.time_utc.cmp(&right.time_utc));
    if rows.len() <= max {
        return rows;
    }
    if max == 1 {
        // A one-frame "window" is the newest scan (the window's anchor
        // end), matching the ORD/US single-frame arms.
        return rows.into_iter().rev().take(1).collect();
    }
    let last = rows.len() - 1;
    let mut selected = Vec::with_capacity(max);
    let mut last_index = None;
    for slot in 0..max {
        let index = slot * last / (max - 1);
        if last_index != Some(index) {
            selected.push(rows[index].clone());
            last_index = Some(index);
        }
    }
    selected
}

/// Run `job` against a provider's archive source, with the derived-
/// capability errors as values (the same texts the greyed UI shows).
fn intl_archive_source<T>(
    provider_id: &str,
    job: impl FnOnce(&dyn data_source::international::ArchiveFrames) -> Result<T, String>,
) -> Result<T, String> {
    let providers = data_source::international::intl_providers();
    let provider = providers
        .iter()
        .find(|provider| provider.id() == provider_id)
        .ok_or_else(|| format!("unknown international provider '{provider_id}'"))?;
    let archive = provider
        .archive_source()
        .ok_or_else(|| format!("{} has no archive source", provider.label()))?;
    job(archive)
}

fn us_rows_from_objects(
    objects: Vec<data_source::S3Object>,
    date: NaiveDate,
) -> Vec<ArchiveScanRow> {
    let fallback = date_midnight_utc(date);
    objects
        .into_iter()
        .map(|object| ArchiveScanRow {
            time_utc: archive_object_scan_time_utc(&object).unwrap_or(fallback),
            load: ArchiveScanLoad::UsObject(object),
        })
        .collect()
}

fn intl_rows_from_plans(plans: Vec<FramePlan>, date: NaiveDate) -> Vec<ArchiveScanRow> {
    let fallback = date_midnight_utc(date);
    plans
        .into_iter()
        .map(|plan| ArchiveScanRow {
            time_utc: intl_plan_time_utc(&plan.identity).unwrap_or(fallback),
            load: ArchiveScanLoad::IntlPlan(plan),
        })
        .collect()
}

/// Rows -> intl plans (US rows, which cannot appear on an intl lister's
/// output, are dropped rather than panicking on upstream surprises).
fn intl_plans_from_rows(rows: Vec<ArchiveScanRow>) -> Vec<FramePlan> {
    rows.into_iter()
        .filter_map(|row| match row.load {
            ArchiveScanLoad::IntlPlan(plan) => Some(plan),
            ArchiveScanLoad::UsObject(_) => None,
        })
        .collect()
}

fn date_midnight_utc(date: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight exists"))
}

/// Scan time from a provider frame identity. Total over the shipped
/// archive grammars: ORD's `{site}_{%Y%m%dT%H%M}_p{n}_h{hash}`, SMHI's
/// `radar_{area}_qcvol_{%Y%m%d%H%M}`, and NCI's
/// `australia-nci_{site}_{%Y%m%d}_{%H%M%S}.pvol.h5` — any `_`-separated
/// segment (or adjacent date+time segment pair) in one of those stamp
/// shapes answers, so future providers with the same conventions inherit
/// hour grouping for free.
pub(crate) fn intl_plan_time_utc(identity: &str) -> Option<DateTime<Utc>> {
    let segments: Vec<&str> = identity.split('_').collect();
    for (index, segment) in segments.iter().enumerate() {
        let bytes = segment.as_bytes();
        let parsed = match bytes.len() {
            12 if bytes.iter().all(u8::is_ascii_digit) => {
                NaiveDateTime::parse_from_str(segment, "%Y%m%d%H%M").ok()
            }
            13 if bytes[8] == b'T' => NaiveDateTime::parse_from_str(segment, "%Y%m%dT%H%M").ok(),
            // A bare 8-digit date segment pairs with the next segment's
            // leading 6-digit HHMMSS (the NCI tarlist file grammar).
            8 if bytes.iter().all(u8::is_ascii_digit) => segments
                .get(index + 1)
                .and_then(|next| next.get(0..6))
                .filter(|hms| hms.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|hms| {
                    NaiveDateTime::parse_from_str(&format!("{segment}{hms}"), "%Y%m%d%H%M%S").ok()
                }),
            _ => None,
        };
        if let Some(naive) = parsed {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// The US volume-chip label, moved verbatim from the old
/// `poll_archive_listing` closure — the pixel-identity contract for the
/// US archive panel keys on this exact derivation (incl. the `??`
/// fallback for keys that do not parse).
fn us_archive_volume_label(object: &data_source::S3Object) -> String {
    // KXXX20260609_235423_V06 -> 23:54:23
    object
        .key
        .rsplit('/')
        .next()
        .and_then(|name| name.split('_').nth(1))
        .filter(|t| t.len() == 6)
        .map(|t| format!("{}:{}:{}", &t[0..2], &t[2..4], &t[4..6]))
        .unwrap_or_else(|| "??".to_owned())
}

/// Rows -> the US panel's `(object, label)` list — exactly what
/// `poll_archive_listing` built before the row union existed.
pub(crate) fn us_volumes_from_rows(
    rows: Vec<ArchiveScanRow>,
) -> Vec<(data_source::S3Object, String)> {
    rows.into_iter()
        .filter_map(|row| match row.load {
            ArchiveScanLoad::UsObject(object) => {
                let label = us_archive_volume_label(&object);
                Some((object, label))
            }
            ArchiveScanLoad::IntlPlan(_) => None,
        })
        .collect()
}

/// One listed international archive day, tagged with the display owner it
/// was listed FOR: a listing for a previous owner never renders under (or
/// loads into) a new one.
#[derive(Clone, Debug)]
pub(crate) struct IntlArchiveDayListing {
    pub(crate) provider_id: String,
    pub(crate) site_id: String,
    pub(crate) rows: Vec<ArchiveScanRow>,
}

impl ViewerApp {
    /// The site that owns the primary display, as a [`SiteRef`] — the
    /// Phase-2 shim derived from `PollSource` (matching
    /// `intl_source_owns_primary_display`'s ownership rule). Phase 3
    /// replaces this one function with the real site model; every
    /// dispatch seam in the archive world routes through here so that
    /// swap is a one-function change.
    pub(crate) fn display_owner_site(&self) -> SiteRef {
        if let FeedSource::Live(SiteRef::Intl {
            provider_id,
            site_id,
        }) = &self.primary.feed
            && !provider_id.is_empty()
            && !site_id.is_empty()
        {
            return SiteRef::Intl {
                provider_id: provider_id.clone(),
                site_id: site_id.clone(),
            };
        }
        SiteRef::Us {
            level2_id: self
                .selected_site()
                .map(|site| site.level2_id.clone())
                .unwrap_or_default(),
        }
    }

    /// THE archive browse/load surface (spec §5): date nav +
    /// hour-grouped minute chips + loop-ending-at-scan + "+N older",
    /// dispatched on the display owner's arm. The US arm renders the
    /// pre-Phase-2 panel byte-for-byte; intl arms reuse the same layout
    /// over provider plans; no-archive providers show the honest greyed
    /// reason instead of silently listing a stale US site.
    pub(crate) fn archive_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let owner = self.display_owner_site();
        match &owner {
            SiteRef::Us { .. } => self.us_archive_browser_body(ui, ctx),
            SiteRef::Intl {
                provider_id,
                site_id,
            } => match archive_access(&owner) {
                ArchiveAccess::None { reason } => {
                    ui.label(format!("{} {site_id}", intl_provider_label(provider_id)));
                    ui.add_enabled(false, egui::Button::new("List"))
                        .on_disabled_hover_text(reason);
                    ui.weak(reason);
                }
                ArchiveAccess::Provider | ArchiveAccess::Level2S3 => {
                    let provider_id = provider_id.clone();
                    let site_id = site_id.clone();
                    self.intl_archive_browser_body(&provider_id, &site_id, ui, ctx);
                }
            },
        }
        ui.separator();
        self.spc_reports_browser_section(ui, ctx);
    }

    /// The US archive listing body — moved VERBATIM from the old
    /// `archive_panel` (pre-Phase-2 main.rs): behavior, strings, sizes,
    /// and widget ids are the pixel-identity gate.
    fn us_archive_browser_body(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        // (The loop transport renders above this section in the DATA tab —
        // archive browsing shouldn't need a tab switch to play what it just
        // loaded.)
        panel_kit::row(ui, "Manual picker frames", |ui| {
            // Right-to-left: the frame count at the row edge, +5 older to
            // its left when a loaded range can grow.
            if ui
                .add(
                    egui::DragValue::new(&mut self.archive_frame_count)
                        .range(1..=MAX_ARCHIVE_FRAME_COUNT)
                        .speed(0.2),
                )
                .on_hover_text(
                    "Archive-browser selections only: load this many scans ending at the \
                     chosen scan. Tornado/report clicks use Event day Track/report loop \
                     controls.",
                )
                .changed()
            {
                self.persist_archive_controls();
                ctx.request_repaint();
            }
            if self.archive_loaded_range.is_some()
                && ui
                    .button("+5 older")
                    .on_hover_text(
                        "Prepend five older scans to the currently loaded archive loop only",
                    )
                    .clicked()
            {
                self.extend_archive_loop_earlier(5, ctx);
            }
        });
        if self.archive_date_input.is_empty() {
            self.archive_date_input = Utc::now().format("%Y-%m-%d").to_string();
        }
        ui.horizontal_wrapped(|ui| {
            // Day navigation: step the date and re-list immediately.
            // Wrapped so Today/List/Load fold under the date at 320 pt.
            let mut step_days: i64 = 0;
            if ui.small_button("◀").on_hover_text("Previous day").clicked() {
                step_days = -1;
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.archive_date_input)
                    .hint_text("YYYY-MM-DD")
                    .desired_width(88.0),
            );
            if ui.small_button("▶").on_hover_text("Next day").clicked() {
                step_days = 1;
            }
            if ui.small_button("Today").clicked() {
                self.archive_date_input = Utc::now().format("%Y-%m-%d").to_string();
                self.start_archive_listing(ctx);
            }
            if step_days != 0
                && let Ok(date) =
                    chrono::NaiveDate::parse_from_str(self.archive_date_input.trim(), "%Y-%m-%d")
            {
                let stepped = date + chrono::Duration::days(step_days);
                self.archive_date_input = stepped.format("%Y-%m-%d").to_string();
                self.start_archive_listing(ctx);
            }
            let listing = self.archive_list_receiver.is_some();
            if ui
                .add_enabled(!listing, egui::Button::new("List"))
                .on_hover_text("List this UTC date's volumes for the selected site")
                .clicked()
            {
                self.start_archive_listing(ctx);
            }
            if ui
                .button("Load")
                .on_hover_text(
                    "Load the newest listed scan for this date. In Loop mode, loads Fetch N scans ending at that scan.",
                )
                .clicked()
            {
                self.start_archive_default_load(ctx);
            }
            if listing {
                ui.spinner();
            }
        });
        let previous_archive_load_loop = self.archive_load_loop;
        // Wrapped, not a kit row: the two mode buttons outgrow the control
        // column at 320 pt and swapping them for a combo would change the
        // interaction (the plan forbids behavior changes).
        ui.horizontal_wrapped(|ui| {
            ui.label("On click:");
            ui.selectable_value(&mut self.archive_load_loop, true, "Loop ending at scan")
                .on_hover_text("Load Manual loop frames ending at the chosen archive scan");
            ui.selectable_value(&mut self.archive_load_loop, false, "Single scan")
                .on_hover_text("Load only the chosen scan");
        });
        if self.archive_load_loop != previous_archive_load_loop {
            self.persist_archive_controls();
        }
        if let Some(progress) = &self.archive_load_progress {
            archive_load_progress_row(ui, progress);
        }
        if let Some(volumes) = &self.archive_volumes {
            if volumes.is_empty() {
                ui.weak("No volumes for that date");
            } else {
                ui.weak(format!("{} volumes (UTC)", volumes.len()));
                let mut load_object: Option<usize> = None;
                egui::ScrollArea::vertical()
                    .id_salt("archive_volume_list")
                    .max_height(190.0)
                    .show(ui, |ui| {
                        // Hour headers + kit chip grids (even row split,
                        // truncating, full name on hover).
                        let mut index = 0usize;
                        while index < volumes.len() {
                            let hour = volumes[index].1.get(0..2).unwrap_or("??");
                            ui.weak(format!("{hour} UTC"));
                            let group_start = index;
                            while index < volumes.len()
                                && volumes[index].1.get(0..2).unwrap_or("??") == hour
                            {
                                index += 1;
                            }
                            let chips = volumes[group_start..index]
                                .iter()
                                .map(|volume| panel_kit::Chip {
                                    label: volume.1.get(3..8).unwrap_or(&volume.1),
                                    hotkey: None,
                                    selected: false,
                                    hover: Some(volume.1.clone()),
                                })
                                .collect::<Vec<_>>();
                            if let Some(clicked) = panel_kit::chip_grid(ui, &chips) {
                                load_object = Some(group_start + clicked);
                            }
                        }
                    });
                if let Some(index) = load_object {
                    self.start_archive_loop_load(index, ctx);
                }
            }
        }
    }

    /// The SPC tornado report fetch/jump rows — the tail of the old
    /// `archive_panel`, moved verbatim; date-based (US storm-report
    /// data), so it renders under every browser arm.
    fn spc_reports_browser_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        panel_kit::row(ui, "Tornado reports + tracks", |ui| {
            let fetching = self.spc_receiver.in_flight();
            if ui
                .add_enabled(!fetching, egui::Button::new("Fetch"))
                .on_hover_text(
                    "Fetch SPC tornado reports and pin the Event day track layer for this date. Click a report to load a centered archive loop.",
                )
                .clicked()
            {
                self.start_spc_fetch(ctx);
            }
            if fetching {
                ui.spinner();
            }
        });
        let mut jump: Option<SpcReport> = None;
        if let Some(reports) = &self.spc_reports {
            if reports.is_empty() {
                ui.weak("No tornado reports for that date");
            } else {
                ui.weak(format!("{} tornado reports", reports.len()));
                egui::ScrollArea::vertical()
                    .id_salt("spc_report_list")
                    .max_height(170.0)
                    .show(ui, |ui| {
                        // Kit select_row: left-aligned truncating monospace
                        // rows — never centered button text.
                        for report in reports {
                            let scale = if report.f_scale.is_empty() || report.f_scale == "UNK" {
                                String::new()
                            } else {
                                format!("EF{} ", report.f_scale)
                            };
                            let label = format!(
                                "{} {}{}, {}",
                                self.time_zone().format_hm(report.time_utc),
                                scale,
                                report.location,
                                report.state
                            );
                            if panel_kit::select_row(
                                ui,
                                false,
                                true,
                                &label,
                                Some(
                                    "Jump: lowest-beam radar + event archive loop before/after this time",
                                ),
                            )
                            .clicked()
                            {
                                jump = Some(report.clone());
                            }
                        }
                    });
            }
        }
        if let Some(report) = jump {
            self.jump_to_spc_report(&report, ctx);
        }
    }

    /// The international arm of the browser: the same date-nav +
    /// hour-grouped chips + On-click + "+5 older" surface over
    /// [`ArchiveScanRow`]s listed through the provider's
    /// `archive_source()`.
    fn intl_archive_browser_body(
        &mut self,
        provider_id: &str,
        site_id: &str,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
    ) {
        ui.weak(format!(
            "{} {site_id} archive",
            intl_provider_label(provider_id)
        ));
        panel_kit::row(ui, "Manual picker frames", |ui| {
            if ui
                .add(
                    egui::DragValue::new(&mut self.archive_frame_count)
                        .range(1..=MAX_ARCHIVE_FRAME_COUNT)
                        .speed(0.2),
                )
                .on_hover_text(
                    "Archive-browser selections only: load this many scans ending at the \
                     chosen scan.",
                )
                .changed()
            {
                self.persist_archive_controls();
                ctx.request_repaint();
            }
            if self.intl_archive_loaded_range.is_some()
                && ui
                    .button("+5 older")
                    .on_hover_text(
                        "Reload the loaded archive loop with five older scans from this listed day",
                    )
                    .clicked()
            {
                self.extend_intl_archive_loop_earlier(5, ctx);
            }
        });
        if self.archive_date_input.is_empty() {
            self.archive_date_input = Utc::now().format("%Y-%m-%d").to_string();
        }
        ui.horizontal_wrapped(|ui| {
            // Wrapped so Today/List/Load fold under the date at 320 pt.
            let mut step_days: i64 = 0;
            if ui.small_button("◀").on_hover_text("Previous day").clicked() {
                step_days = -1;
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.archive_date_input)
                    .hint_text("YYYY-MM-DD")
                    .desired_width(88.0),
            );
            if ui.small_button("▶").on_hover_text("Next day").clicked() {
                step_days = 1;
            }
            if ui.small_button("Today").clicked() {
                self.archive_date_input = Utc::now().format("%Y-%m-%d").to_string();
                self.start_intl_archive_day_listing(ctx);
            }
            if step_days != 0
                && let Ok(date) =
                    chrono::NaiveDate::parse_from_str(self.archive_date_input.trim(), "%Y-%m-%d")
            {
                let stepped = date + chrono::Duration::days(step_days);
                self.archive_date_input = stepped.format("%Y-%m-%d").to_string();
                self.start_intl_archive_day_listing(ctx);
            }
            let listing = self.intl_archive_list_rx.in_flight();
            if ui
                .add_enabled(!listing, egui::Button::new("List"))
                .on_hover_text("List this UTC date's archive scans for this site")
                .clicked()
            {
                self.start_intl_archive_day_listing(ctx);
            }
            if ui
                .button("Load")
                .on_hover_text(
                    "Load the newest listed scan for this date. In Loop mode, loads Fetch N scans ending at that scan.",
                )
                .clicked()
            {
                self.start_intl_archive_default_load(ctx);
            }
            if provider_id == "ord"
                && ui
                    .add_enabled(!listing, egui::Button::new("Load full day"))
                    .on_hover_text(
                        "List every ORD scan on this UTC date, then download and decode the day in bounded 20-scan phases. The 20 objects shown by MeteoGate are elevation coverages, not a scan limit.",
                    )
                    .clicked()
            {
                self.start_ord_full_day_load(ctx);
            }
            if listing {
                ui.spinner();
            }
        });
        let previous_archive_load_loop = self.archive_load_loop;
        // Wrapped, not a kit row: the two mode buttons outgrow the control
        // column at 320 pt and swapping them for a combo would change the
        // interaction (the plan forbids behavior changes).
        ui.horizontal_wrapped(|ui| {
            ui.label("On click:");
            ui.selectable_value(&mut self.archive_load_loop, true, "Loop ending at scan")
                .on_hover_text("Load Manual loop frames ending at the chosen archive scan");
            ui.selectable_value(&mut self.archive_load_loop, false, "Single scan")
                .on_hover_text("Load only the chosen scan");
        });
        if self.archive_load_loop != previous_archive_load_loop {
            self.persist_archive_controls();
        }
        if let Some(progress) = &self.archive_load_progress {
            archive_load_progress_row(ui, progress);
        }
        let mut load_row: Option<usize> = None;
        if let Some(listing) = &self.intl_archive_rows {
            // A listing for a previous display owner never renders under
            // (or loads into) the new one.
            if listing.provider_id == provider_id && listing.site_id == site_id {
                if listing.rows.is_empty() {
                    ui.weak("No archive scans for that date");
                } else {
                    ui.weak(format!("{} scans (UTC)", listing.rows.len()));
                    let rows = &listing.rows;
                    egui::ScrollArea::vertical()
                        .id_salt("intl_archive_scan_list")
                        .max_height(190.0)
                        .show(ui, |ui| {
                            let mut index = 0usize;
                            while index < rows.len() {
                                let hour = rows[index].time_utc.format("%H").to_string();
                                ui.weak(format!("{hour} UTC"));
                                let group_start = index;
                                while index < rows.len()
                                    && rows[index].time_utc.format("%H").to_string() == hour
                                {
                                    index += 1;
                                }
                                let labels = rows[group_start..index]
                                    .iter()
                                    .map(|row| {
                                        (
                                            row.time_utc.format("%M:%S").to_string(),
                                            intl_row_hover_text(row),
                                        )
                                    })
                                    .collect::<Vec<_>>();
                                let chips = labels
                                    .iter()
                                    .map(|(label, hover)| panel_kit::Chip {
                                        label,
                                        hotkey: None,
                                        selected: false,
                                        hover: Some(hover.clone()),
                                    })
                                    .collect::<Vec<_>>();
                                if let Some(clicked) = panel_kit::chip_grid(ui, &chips) {
                                    load_row = Some(group_start + clicked);
                                }
                            }
                        });
                }
            }
        }
        if let Some(index) = load_row {
            self.start_intl_archive_chip_load(index, ctx);
        }
    }

    /// Kick a background listing of the display owner's archive date
    /// (the intl mirror of `start_archive_listing`).
    pub(crate) fn start_intl_archive_day_listing(&mut self, ctx: &egui::Context) {
        let SiteRef::Intl {
            provider_id,
            site_id,
        } = self.display_owner_site()
        else {
            return;
        };
        // Browsing the archive is explicit intent — the live poll must
        // not stomp archive frames (same contract as the US browser).
        if self.poll_active {
            self.poll_active = false;
            self.status = "Live poll paused (archive browse)".to_owned();
        }
        let Ok(date) =
            chrono::NaiveDate::parse_from_str(self.archive_date_input.trim(), "%Y-%m-%d")
        else {
            self.status = "Archive date must be YYYY-MM-DD".to_owned();
            return;
        };
        self.status = format!(
            "Archive: listing {} {site_id} {date}",
            intl_provider_label(&provider_id)
        );
        self.intl_archive_rows = None;
        self.intl_archive_loaded_range = None;
        self.intl_archive_list_rx.cancel();
        self.intl_archive_list_rx.spawn(ctx, move |tx| {
            let lister = ArchiveLister::Intl {
                provider_id: provider_id.clone(),
                site_id: site_id.clone(),
            };
            let result = lister.list_day(date).map(|rows| IntlArchiveDayListing {
                provider_id,
                site_id,
                rows,
            });
            let _ = tx.send(result);
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    pub(crate) fn poll_intl_archive_listing(&mut self, ctx: &egui::Context) {
        match self.intl_archive_list_rx.poll() {
            SlotPoll::Ready(Ok(listing)) => {
                self.status = format!("Archive: {} scans listed", listing.rows.len());
                self.intl_archive_rows = Some(listing);
                if self.intl_archive_full_day_after_listing {
                    self.intl_archive_full_day_after_listing = false;
                    self.start_ord_full_day_load(ctx);
                    return;
                }
                if self.intl_archive_load_after_listing {
                    self.intl_archive_load_after_listing = false;
                    let newest = self
                        .intl_archive_rows
                        .as_ref()
                        .map(|listing| listing.rows.len())
                        .unwrap_or(0);
                    if newest == 0 {
                        self.status = "Archive: no scans to load for that date".to_owned();
                    } else {
                        self.start_intl_archive_chip_load(newest - 1, ctx);
                    }
                }
                ctx.request_repaint();
            }
            SlotPoll::Ready(Err(err)) => {
                self.intl_archive_rows = None;
                self.intl_archive_load_after_listing = false;
                self.intl_archive_full_day_after_listing = false;
                self.status = format!("Archive listing failed: {err}");
                ctx.request_repaint();
            }
            SlotPoll::Idle => {}
            SlotPoll::Pending => {
                ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
            }
            SlotPoll::Disconnected => {
                self.intl_archive_load_after_listing = false;
                self.intl_archive_full_day_after_listing = false;
                self.status = "Archive listing worker disconnected".to_owned();
            }
        }
    }

    /// The intl mirror of `start_archive_default_load`: load the newest
    /// listed scan (listing first when needed).
    fn start_intl_archive_default_load(&mut self, ctx: &egui::Context) {
        if self.intl_archive_list_rx.in_flight() {
            self.intl_archive_load_after_listing = true;
            self.status = "Archive: will load when the listing finishes".to_owned();
            ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
            return;
        }
        let current_rows = self.current_intl_archive_rows_len();
        if current_rows > 0 {
            self.start_intl_archive_chip_load(current_rows - 1, ctx);
            return;
        }
        self.intl_archive_load_after_listing = true;
        self.start_intl_archive_day_listing(ctx);
    }

    /// Load every scan listed for the current ORD UTC date. Download/decode
    /// is phased so a full day never exists twice as one giant worker-side
    /// batch before it reaches the history engine.
    fn start_ord_full_day_load(&mut self, ctx: &egui::Context) {
        let SiteRef::Intl {
            provider_id,
            site_id,
        } = self.display_owner_site()
        else {
            return;
        };
        if provider_id != "ord" {
            self.status = "Full-day phased loading is currently an ORD archive feature".to_owned();
            return;
        }
        if self.load_receiver.is_some() {
            self.status = "Wait for the current load to finish".to_owned();
            return;
        }
        if self.intl_archive_list_rx.in_flight() {
            self.intl_archive_full_day_after_listing = true;
            self.status = "ORD archive: full day will load when listing finishes".to_owned();
            return;
        }
        let Some(listing) = self
            .intl_archive_rows
            .as_ref()
            .filter(|listing| listing.provider_id == provider_id && listing.site_id == site_id)
        else {
            self.intl_archive_full_day_after_listing = true;
            self.start_intl_archive_day_listing(ctx);
            return;
        };
        if listing.rows.is_empty() {
            self.status = "ORD archive: no scans to load for that date".to_owned();
            return;
        }
        if listing.rows.len() > MAX_HISTORY_FRAME_LIMIT {
            self.status = format!(
                "ORD archive: {} scans exceed the {}-frame history safety limit",
                listing.rows.len(),
                MAX_HISTORY_FRAME_LIMIT
            );
            return;
        }
        let rows = listing.rows.clone();
        let end = rows.len() - 1;
        let label = format!(
            "ORD full day {site_id} {} ({} scans)",
            rows[0].time_utc.format("%Y-%m-%d"),
            rows.len()
        );
        self.start_intl_archive_row_batch(
            provider_id,
            site_id,
            rows,
            end,
            Some((0, end)),
            label,
            true,
            ctx,
        );
    }

    /// Row count of the listing IF it belongs to the current display
    /// owner (stale listings answer 0).
    fn current_intl_archive_rows_len(&self) -> usize {
        let SiteRef::Intl {
            provider_id,
            site_id,
        } = self.display_owner_site()
        else {
            return 0;
        };
        self.intl_archive_rows
            .as_ref()
            .filter(|listing| listing.provider_id == provider_id && listing.site_id == site_id)
            .map(|listing| listing.rows.len())
            .unwrap_or(0)
    }

    /// Chip click: load a loop of archive scans ending at the chosen row
    /// (the chosen scan plus the preceding Fetch-N-1 scans; Single mode
    /// loads only the chosen scan — the same On-click contract as the US
    /// browser).
    pub(crate) fn start_intl_archive_chip_load(&mut self, chosen: usize, ctx: &egui::Context) {
        let SiteRef::Intl {
            provider_id,
            site_id,
        } = self.display_owner_site()
        else {
            return;
        };
        let Some(listing) = self
            .intl_archive_rows
            .as_ref()
            .filter(|listing| listing.provider_id == provider_id && listing.site_id == site_id)
        else {
            self.status = "Archive: list a date before loading".to_owned();
            return;
        };
        if chosen >= listing.rows.len() {
            return;
        }
        let limit = if self.archive_load_loop {
            self.archive_frame_count.max(1)
        } else {
            1
        };
        let start = chosen.saturating_sub(limit - 1);
        let rows = listing.rows[start..=chosen].to_vec();
        let label = format!(
            "{} archive {site_id} {}",
            provider_id.to_ascii_uppercase(),
            listing.rows[chosen].time_utc.format("%Y-%m-%d %H:%MZ")
        );
        self.start_intl_archive_row_batch(
            provider_id,
            site_id,
            rows,
            chosen - start,
            Some((start, chosen)),
            label,
            false,
            ctx,
        );
    }

    /// "+N older": reload the loaded loop with `count` older scans from
    /// the listed day prepended.
    pub(crate) fn extend_intl_archive_loop_earlier(&mut self, count: usize, ctx: &egui::Context) {
        let SiteRef::Intl {
            provider_id,
            site_id,
        } = self.display_owner_site()
        else {
            return;
        };
        let Some(listing) = self
            .intl_archive_rows
            .as_ref()
            .filter(|listing| listing.provider_id == provider_id && listing.site_id == site_id)
        else {
            return;
        };
        let Some((start, end)) = self.intl_archive_loaded_range else {
            return;
        };
        if start == 0 || end >= listing.rows.len() || self.load_receiver.is_some() {
            return;
        }
        let new_start = start.saturating_sub(count);
        let rows = listing.rows[new_start..=end].to_vec();
        if rows.is_empty() {
            return;
        }
        let label = format!(
            "{} archive {site_id} {}-{}Z",
            provider_id.to_ascii_uppercase(),
            listing.rows[new_start].time_utc.format("%H:%M"),
            listing.rows[end].time_utc.format("%H:%M")
        );
        self.start_intl_archive_row_batch(
            provider_id,
            site_id,
            rows,
            end - new_start,
            Some((new_start, end)),
            label,
            false,
            ctx,
        );
    }

    /// The one intl archive plan-batch loader: rows -> plans -> the
    /// generalized `fetch_intl_frame_plan_batch` (spec §5 — the ORD/SMHI
    /// one-off fetchers collapse onto this path), installing through the
    /// shared decoded-load pipeline with the display owner set first.
    #[allow(clippy::too_many_arguments)]
    fn start_intl_archive_row_batch(
        &mut self,
        provider_id: String,
        site_id: String,
        rows: Vec<ArchiveScanRow>,
        selected_index: usize,
        loaded_range: Option<(usize, usize)>,
        label: String,
        phased: bool,
        ctx: &egui::Context,
    ) {
        let plans = intl_plans_from_rows(rows);
        if plans.is_empty() {
            self.status = "Archive: no scans selected".to_owned();
            return;
        }
        if self.load_receiver.is_some() {
            self.status = "Wait for the current load to finish".to_owned();
            return;
        }
        let total_frames = plans.len().min(MAX_HISTORY_FRAME_LIMIT);
        if total_frames > self.primary.limits.frame_limit {
            self.primary.limits.frame_limit = total_frames;
        }
        self.set_intl_archive_primary_source(&provider_id, &site_id);
        self.intl_loop_rx = None;
        self.intl_loop_requested = 0;
        self.pending_site_id = Some(site_id.clone());
        self.live_refresh_skip_reason = None;
        self.clear_frame_history();
        self.clear_displayed_volume_for_pending_load(ctx);
        self.intl_archive_loaded_range = loaded_range;

        let total = plans.len();
        let selected_index = selected_index.min(total.saturating_sub(1));
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.primary_load_is_auto_refresh = false;
        self.archive_load_progress = Some(ArchiveLoadProgress {
            label: label.clone(),
            detail: "queued".to_owned(),
            done: 0,
            total,
        });
        self.status = format!("{label} queued");
        let path_prefix = format!("{provider_id}-archive");
        let source_prefix = format!("{} archive", provider_id.to_ascii_uppercase());
        let worker_label = label.clone();
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            if phased {
                fetch_intl_frame_plan_batch_phased(
                    &site_id,
                    plans,
                    &worker_label,
                    &path_prefix,
                    &source_prefix,
                    &sender,
                );
            } else {
                fetch_intl_frame_plan_batch(
                    &site_id,
                    plans,
                    selected_index,
                    &worker_label,
                    &path_prefix,
                    &source_prefix,
                    &sender,
                );
            }
            ctx_clone.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    /// Point the archive browser at an international provider/site (the
    /// Data ▸ Radar coverage "Browse archive" button): the site becomes
    /// the display owner — exactly the ownership contract the old
    /// ORD/SMHI archive loads used — and today's date lists.
    pub(crate) fn open_intl_archive_browser(
        &mut self,
        provider_id: String,
        site_id: String,
        ctx: &egui::Context,
    ) {
        self.set_intl_archive_primary_source(&provider_id, &site_id);
        if self.archive_date_input.trim().is_empty() {
            self.archive_date_input = Utc::now().format("%Y-%m-%d").to_string();
        }
        self.start_intl_archive_day_listing(ctx);
        self.status = format!(
            "Archive: listing {} {site_id} — browse in Data > Archive",
            intl_provider_label(&provider_id)
        );
    }

    /// Unified Player "Archive Window" for an intl display owner with an
    /// archive source: `ArchiveFrames::window_plans` (newest `max`
    /// inside the window, oldest first) through the shared plan-batch
    /// loader. The ORD arm keeps its dedicated edge-preserving window
    /// loader; this generic arm serves SMHI and every future
    /// `archive_source()` provider unchanged.
    pub(crate) fn start_intl_archive_window_load(
        &mut self,
        provider_id: String,
        site_id: String,
        start_utc: DateTime<Utc>,
        end_utc: DateTime<Utc>,
        max_frames: usize,
        ctx: &egui::Context,
    ) {
        let max_frames = max_frames.clamp(1, MAX_HISTORY_FRAME_LIMIT);
        self.archive_date_input = start_utc.format("%Y-%m-%d").to_string();
        self.archive_frame_count = max_frames.min(MAX_ARCHIVE_FRAME_COUNT);
        self.archive_load_loop = true;
        self.primary.limits.frame_limit = self.primary.limits.frame_limit.max(max_frames);

        let label = format!(
            "{} archive {site_id} window {} to {}",
            provider_id.to_ascii_uppercase(),
            start_utc.format("%Y-%m-%d %H:%MZ"),
            end_utc.format("%Y-%m-%d %H:%MZ")
        );
        let replaced = self.load_receiver.take().is_some();
        self.set_intl_archive_primary_source(&provider_id, &site_id);
        self.intl_loop_rx = None;
        self.intl_loop_requested = 0;
        self.pending_site_id = Some(site_id.clone());
        self.live_refresh_skip_reason = None;
        self.clear_frame_history();
        self.clear_displayed_volume_for_pending_load(ctx);
        self.intl_archive_loaded_range = None;
        if self.unified_player.auto_sync_warnings {
            self.hazards_visible = true;
            self.hazards_active_only = false;
            self.live_hazard_auto_refresh = false;
            self.load_event_loop_hazards(start_utc, end_utc, ctx);
        }

        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.primary_load_is_auto_refresh = false;
        self.archive_load_progress = Some(ArchiveLoadProgress {
            label: label.clone(),
            detail: "listing provider archive".to_owned(),
            done: 0,
            total: max_frames,
        });
        self.status = if replaced {
            format!("Replaced active load; {label} queued")
        } else {
            format!("{label} queued")
        };
        let worker_label = label.clone();
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            fetch_intl_archive_window_batch(
                &provider_id,
                &site_id,
                start_utc,
                end_utc,
                max_frames,
                &worker_label,
                &sender,
            );
            ctx_clone.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    /// Unified Player "Loop Ending At" for an intl display owner with an
    /// archive source: the target date's `day_plans`, trimmed to scans at
    /// or before the target (identity-stamped), newest `count` kept.
    pub(crate) fn start_intl_archive_loop_ending_at(
        &mut self,
        provider_id: String,
        site_id: String,
        target_utc: DateTime<Utc>,
        count: usize,
        ctx: &egui::Context,
    ) {
        let count = count.clamp(1, MAX_HISTORY_FRAME_LIMIT);
        self.archive_date_input = target_utc.format("%Y-%m-%d").to_string();
        self.archive_frame_count = count.min(MAX_ARCHIVE_FRAME_COUNT);
        self.archive_load_loop = true;
        self.primary.limits.frame_limit = self.primary.limits.frame_limit.max(count);

        let label = format!(
            "{} archive {site_id} loop near {}",
            provider_id.to_ascii_uppercase(),
            target_utc.format("%Y-%m-%d %H:%MZ")
        );
        let replaced = self.load_receiver.take().is_some();
        self.set_intl_archive_primary_source(&provider_id, &site_id);
        self.intl_loop_rx = None;
        self.intl_loop_requested = 0;
        self.pending_site_id = Some(site_id.clone());
        self.live_refresh_skip_reason = None;
        self.clear_frame_history();
        self.clear_displayed_volume_for_pending_load(ctx);
        self.intl_archive_loaded_range = None;

        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.primary_load_is_auto_refresh = false;
        self.archive_load_progress = Some(ArchiveLoadProgress {
            label: label.clone(),
            detail: "listing provider archive".to_owned(),
            done: 0,
            total: count,
        });
        self.status = if replaced {
            format!("Replaced active load; {label} queued")
        } else {
            format!("{label} queued")
        };
        let worker_label = label.clone();
        let ctx_clone = ctx.clone();
        thread::spawn(move || {
            fetch_intl_archive_ending_at_batch(
                &provider_id,
                &site_id,
                target_utc,
                count,
                &worker_label,
                &sender,
            );
            ctx_clone.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    /// Event Loop Builder "Build Radar Loop" for an international plan:
    /// the intl mirror of `start_event_loop_archive_radar_load` — event
    /// chrome (warnings window, models, autoplay, centering) plus a
    /// provider archive window load.
    pub(crate) fn start_intl_event_loop_radar_load(
        &mut self,
        plan: crate::event_loop_builder::EventLoopRadarPlan,
        ctx: &egui::Context,
    ) -> String {
        let SiteRef::Intl {
            provider_id,
            site_id,
        } = plan.site.clone()
        else {
            // Callers dispatch on kind; stay total anyway.
            return "US event loops build on the Level-II archive loader".to_owned();
        };
        let replaced_active_load = self.load_receiver.take().is_some();
        if replaced_active_load {
            self.archive_load_progress = None;
            self.pending_site_id = None;
            self.live_refresh_skip_reason = None;
        }
        if plan.center_on_site
            && let Some(record) = data_source::sites::resolve(&SiteRef::Intl {
                provider_id: provider_id.clone(),
                site_id: site_id.clone(),
            })
            && let Some((lat, lon)) = record.lat_lon
        {
            self.center_map_on(lat, lon);
        }
        self.primary.live.enabled = false;
        self.pending_debug_archive_case = None;
        self.clear_camera_follow_targets();
        self.hazards_visible = plan.include_warnings;
        self.spc_reports_enabled = false;
        self.mping_enabled = false;
        if plan.include_warnings {
            self.hazards_active_only = false;
            self.live_hazard_auto_refresh = false;
            self.load_event_loop_hazards(plan.start_utc, plan.end_utc, ctx);
        } else {
            self.event_loop_hazard_window = None;
            self.pending_event_loop_hazard_window = None;
        }
        if plan.include_models {
            self.model_enabled = true;
            self.model_dock_open = true;
        }
        let max_frames = plan.max_frames.clamp(1, MAX_HISTORY_FRAME_LIMIT);
        self.primary.limits.frame_limit = self.primary.limits.frame_limit.max(max_frames);
        self.set_intl_archive_primary_source(&provider_id, &site_id);
        self.intl_loop_rx = None;
        self.intl_loop_requested = 0;
        self.pending_site_id = Some(site_id.clone());
        self.live_refresh_skip_reason = None;
        self.clear_frame_history();
        self.clear_displayed_volume_for_pending_load(ctx);
        self.intl_archive_loaded_range = None;
        self.event_explorer.pending_autoplay = plan.auto_play;

        let label = format!("Event loop {site_id}");
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.primary_load_is_auto_refresh = false;
        let progress = ArchiveLoadProgress::indeterminate(
            "Event loop radar",
            format!(
                "Listing {} {site_id} {} to {}",
                intl_provider_label(&provider_id),
                plan.start_utc.format("%Y-%m-%d %H:%MZ"),
                plan.end_utc.format("%Y-%m-%d %H:%MZ")
            ),
        );
        self.status = progress.status_text();
        self.archive_load_progress = Some(progress);
        let start_utc = plan.start_utc;
        let end_utc = plan.end_utc;
        let ctx_clone = ctx.clone();
        let worker_provider = provider_id.clone();
        let worker_site = site_id.clone();
        thread::spawn(move || {
            fetch_intl_archive_window_batch(
                &worker_provider,
                &worker_site,
                start_utc,
                end_utc,
                max_frames,
                &label,
                &sender,
            );
            ctx_clone.request_repaint();
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
        format!(
            "{}event loop radar queued: {site_id} {} to {}",
            if replaced_active_load {
                "Replaced active radar load; "
            } else {
                ""
            },
            start_utc.format("%H:%MZ"),
            end_utc.format("%H:%MZ")
        )
    }

    /// Extend the loaded archive loop further back in time: decode `count`
    /// volumes preceding the loaded range and let the (identity-sorted)
    /// frame history slot them in order.
    fn extend_archive_loop_earlier(&mut self, count: usize, ctx: &egui::Context) {
        let Some(site) = self.selected_site().cloned() else {
            return;
        };
        let Some(volumes) = &self.archive_volumes else {
            return;
        };
        let Some((start, chosen)) = self.archive_loaded_range else {
            return;
        };
        if start == 0 || self.load_receiver.is_some() {
            return;
        }
        let new_start = start.saturating_sub(count);
        let objects: Vec<data_source::S3Object> = volumes[new_start..start]
            .iter()
            .map(|(object, _)| object.clone())
            .collect();
        if objects.is_empty() {
            return;
        }
        let total_frames = chosen - new_start + 1;
        if total_frames > self.primary.limits.frame_limit {
            self.primary.limits.frame_limit = total_frames;
        }
        self.archive_loaded_range = Some((new_start, chosen));
        let site_id = site.level2_id.clone();
        self.begin_primary_load_telemetry();
        let (sender, receiver) = mpsc::channel();
        self.load_receiver = Some(receiver);
        self.primary_load_is_auto_refresh = false;
        self.pending_site_id = Some(site_id.clone());
        let progress = ArchiveLoadProgress {
            label: "Archive extend".to_owned(),
            detail: format!("Queued {site_id} {} earlier scans", objects.len()),
            done: 0,
            total: objects.len(),
        };
        self.status = progress.status_text();
        self.archive_load_progress = Some(progress);
        let site_cache = cache_dir(&site.level2_id);
        let known_frame_paths = self.current_history_paths();
        thread::spawn(move || {
            let total_start = Instant::now();
            let mut decoded_frames = Vec::new();
            let count = objects.len();
            let progress_label = "Archive extend";
            for (index, object) in objects.into_iter().enumerate() {
                let object_name = object
                    .key
                    .rsplit('/')
                    .next()
                    .unwrap_or(&object.key)
                    .to_owned();
                send_archive_progress(
                    &sender,
                    progress_label,
                    format!("Fetching {site_id} {object_name}"),
                    index,
                    count,
                );
                match decode_archive_history_object(
                    &site_id,
                    object,
                    &site_cache,
                    &known_frame_paths,
                    None,
                    total_start,
                    &sender,
                    false,
                    true,
                ) {
                    Ok(Some(decoded)) => {
                        let _ = sender.send(AsyncLoadResult {
                            label: format!("L2 {site_id} archive extend"),
                            update: AsyncLoadUpdate::History(
                                DecodedLoadBatch {
                                    frames: vec![decoded.clone()],
                                    selected_index: 0,
                                },
                                false,
                            ),
                        });
                        decoded_frames.push(decoded);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        send_archive_progress(
                            &sender,
                            progress_label,
                            format!("Failed {site_id} {object_name}"),
                            index + 1,
                            count,
                        );
                        let _ = sender.send(AsyncLoadResult {
                            label: format!("L2 {site_id} archive extend"),
                            update: AsyncLoadUpdate::Final(Err(err)),
                        });
                        return;
                    }
                }
                send_archive_progress(
                    &sender,
                    progress_label,
                    format!("Decoded {site_id} {object_name}"),
                    index + 1,
                    count,
                );
            }
            let _ = sender.send(AsyncLoadResult {
                label: format!("L2 {site_id} archive extend"),
                update: AsyncLoadUpdate::Unchanged {
                    timings: None,
                    reason: format!("loop extended {} volumes earlier", decoded_frames.len()),
                },
            });
        });
        ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
    }

    /// Kick a background listing of the archive date's volumes.
    fn start_archive_default_load(&mut self, ctx: &egui::Context) {
        if self.archive_list_receiver.is_some() {
            self.archive_load_after_listing = true;
            self.status = "Archive: will load when the listing finishes".to_owned();
            ctx.request_repaint_after(Duration::from_millis(ACTIVE_LOAD_POLL_MS));
            return;
        }
        if let Some(volumes) = &self.archive_volumes
            && !volumes.is_empty()
        {
            let newest = volumes.len() - 1;
            self.start_archive_loop_load(newest, ctx);
            return;
        }
        self.archive_load_after_listing = true;
        self.start_archive_listing(ctx);
    }

    /// Kick a background listing of the archive date's volumes.
    pub(crate) fn start_archive_listing(&mut self, ctx: &egui::Context) {
        let Some(site) = self.selected_site().cloned() else {
            self.status = "No site selected".to_owned();
            return;
        };
        // Browsing the archive is explicit intent — same contract as
        // explicit site loads: the URL poll must not stomp archive frames.
        if self.poll_active {
            self.poll_active = false;
            self.status = "URL poll paused (archive browse)".to_owned();
        }
        let Ok(date) =
            chrono::NaiveDate::parse_from_str(self.archive_date_input.trim(), "%Y-%m-%d")
        else {
            self.status = "Archive date must be YYYY-MM-DD".to_owned();
            return;
        };
        let progress = ArchiveLoadProgress::listing(&site.level2_id, date);
        self.status = progress.status_text();
        self.archive_load_progress = Some(progress);
        let (sender, receiver) = mpsc::channel();
        self.archive_list_receiver = Some(receiver);
        self.archive_volumes = None;
        let site_id = site.level2_id.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let lister = ArchiveLister::Us { level2_id: site_id };
            let result = lister.list_day(date);
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    pub(crate) fn poll_archive_listing(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.archive_list_receiver else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(rows)) => {
                self.archive_list_receiver = None;
                self.archive_load_progress = None;
                let volumes = us_volumes_from_rows(rows);
                self.status = format!("Archive: {} volumes listed", volumes.len());
                self.archive_volumes = Some(volumes);
                if self.archive_load_after_listing {
                    self.archive_load_after_listing = false;
                    if let Some(volumes) = &self.archive_volumes {
                        if volumes.is_empty() {
                            self.status = "Archive: no volumes to load for that date".to_owned();
                        } else {
                            let newest = volumes.len() - 1;
                            self.start_archive_loop_load(newest, ctx);
                        }
                    }
                }
                ctx.request_repaint();
            }
            Ok(Err(err)) => {
                self.archive_list_receiver = None;
                self.archive_load_progress = None;
                self.archive_load_after_listing = false;
                self.status = format!("Archive listing failed: {err}");
                ctx.request_repaint();
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.archive_list_receiver = None;
                self.archive_load_progress = None;
                self.archive_load_after_listing = false;
            }
        }
    }

    /// Load a loop of archive volumes ending at the chosen index (the
    /// chosen scan plus the preceding history-limit-1 scans), through the
    /// normal decode/install pipeline.
    pub(crate) fn start_archive_loop_load(&mut self, chosen: usize, ctx: &egui::Context) -> bool {
        let limit = if self.archive_load_loop {
            self.archive_frame_count.max(1)
        } else {
            1
        };
        self.start_archive_range_load(chosen.saturating_sub(limit - 1), chosen, ctx)
    }
}

const ORD_FULL_DAY_DECODE_PHASE: usize = 20;

/// Reuse the proven international batch decoder in bounded phases. One
/// successful phase is held back so the final message can select the newest
/// frame; earlier phases stream through `History` and are installed while the
/// next phase downloads. Dropping the outer receiver makes the next send fail
/// and cooperatively stops the remaining day.
fn fetch_intl_frame_plan_batch_phased(
    site_id: &str,
    plans: Vec<FramePlan>,
    label: &str,
    path_prefix: &str,
    source_prefix: &str,
    sender: &mpsc::Sender<AsyncLoadResult>,
) {
    let total = plans.len();
    let phase_total = total.div_ceil(ORD_FULL_DAY_DECODE_PHASE);
    let mut pending_batch: Option<DecodedLoadBatch> = None;
    let mut errors = Vec::new();

    for (phase_index, chunk) in plans.chunks(ORD_FULL_DAY_DECODE_PHASE).enumerate() {
        let phase_start = phase_index * ORD_FULL_DAY_DECODE_PHASE;
        let phase_plans = chunk.to_vec();
        let selected_index = phase_plans.len().saturating_sub(1);
        let (phase_sender, phase_receiver) = mpsc::channel();
        fetch_intl_frame_plan_batch(
            site_id,
            phase_plans,
            selected_index,
            label,
            path_prefix,
            source_prefix,
            &phase_sender,
        );
        drop(phase_sender);

        while let Ok(message) = phase_receiver.recv() {
            match message.update {
                AsyncLoadUpdate::ArchiveProgress(mut progress) => {
                    progress.label = format!("{label} phase {}/{}", phase_index + 1, phase_total);
                    progress.done = (phase_start + progress.done).min(total);
                    progress.total = total;
                    if sender
                        .send(AsyncLoadResult {
                            label: label.to_owned(),
                            update: AsyncLoadUpdate::ArchiveProgress(progress),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                AsyncLoadUpdate::Final(Ok(batch)) => {
                    if let Some(previous) = pending_batch.replace(batch)
                        && sender
                            .send(AsyncLoadResult {
                                label: label.to_owned(),
                                update: AsyncLoadUpdate::History(previous, false),
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                AsyncLoadUpdate::Final(Err(error)) => errors.push(error),
                // The reused worker currently emits only progress + final;
                // forward any future incremental update rather than dropping
                // it if that contract expands.
                update => {
                    if sender
                        .send(AsyncLoadResult {
                            label: label.to_owned(),
                            update,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    }

    let update = pending_batch.map_or_else(
        || {
            AsyncLoadUpdate::Final(Err(errors.pop().unwrap_or_else(|| {
                "ORD full-day archive contained no decodable scans".to_owned()
            })))
        },
        |batch| AsyncLoadUpdate::Final(Ok(batch)),
    );
    let _ = sender.send(AsyncLoadResult {
        label: label.to_owned(),
        update,
    });
}

/// Worker body for intl archive-window loads: list through
/// `window_plans` (thinned per census D15 — see
/// [`limit_intl_rows_for_window`]), then stream the plans through the one
/// generalized plan-batch fetcher. Shared by the PRIMARY Unified-Player
/// window load and the Phase-4c coordinated OVERLAY window load
/// (`start_intl_radar_layer_archive_window_load`), which is why it is
/// `pub(crate)`.
pub(crate) fn fetch_intl_archive_window_batch(
    provider_id: &str,
    site_id: &str,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    max_frames: usize,
    label: &str,
    sender: &mpsc::Sender<AsyncLoadResult>,
) {
    let lister = ArchiveLister::Intl {
        provider_id: provider_id.to_owned(),
        site_id: site_id.to_owned(),
    };
    let plans = match lister
        .list_window(start_utc, end_utc, max_frames)
        .map(intl_plans_from_rows)
    {
        Ok(plans) if !plans.is_empty() => plans,
        Ok(_) => {
            let _ = sender.send(AsyncLoadResult {
                label: label.to_owned(),
                update: AsyncLoadUpdate::Final(Err(format!(
                    "no archive scans for {site_id} in {} to {}",
                    start_utc.format("%Y-%m-%d %H:%MZ"),
                    end_utc.format("%Y-%m-%d %H:%MZ")
                ))),
            });
            return;
        }
        Err(err) => {
            let _ = sender.send(AsyncLoadResult {
                label: label.to_owned(),
                update: AsyncLoadUpdate::Final(Err(err)),
            });
            return;
        }
    };
    let selected_index = plans.len().saturating_sub(1);
    let path_prefix = format!("{provider_id}-archive");
    let source_prefix = format!("{} archive", provider_id.to_ascii_uppercase());
    fetch_intl_frame_plan_batch(
        site_id,
        plans,
        selected_index,
        label,
        &path_prefix,
        &source_prefix,
        sender,
    );
}

/// Worker body for intl loop-ending-at loads: the target date's
/// `day_plans`, trimmed to identity stamps at or before the target,
/// newest `count` kept (scans without a parseable stamp anchor at the
/// date's midnight, so they only survive the trim for early targets).
fn fetch_intl_archive_ending_at_batch(
    provider_id: &str,
    site_id: &str,
    target_utc: DateTime<Utc>,
    count: usize,
    label: &str,
    sender: &mpsc::Sender<AsyncLoadResult>,
) {
    let date = target_utc.date_naive();
    let lister = ArchiveLister::Intl {
        provider_id: provider_id.to_owned(),
        site_id: site_id.to_owned(),
    };
    let rows = match lister.list_day(date) {
        Ok(rows) => rows,
        Err(err) => {
            let _ = sender.send(AsyncLoadResult {
                label: label.to_owned(),
                update: AsyncLoadUpdate::Final(Err(err)),
            });
            return;
        }
    };
    let Some(end) = rows.iter().rposition(|row| row.time_utc <= target_utc) else {
        let _ = sender.send(AsyncLoadResult {
            label: label.to_owned(),
            update: AsyncLoadUpdate::Final(Err(format!(
                "no archive scans at or before {} for {site_id}",
                target_utc.format("%Y-%m-%d %H:%MZ")
            ))),
        });
        return;
    };
    let start = (end + 1).saturating_sub(count.max(1));
    let plans = intl_plans_from_rows(rows[start..=end].to_vec());
    let selected_index = plans.len().saturating_sub(1);
    let path_prefix = format!("{provider_id}-archive");
    let source_prefix = format!("{} archive", provider_id.to_ascii_uppercase());
    fetch_intl_frame_plan_batch(
        site_id,
        plans,
        selected_index,
        label,
        &path_prefix,
        &source_prefix,
        sender,
    );
}

fn intl_row_hover_text(row: &ArchiveScanRow) -> String {
    match &row.load {
        ArchiveScanLoad::IntlPlan(plan) => format!(
            "{} · {} · {} part{}",
            row.time_utc.format("%Y-%m-%d %H:%M:%SZ"),
            crate::compact_intl_identity(&plan.identity),
            plan.parts.len(),
            if plan.parts.len() == 1 { "" } else { "s" }
        ),
        ArchiveScanLoad::UsObject(object) => object.key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3(key: &str) -> data_source::S3Object {
        data_source::S3Object {
            key: key.to_owned(),
            size: 1,
            last_modified: None,
        }
    }

    fn plan(identity: &str) -> FramePlan {
        FramePlan {
            identity: identity.to_owned(),
            parts: vec![data_source::international::PlanPart {
                url: format!("https://example.invalid/{identity}.h5"),
            }],
            merge: false,
        }
    }

    /// Census D15 (owner decision 2026-07-02): over-cap intl archive
    /// windows thin by EVENLY-SPACED sampling that keeps BOTH window
    /// edges, wherever the identity stamps parse — the ORD grammar here.
    /// Rows stamped outside `[start, end]` (day-fold ride-alongs) drop.
    #[test]
    fn intl_window_rows_thin_evenly_and_keep_both_edges_when_stamps_parse() {
        let start = Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 9, 6, 0, 0).unwrap();
        // 13 in-window scans on a 5-minute cadence, plus one day-fold
        // ride-along BEFORE the window start.
        let mut plans: Vec<FramePlan> = vec![plan(&format!(
            "deess_{}_p1_hride",
            (start - chrono::Duration::minutes(30)).format("%Y%m%dT%H%M")
        ))];
        plans.extend((0..13).map(|i| {
            plan(&format!(
                "deess_{}_p1_h{i:03}",
                (start + chrono::Duration::minutes(5 * i)).format("%Y%m%dT%H%M")
            ))
        }));
        let rows = intl_rows_from_plans(plans, start.date_naive());

        let limited = limit_intl_rows_for_window(rows, start, end, 5);
        let minutes: Vec<i64> = limited
            .iter()
            .map(|row| (row.time_utc - start).num_minutes())
            .collect();
        assert_eq!(
            minutes,
            vec![0, 15, 30, 45, 60],
            "evenly spaced, exact-window retain, START and END edges kept"
        );
    }

    /// Census D15 fallback arm: a provider whose stamps do NOT parse keeps
    /// the legacy newest-tail cap — a DOCUMENTED capability limit for
    /// stampless grammars, not silent drift.
    #[test]
    fn intl_window_rows_fall_back_to_newest_tail_for_unparseable_stamps() {
        let start = Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap();
        let end = start + chrono::Duration::hours(1);
        let rows: Vec<ArchiveScanRow> = (0..6)
            .map(|i| ArchiveScanRow {
                time_utc: start + chrono::Duration::minutes(5 * i),
                load: ArchiveScanLoad::IntlPlan(plan(&format!("mystery-scan-{i}"))),
            })
            .collect();
        let limited = limit_intl_rows_for_window(rows, start, end, 3);
        let minutes: Vec<i64> = limited
            .iter()
            .map(|row| (row.time_utc - start).num_minutes())
            .collect();
        assert_eq!(
            minutes,
            vec![15, 20, 25],
            "newest-tail: the window START edge drops first (legacy shape)"
        );
    }

    /// D15 edge cases: an under-cap window passes through untouched, and
    /// a one-frame cap keeps the NEWEST scan (the window's end anchor).
    #[test]
    fn intl_window_row_limit_edge_cases() {
        let start = Utc.with_ymd_and_hms(2026, 6, 9, 5, 0, 0).unwrap();
        let end = start + chrono::Duration::hours(1);
        let rows: Vec<ArchiveScanRow> = (0..3)
            .map(|i| {
                let time = start + chrono::Duration::minutes(10 * i);
                ArchiveScanRow {
                    time_utc: time,
                    load: ArchiveScanLoad::IntlPlan(plan(&format!(
                        "deess_{}_p1_hedge",
                        time.format("%Y%m%dT%H%M")
                    ))),
                }
            })
            .collect();
        assert_eq!(
            limit_intl_rows_for_window(rows.clone(), start, end, 10).len(),
            3,
            "under the cap nothing thins"
        );
        let newest = limit_intl_rows_for_window(rows, start, end, 1);
        assert_eq!(newest.len(), 1);
        assert_eq!(
            (newest[0].time_utc - start).num_minutes(),
            20,
            "max=1 keeps the newest scan"
        );
    }

    /// Pixel-identity contract for the US arm: row assembly + the
    /// `us_volumes_from_rows` conversion reproduce the exact
    /// `(object, label)` list the pre-Phase-2 `poll_archive_listing`
    /// built — including listing order and the `??` fallback.
    #[test]
    fn us_rows_reproduce_the_old_volume_labels_byte_for_byte() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 9).unwrap();
        let objects = vec![
            s3("KEAX/KEAX20260609_055123_V06"),
            s3("KEAX/KEAX20260609_055723_V06"),
            s3("KEAX/not-a-volume-name"),
        ];
        // The OLD derivation, verbatim from the pre-Phase-2 closure.
        let old: Vec<(data_source::S3Object, String)> = objects
            .clone()
            .into_iter()
            .map(|object| {
                let label = object
                    .key
                    .rsplit('/')
                    .next()
                    .and_then(|name| name.split('_').nth(1))
                    .filter(|t| t.len() == 6)
                    .map(|t| format!("{}:{}:{}", &t[0..2], &t[2..4], &t[4..6]))
                    .unwrap_or_else(|| "??".to_owned());
                (object, label)
            })
            .collect();

        let rows = us_rows_from_objects(objects, date);
        let new = us_volumes_from_rows(rows);

        assert_eq!(new.len(), old.len());
        for ((new_object, new_label), (old_object, old_label)) in new.iter().zip(old.iter()) {
            assert_eq!(new_object.key, old_object.key, "listing order preserved");
            assert_eq!(new_label, old_label);
        }
        assert_eq!(new[0].1, "05:51:23");
        assert_eq!(new[2].1, "??");
    }

    #[test]
    fn us_rows_carry_scan_times_with_midnight_fallback() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 9).unwrap();
        let rows = us_rows_from_objects(
            vec![s3("KEAX/KEAX20260609_055123_V06"), s3("KEAX/junk")],
            date,
        );
        assert_eq!(
            rows[0].time_utc,
            Utc.with_ymd_and_hms(2026, 6, 9, 5, 51, 23).unwrap()
        );
        assert_eq!(
            rows[1].time_utc,
            Utc.with_ymd_and_hms(2026, 6, 9, 0, 0, 0).unwrap()
        );
    }

    /// The shipped archive identity grammars parse; junk does not.
    #[test]
    fn intl_plan_time_parses_ord_smhi_and_nci_identity_stamps() {
        assert_eq!(
            intl_plan_time_utc("deess_20260609T0550_p3_h00deadbeef000000"),
            Some(Utc.with_ymd_and_hms(2026, 6, 9, 5, 50, 0).unwrap())
        );
        assert_eq!(
            intl_plan_time_utc("radar_angelholm_qcvol_202606120625"),
            Some(Utc.with_ymd_and_hms(2026, 6, 12, 6, 25, 0).unwrap())
        );
        assert_eq!(
            intl_plan_time_utc("australia-nci_2_2_20260625_143000.pvol.h5"),
            Some(Utc.with_ymd_and_hms(2026, 6, 25, 14, 30, 0).unwrap())
        );
        assert_eq!(intl_plan_time_utc("no_stamp_here"), None);
        assert_eq!(intl_plan_time_utc("bad_20261399T9999_p1_h00"), None);
        assert_eq!(intl_plan_time_utc("dated_20260625_but-no-time"), None);
    }

    #[test]
    fn intl_rows_wrap_plans_with_identity_times_and_midnight_fallback() {
        let date = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let rows = intl_rows_from_plans(
            vec![
                plan("radar_angelholm_qcvol_202606120625"),
                plan("stampless-identity"),
            ],
            date,
        );
        assert_eq!(
            rows[0].time_utc,
            Utc.with_ymd_and_hms(2026, 6, 12, 6, 25, 0).unwrap()
        );
        assert!(matches!(rows[0].load, ArchiveScanLoad::IntlPlan(_)));
        assert_eq!(
            rows[1].time_utc,
            Utc.with_ymd_and_hms(2026, 6, 12, 0, 0, 0).unwrap()
        );
    }

    /// The honest-grey gate: reasons are values from the SAME call that
    /// powers the loaders (spec §1.3), keyed on the derived
    /// `supports_archive()` capability — ORD/SMHI answer `Provider`,
    /// DWD answers `None` with the hover reason, US answers `Level2S3`.
    #[test]
    fn archive_access_derives_from_provider_capability() {
        assert_eq!(
            archive_access(&SiteRef::Us {
                level2_id: "KTLX".to_owned()
            }),
            ArchiveAccess::Level2S3
        );
        for provider_id in ["ord", "smhi"] {
            assert_eq!(
                archive_access(&SiteRef::Intl {
                    provider_id: provider_id.to_owned(),
                    site_id: "x".to_owned()
                }),
                ArchiveAccess::Provider,
                "{provider_id}"
            );
        }
        assert_eq!(
            archive_access(&SiteRef::Intl {
                provider_id: "dwd".to_owned(),
                site_id: "deess".to_owned()
            }),
            ArchiveAccess::None {
                reason: REASON_NO_PROVIDER_ARCHIVE
            }
        );
        assert_eq!(
            archive_access(&SiteRef::Intl {
                provider_id: "nonesuch".to_owned(),
                site_id: "x".to_owned()
            }),
            ArchiveAccess::None {
                reason: REASON_UNKNOWN_PROVIDER
            }
        );
    }

    /// The tripwire twin: every provider the registry says supports
    /// archive answers `Provider` here, everything else `None` — the
    /// gate can never drift from the derived capability set.
    #[test]
    fn archive_access_matches_the_derived_capability_for_every_provider() {
        for provider in data_source::international::intl_providers() {
            let access = archive_access(&SiteRef::Intl {
                provider_id: provider.id().to_owned(),
                site_id: "any".to_owned(),
            });
            if provider.supports_archive() {
                assert_eq!(access, ArchiveAccess::Provider, "{}", provider.id());
            } else {
                assert!(
                    matches!(access, ArchiveAccess::None { .. }),
                    "{}",
                    provider.id()
                );
            }
        }
    }
}
