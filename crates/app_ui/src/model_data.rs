//! Model data dock — rusty-weather's rw-ui panels mounted inside BowEcho.
//!
//! The panels (run browser, false-color field viewer, skew-T sounding) were
//! built to take a `&mut egui::Ui` from any egui host; all store IO runs on
//! rw-ui's own worker thread, so BowEcho's render loop never blocks. The
//! data source is an rw-store directory on disk (produced by rusty-weather
//! ingest, default `C:\Users\drew\rusty-weather\store`).

use eframe::egui;
use rw_ui::{
    ColorTableEditorPanel, FieldViewerEvent, FieldViewerPanel, HourKey, PlotViewerPanel,
    RunBrowserPanel, SoundingPanel, StoreRequest, StoreResponse, StoreTree, StoreView, StoreWorker,
    StyleOverrideSettings,
};
use std::path::PathBuf;

pub struct ModelDataDock {
    worker: StoreWorker,
    store_root: PathBuf,
    tree: Option<StoreTree>,
    browser: RunBrowserPanel,
    viewer: FieldViewerPanel,
    sounding: SoundingPanel,
    /// Most recent loaded field (kept for the map layer).
    latest_field: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// Most recent sounding data (kept for the native skew-T window).
    latest_sounding: Option<std::sync::Arc<rw_ui::SoundingData>>,
    /// One-shot: the user asked to put the current field on the radar map.
    map_request: Option<std::sync::Arc<rw_ui::FieldData>>,
    /// v0.2.3 custom-domain plot viewer: renders the selected field through
    /// rusty-weather's native plot pipeline over a user-chosen domain (shift-
    /// drag a box on the field viewer, or rotate a corner). Shown as a floating
    /// window when `show_plot_viewer` is set.
    plot_viewer: PlotViewerPanel,
    show_plot_viewer: bool,
    /// v0.2.3 user-editable model field-plot color tables (rw-ui). Distinct
    /// from the radar-side table editor: this edits the STYLE OVERRIDES the
    /// store worker resolves palettes through. Edits are pushed to the worker
    /// and the current field reloaded so the new palette shows.
    color_tables: ColorTableEditorPanel,
    show_color_tables: bool,
}

impl ModelDataDock {
    pub fn new(ctx: &egui::Context, store_root: PathBuf) -> Self {
        let repaint = ctx.clone();
        let worker = StoreWorker::spawn(StoreView::new(&store_root), move || {
            repaint.request_repaint();
        });
        worker.send(StoreRequest::Enumerate);
        Self {
            worker,
            store_root,
            tree: None,
            browser: RunBrowserPanel::new(),
            viewer: FieldViewerPanel::new(),
            sounding: SoundingPanel::new(),
            latest_field: None,
            latest_sounding: None,
            map_request: None,
            plot_viewer: PlotViewerPanel::new(),
            show_plot_viewer: false,
            color_tables: ColorTableEditorPanel::new(),
            show_color_tables: false,
        }
    }

    /// Push edited color-table style overrides to the store worker and reload
    /// the current field so the new palette shows (mirrors the rusty-weather
    /// reference host). The `StyleOverridesApplied` ack is a no-op — the reload
    /// is what repaints.
    fn apply_color_table_changes(&mut self) {
        let settings = self.color_tables.settings().clone().normalized();
        self.worker.send(StoreRequest::SetStyleOverrides(settings));
        self.plot_viewer.clear();
        if let Some(field) = self.viewer.wanted_field() {
            self.viewer.set_loading(&field.var);
            self.worker.send(StoreRequest::LoadField(field));
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(ctx: &egui::Context, tree: StoreTree) -> Self {
        let mut dock = Self::new(ctx, std::env::temp_dir().join("bowecho-model-dock-test"));
        dock.tree = Some(tree);
        dock
    }

    fn select_hour(&mut self, key: HourKey) {
        self.worker.send(StoreRequest::LoadHour(key));
    }

    /// Drain worker responses into panel state (mirrors the rusty-weather
    /// reference host).
    fn handle_responses(&mut self) {
        while let Some(response) = self.worker.try_recv() {
            match response {
                StoreResponse::Tree(tree) => {
                    if self.browser.selected().is_none() {
                        let first = tree.models.first().and_then(|model| {
                            model.runs.first().and_then(|run| {
                                run.hours.first().map(|hour| HourKey {
                                    model: model.model.clone(),
                                    run: run.run.clone(),
                                    hour: hour.hour,
                                })
                            })
                        });
                        if let Some(key) = first {
                            self.browser.select(key.clone());
                            self.select_hour(key);
                        }
                    }
                    self.tree = Some(tree);
                }
                StoreResponse::HourVars(key, Ok(vars)) => {
                    if self.browser.selected() == Some(&key) {
                        self.viewer.set_hour(key, vars);
                        if let Some(field) = self.viewer.wanted_field() {
                            self.viewer.set_loading(&field.var);
                            self.worker.send(StoreRequest::LoadField(field));
                        }
                    }
                }
                StoreResponse::HourVars(_, Err(message)) => {
                    self.viewer.set_error(message);
                }
                StoreResponse::Field(key, boxed) => match *boxed {
                    Ok(field) => {
                        self.latest_field = Some(std::sync::Arc::new(field.clone()));
                        self.viewer.set_field(field);
                    }
                    Err(message) => {
                        if self.viewer.wanted_field().as_ref() == Some(&key) {
                            self.viewer.set_error(message);
                        }
                    }
                },
                StoreResponse::Sounding(_, Ok(data)) => {
                    self.latest_sounding = Some(std::sync::Arc::new(data.clone()));
                    self.sounding.set_data(data);
                }
                StoreResponse::Sounding(_, Err(message)) => {
                    self.sounding.set_error(message);
                }
                // v0.2.3: worker ack that the style overrides were applied.
                // No-op by design — `apply_color_table_changes` already
                // reloads the field, and that reload is what repaints.
                StoreResponse::StyleOverridesApplied => {}
            }
        }
    }

    /// Drain worker responses even while the window is closed — keeps the
    /// store browser, LUT, and sounding flows alive for map interactions.
    pub fn pump(&mut self) {
        self.handle_responses();
    }

    /// One-shot map request (the app installs it as a radar-map layer).
    pub fn take_map_request(&mut self) -> Option<std::sync::Arc<rw_ui::FieldData>> {
        self.map_request.take()
    }

    /// The most recently loaded field (for layer auto-refresh).
    pub fn latest_field(&self) -> Option<&std::sync::Arc<rw_ui::FieldData>> {
        self.latest_field.as_ref()
    }

    /// Selected model hour in the store browser.
    pub fn selected_hour(&self) -> Option<&rw_ui::HourKey> {
        self.browser.selected()
    }

    /// Select an exact store hour, requesting its variable list if it is a
    /// real change. Returns true when a new hour was requested.
    pub fn select_hour_key(&mut self, key: HourKey) -> bool {
        if self.browser.selected() == Some(&key) {
            return false;
        }
        self.browser.select(key.clone());
        self.select_hour(key);
        true
    }

    /// The most recent sounding (for the native skew-T window).
    pub fn latest_sounding(&self) -> Option<&std::sync::Arc<rw_ui::SoundingData>> {
        self.latest_sounding.as_ref()
    }

    /// Whether the reusable rw-ui sounding panel has model sounding content
    /// ready for an external host pane/window.
    pub fn sounding_has_content(&self) -> bool {
        self.sounding.has_content()
    }

    /// Render the reusable rw-ui sounding panel outside the Model Data dock.
    pub fn sounding_ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();
        self.sounding.ui(ui);
    }

    pub fn sounding_view_state_json(&self) -> serde_json::Value {
        self.sounding.view_state_json()
    }

    pub fn apply_sounding_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        self.sounding.apply_view_state_json(value)
    }

    /// Serialize the current model field-plot color-table overrides for
    /// persistence (opaque JSON; kept in app settings like the sounding state).
    pub fn style_overrides_json(&self) -> serde_json::Value {
        serde_json::to_value(self.color_tables.settings()).unwrap_or(serde_json::Value::Null)
    }

    /// Restore persisted color-table overrides: load them into the editor and
    /// push them to the store worker so field palettes resolve through them.
    /// Returns false on malformed JSON (older/newer schema) — left at defaults.
    pub fn apply_style_overrides_json(&mut self, value: &serde_json::Value) -> bool {
        match serde_json::from_value::<StyleOverrideSettings>(value.clone()) {
            Ok(settings) => {
                self.color_tables.set_settings(settings.clone());
                self.worker
                    .send(StoreRequest::SetStyleOverrides(settings.normalized()));
                true
            }
            Err(_) => false,
        }
    }

    /// Newest (model, run, hour-count) in the store tree — freshness display.
    pub fn newest_run(&self) -> Option<(String, String, usize)> {
        let tree = self.tree.as_ref()?;
        let model = tree.models.first()?;
        let run = model.runs.last()?;
        Some((model.model.clone(), run.run.clone(), run.hours.len()))
    }

    /// Re-scan the store (after an ingest finishes).
    pub fn rescan(&mut self) {
        self.worker.send(StoreRequest::Enumerate);
    }

    /// Step the selected forecast hour within the current run; the viewer
    /// re-requests its current variable automatically when the hour lands.
    pub fn step_hour(&mut self, delta: i64) {
        let Some(current) = self.browser.selected().cloned() else {
            return;
        };
        let Some(tree) = &self.tree else {
            return;
        };
        let hours: Vec<u16> = tree
            .models
            .iter()
            .find(|m| m.model == current.model)
            .and_then(|m| m.runs.iter().find(|r| r.run == current.run))
            .map(|r| r.hours.iter().map(|h| h.hour).collect())
            .unwrap_or_default();
        let Some(position) = hours.iter().position(|&h| h == current.hour) else {
            return;
        };
        let next = position as i64 + delta;
        if next < 0 || next as usize >= hours.len() {
            return;
        }
        let key = HourKey {
            model: current.model,
            run: current.run,
            hour: hours[next as usize],
        };
        self.browser.select(key.clone());
        self.select_hour(key);
    }

    /// Model slug of the hour selected in the store browser — what
    /// `request_sounding_at` would sample. Callers holding grid coords
    /// from a specific model's LUT use this to detect cross-model
    /// mismatches in mixed hrrr+gfs stores.
    pub fn browsed_hour_model(&self) -> Option<String> {
        self.viewer.hour().map(|hour| hour.model.clone())
    }

    /// Request a sounding at storage-order grid coordinates (map click).
    pub fn request_sounding_at(&mut self, fx: f64, fy: f64) {
        if let Some(hour) = self.viewer.hour().cloned() {
            self.sounding.set_loading();
            self.worker
                .send(StoreRequest::LoadSounding { hour, fx, fy });
        }
    }

    /// Request a sounding from an EXPLICIT run/hour (independent of the
    /// browser selection) — used by callers that must not be stale.
    pub fn request_sounding_for(&mut self, hour: HourKey, fx: f64, fy: f64) {
        self.sounding.set_loading();
        self.worker
            .send(StoreRequest::LoadSounding { hour, fx, fy });
    }

    /// The hour key in the NEWEST run COVERING `target` whose valid time
    /// is closest to it — run slugs parse as "YYYYMMDD_HHz", valid =
    /// run + fhr. Era guard: a run is only eligible when `target` falls
    /// inside its plausible forecast coverage (init <= target <= init +
    /// the model's max forecast horizon), so a mixed archive+live store
    /// never pins a 2013 event time to today's run — or a live time to
    /// an archived event's run. Returns None when no run covers `target`.
    /// Returns (key, valid time, run age at `target`).
    ///
    /// `preferred_model` pins the lookup to one model's runs (callers
    /// holding grid coordinates from a specific model's LUT must not mix
    /// grids in an hrrr+gfs store); `None` keeps the historical
    /// first-model behavior.
    pub fn newest_hour_valid_near(
        &self,
        target: chrono::DateTime<chrono::Utc>,
        preferred_model: Option<&str>,
    ) -> Option<(HourKey, chrono::DateTime<chrono::Utc>, chrono::Duration)> {
        let tree = self.tree.as_ref()?;
        let model = match preferred_model {
            Some(slug) => tree.models.iter().find(|entry| entry.model == slug)?,
            None => tree.models.first()?,
        };
        // Runs are sorted newest first (StoreTree contract), so the first
        // run covering `target` is the newest eligible one.
        let (run, run_time) = model.runs.iter().find_map(|run| {
            let run_time = model_run_time_utc(&run.run)?;
            let horizon = chrono::Duration::hours(model_max_forecast_horizon_hours(
                &model.model,
                chrono::Timelike::hour(&run_time) as u8,
            ));
            (run_time <= target && target <= run_time + horizon).then_some((run, run_time))
        })?;
        let best = run.hours.iter().min_by_key(|hour| {
            (run_time + chrono::Duration::hours(hour.hour as i64) - target)
                .num_seconds()
                .abs()
        })?;
        let valid = run_time + chrono::Duration::hours(best.hour as i64);
        Some((
            HourKey {
                model: model.model.clone(),
                run: run.run.clone(),
                hour: best.hour,
            },
            valid,
            target - run_time,
        ))
    }

    /// Select the newest-run forecast hour valid closest to `target`.
    /// Existing map layers auto-refresh when their variable lands, so this
    /// is the bridge used by BowEcho's unified timeline player.
    pub fn select_newest_hour_valid_near(
        &mut self,
        target: chrono::DateTime<chrono::Utc>,
        preferred_model: Option<&str>,
    ) -> Option<(
        HourKey,
        chrono::DateTime<chrono::Utc>,
        chrono::Duration,
        bool,
    )> {
        let (key, valid, run_age) = self.newest_hour_valid_near(target, preferred_model)?;
        let changed = self.select_hour_key(key.clone());
        Some((key, valid, run_age, changed))
    }

    /// The dock body — call inside an egui Window/panel. Returns false when
    /// the user asked to close.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.handle_responses();

        egui::Panel::left("model_runs")
            .resizable(true)
            .default_size(230.0)
            .show_inside(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.strong("Runs");
                    if ui.button("⟳").on_hover_text("Re-scan the store").clicked() {
                        self.worker.send(StoreRequest::Enumerate);
                    }
                });
                ui.label(
                    egui::RichText::new(self.store_root.display().to_string())
                        .small()
                        .weak(),
                );
                ui.separator();
                let mut picked = None;
                match &self.tree {
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("scanning store…");
                        });
                    }
                    Some(tree) if tree.models.is_empty() => {
                        ui.label(format!(
                            "No model runs under\n{}",
                            self.store_root.display()
                        ));
                        ui.label(
                            egui::RichText::new(
                                "Run rusty-weather ingest, or point the store path at an rw-store directory.",
                            )
                            .small()
                            .weak(),
                        );
                    }
                    Some(tree) => {
                        let browser = &mut self.browser;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            picked = browser.ui(ui, tree);
                        });
                    }
                }
                if let Some(key) = picked {
                    self.select_hour(key);
                }
            });

        if self.sounding.has_content() {
            egui::Panel::right("model_sounding")
                .resizable(true)
                .default_size(520.0)
                .show_inside(ui, |ui| {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.strong("Sounding");
                        if ui.button("✕").on_hover_text("Close sounding").clicked() {
                            self.sounding.clear();
                        }
                    });
                    ui.separator();
                    self.sounding.ui(ui);
                });
        }

        egui::CentralPanel::default().show_inside(ui, |ui| {
            if self.latest_field.is_some() {
                ui.horizontal(|ui| {
                    if ui
                        .button("Show on radar map")
                        .on_hover_text(
                            "Render this field as a layer under the radar (opacity in Layers)",
                        )
                        .clicked()
                    {
                        self.map_request = self.latest_field.clone();
                    }
                    ui.toggle_value(&mut self.show_plot_viewer, "🗺 Native plot")
                        .on_hover_text(
                            "Render the selected field through rusty-weather's native plot \
                             pipeline. Shift-drag a box on the field viewer to plot a custom \
                             domain; drag a selection corner to rotate.",
                        );
                    ui.toggle_value(&mut self.show_color_tables, "🎨 Color tables")
                        .on_hover_text(
                            "Edit model field-plot color tables: bind a product to a palette, \
                             edit its levels and colors; the field reloads with your palette.",
                        );
                });
            }
            match self.viewer.ui(ui) {
                Some(FieldViewerEvent::VarSelected(var)) => {
                    self.viewer.set_loading(&var);
                    if let Some(field) = self.viewer.wanted_field() {
                        self.worker.send(StoreRequest::LoadField(field));
                    }
                }
                Some(FieldViewerEvent::PointClicked { fx, fy }) => {
                    if let Some(hour) = self.viewer.hour().cloned() {
                        self.sounding.set_loading();
                        self.worker
                            .send(StoreRequest::LoadSounding { hour, fx, fy });
                    }
                }
                // v0.2.3 custom-domain plot: shift-drag a box on the field
                // viewer to select an arbitrary plot domain, or drag a corner
                // to rotate it. Open the native plot viewer and retarget it.
                Some(FieldViewerEvent::DomainSelected(domain)) => {
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain(domain);
                }
                Some(FieldViewerEvent::DomainRotationChanged { rotation_deg }) => {
                    self.show_plot_viewer = true;
                    self.plot_viewer.set_active_domain_rotation(rotation_deg);
                }
                None => {}
            }
        });

        // v0.2.3 custom-domain native plot, as a floating window. Rendered
        // after the field viewer so a domain selected this frame shows at once.
        // `current_field()` borrows `self.viewer` immutably while the closure
        // holds `&mut self.plot_viewer` — disjoint fields, so this is sound.
        if self.show_plot_viewer {
            let field = self.viewer.current_field();
            let mut open = true;
            egui::Window::new("🗺 Native plot")
                .open(&mut open)
                .default_size([560.0, 440.0])
                .show(ui.ctx(), |ui| {
                    self.plot_viewer.ui(ui, field);
                });
            if !open {
                self.show_plot_viewer = false;
            }
        }

        // v0.2.3 editable color tables. `current_field()` borrows `self.viewer`
        // for the panel, so it is scoped and dropped BEFORE apply — which
        // reloads the field and thus needs `self.viewer` mutably.
        if self.show_color_tables {
            let mut open = true;
            let mut changed = false;
            {
                let field = self.viewer.current_field();
                egui::Window::new("🎨 Color tables")
                    .open(&mut open)
                    .default_size([520.0, 520.0])
                    .show(ui.ctx(), |ui| {
                        self.color_tables.ui(ui, field);
                        changed = self.color_tables.take_changed();
                    });
            }
            if changed {
                self.apply_color_table_changes();
            }
            if !open {
                self.show_color_tables = false;
            }
        }
    }
}

/// Max forecast horizon (hours past init) a stored run can plausibly
/// cover — the last supported forecast hour from the model's ingest spec
/// (`rustwx_models::supported_forecast_hours`). Unknown store slugs fall
/// back to the longest built-in horizon (GFS/GEFS, 384 h) so the era
/// guard still separates archive runs from live ones.
fn model_max_forecast_horizon_hours(model: &str, cycle_hour_utc: u8) -> i64 {
    model
        .parse::<rustwx_core::ModelId>()
        .ok()
        .and_then(|id| {
            rustwx_models::supported_forecast_hours(id, cycle_hour_utc)
                .last()
                .copied()
        })
        .map_or(384, i64::from)
}

fn model_run_time_utc(run: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (date, cycle) = run.split_once('_')?;
    let naive = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    let cycle_hour: u32 = cycle.trim_end_matches('z').parse().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        naive.and_hms_opt(cycle_hour, 0, 0)?,
        chrono::Utc,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn model_run_time_parses_operational_run_slug() {
        assert_eq!(
            model_run_time_utc("20260618_03z"),
            Some(chrono::Utc.with_ymd_and_hms(2026, 6, 18, 3, 0, 0).unwrap())
        );
        assert_eq!(model_run_time_utc("bad-run"), None);
    }

    #[test]
    fn model_max_forecast_horizon_follows_the_ingest_spec() {
        assert_eq!(model_max_forecast_horizon_hours("hrrr", 0), 48);
        assert_eq!(model_max_forecast_horizon_hours("hrrr", 17), 18);
        assert_eq!(model_max_forecast_horizon_hours("gfs", 12), 384);
        // Unknown store slugs keep the era guard working via the fallback.
        assert_eq!(model_max_forecast_horizon_hours("mystery-model", 0), 384);
    }

    /// StoreTree contract: runs sorted descending (newest first),
    /// hours ascending — mirrors rw-ui's StoreView::enumerate.
    fn tree_with_runs(model: &str, runs: &[(&str, &[u16])]) -> StoreTree {
        StoreTree {
            models: vec![rw_ui::ModelEntry {
                model: model.to_owned(),
                runs: runs
                    .iter()
                    .map(|(run, hours)| rw_ui::RunEntry {
                        run: (*run).to_owned(),
                        build: "test".to_owned(),
                        writer_version: "test".to_owned(),
                        nx: 2,
                        ny: 2,
                        hours: hours
                            .iter()
                            .map(|&hour| rw_ui::HourEntry {
                                hour,
                                file: format!("f{hour:03}.rws"),
                                variable_count: 1,
                                written_unix: 0,
                            })
                            .collect(),
                    })
                    .collect(),
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn era_guard_picks_the_run_covering_the_target_time() {
        let ctx = egui::Context::default();
        // Mixed store: today's live run alongside an archived event's run.
        let dock = ModelDataDock::new_for_test(
            &ctx,
            tree_with_runs(
                "hrrr",
                &[("20260618_00z", &[0, 1, 2]), ("20130520_18z", &[0, 1, 2])],
            ),
        );

        // Archive workflow: a 2013 event time must land in the 2013 run.
        let event = chrono::Utc.with_ymd_and_hms(2013, 5, 20, 20, 5, 0).unwrap();
        let (key, valid, run_age) = dock
            .newest_hour_valid_near(event, Some("hrrr"))
            .expect("2013 run covers the event time");
        assert_eq!(key.run, "20130520_18z");
        assert_eq!(key.hour, 2);
        assert_eq!(
            valid,
            chrono::Utc.with_ymd_and_hms(2013, 5, 20, 20, 0, 0).unwrap()
        );
        assert_eq!(run_age, chrono::Duration::minutes(125));

        // Live workflow: a current target must land in the live run, not
        // the archived one.
        let live = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 1, 40, 0).unwrap();
        let (key, _, _) = dock
            .newest_hour_valid_near(live, Some("hrrr"))
            .expect("live run covers the live time");
        assert_eq!(key.run, "20260618_00z");
        assert_eq!(key.hour, 2);

        // A between-eras target is covered by neither run: never silently
        // pin a run whose forecast horizon can't reach the target.
        let uncovered = chrono::Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(dock.newest_hour_valid_near(uncovered, Some("hrrr")), None);
    }

    #[test]
    fn era_guard_prefers_the_newest_covering_run() {
        let ctx = egui::Context::default();
        let dock = ModelDataDock::new_for_test(
            &ctx,
            tree_with_runs(
                "hrrr",
                &[
                    ("20260618_00z", &[0, 1]),
                    ("20260617_18z", &[0, 1, 2, 3, 4, 5, 6, 7]),
                ],
            ),
        );
        // Both runs cover 00:40z on the 18th; the newest one wins.
        let target = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 0, 40, 0).unwrap();
        let (key, _, _) = dock
            .newest_hour_valid_near(target, Some("hrrr"))
            .expect("both runs cover the target");
        assert_eq!(key.run, "20260618_00z");
        assert_eq!(key.hour, 1);
    }
}
