//! Model data dock — rusty-weather's rw-ui panels mounted inside BowEcho.
//!
//! The panels (run browser, false-color field viewer, skew-T sounding) were
//! built to take a `&mut egui::Ui` from any egui host; all store IO runs on
//! rw-ui's own worker thread, so BowEcho's render loop never blocks. The
//! data source is an rw-store directory on disk (produced by rusty-weather
//! ingest, default `C:\Users\drew\rusty-weather\store`).

use eframe::egui;
use rw_ui::{
    FieldViewerEvent, FieldViewerPanel, HourKey, RunBrowserPanel, SoundingPanel, StoreRequest,
    StoreResponse, StoreTree, StoreView, StoreWorker,
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
        }
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

    /// The most recent sounding (for the native skew-T window).
    pub fn latest_sounding(&self) -> Option<&std::sync::Arc<rw_ui::SoundingData>> {
        self.latest_sounding.as_ref()
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

    /// The hour key in the NEWEST run whose valid time is closest to
    /// `target` — run slugs parse as "YYYYMMDD_HHz", valid = run + fhr.
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
        let run = model.runs.last()?;
        let (date, cycle) = run.run.split_once('_')?;
        let naive = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
        let cycle_hour: u32 = cycle.trim_end_matches('z').parse().ok()?;
        let run_time = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            naive.and_hms_opt(cycle_hour, 0, 0)?,
            chrono::Utc,
        );
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
                None => {}
            }
        });
    }
}
