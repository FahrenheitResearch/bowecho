use chrono::{
    DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday,
};
use eframe::egui;

const DEFAULT_LOOP_HOURS: i64 = 4;
const DEFAULT_TIMELINE_STEP_MINUTES: u16 = 5;
const DEFAULT_MAX_RADAR_FRAMES: u16 = 96;

#[derive(Clone, Debug)]
pub(crate) struct EventLoopBuilderState {
    pub(crate) open: bool,
    seeded: bool,
    start_date: String,
    start_time: String,
    end_date: String,
    end_time: String,
    time_zone: LoopTimeZone,
    site_id: String,
    site_label: String,
    include_radar: bool,
    include_warnings: bool,
    include_models: bool,
    center_on_site: bool,
    auto_play: bool,
    step_minutes: u16,
    max_radar_frames: u16,
    status: String,
    preview: Option<EventLoopPreview>,
}

impl Default for EventLoopBuilderState {
    fn default() -> Self {
        let mut state = Self {
            open: false,
            seeded: false,
            start_date: String::new(),
            start_time: String::new(),
            end_date: String::new(),
            end_time: String::new(),
            time_zone: LoopTimeZone::Utc,
            site_id: String::new(),
            site_label: String::new(),
            include_radar: true,
            include_warnings: true,
            include_models: false,
            center_on_site: false,
            auto_play: true,
            step_minutes: DEFAULT_TIMELINE_STEP_MINUTES,
            max_radar_frames: DEFAULT_MAX_RADAR_FRAMES,
            status: "Ready".to_owned(),
            preview: None,
        };
        state.set_range_ending(Utc::now());
        state
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EventLoopBuilderContext {
    pub(crate) current_site_id: String,
    pub(crate) current_site_label: String,
    pub(crate) display_time_zone_slug: String,
    pub(crate) loaded_radar_frames: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum EventLoopBuilderAction {
    BuildRadar(EventLoopRadarPlan),
}

#[derive(Clone, Debug)]
pub(crate) struct EventLoopRadarPlan {
    pub(crate) site_id: String,
    pub(crate) start_utc: DateTime<Utc>,
    pub(crate) end_utc: DateTime<Utc>,
    pub(crate) max_frames: usize,
    pub(crate) auto_play: bool,
    pub(crate) center_on_site: bool,
    pub(crate) include_warnings: bool,
    pub(crate) include_models: bool,
    pub(crate) timeline_step_minutes: u16,
}

#[derive(Clone, Debug)]
struct EventLoopPreview {
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    ticks: usize,
    radar_estimate: usize,
    layers: Vec<EventLoopLayerSummary>,
}

#[derive(Clone, Debug)]
struct EventLoopLayerSummary {
    label: &'static str,
    detail: String,
    enabled: bool,
}

impl EventLoopBuilderState {
    pub(crate) fn mark_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
    }

    pub(crate) fn show_window(
        &mut self,
        ctx: &egui::Context,
        context: EventLoopBuilderContext,
    ) -> Option<EventLoopBuilderAction> {
        if !self.open {
            return None;
        }
        self.seed_from_context(&context);
        let mut open = self.open;
        let mut action = None;
        egui::Window::new("Event Loop Builder")
            .id(egui::Id::new("bowecho_event_loop_builder"))
            .open(&mut open)
            .default_width(620.0)
            .default_height(620.0)
            .resizable(true)
            .show(ctx, |ui| {
                action = self.ui(ui, &context);
            });
        self.open = open;
        action
    }

    fn seed_from_context(&mut self, context: &EventLoopBuilderContext) {
        if self.seeded {
            return;
        }
        self.time_zone = LoopTimeZone::from_slug(&context.display_time_zone_slug);
        self.site_id = context.current_site_id.clone();
        self.site_label = context.current_site_label.clone();
        self.set_range_ending(Utc::now());
        self.seeded = true;
    }

    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        context: &EventLoopBuilderContext,
    ) -> Option<EventLoopBuilderAction> {
        let mut action = None;
        ui.vertical(|ui| {
            ui.heading("Synchronized Event Loop");
            ui.add_space(4.0);

            self.time_range_ui(ui);
            ui.separator();
            self.target_ui(ui, context);
            ui.separator();
            self.layer_ui(ui);
            ui.separator();
            self.timeline_ui(ui, context);
            ui.separator();

            ui.horizontal(|ui| {
                if ui.button("Preview").clicked() {
                    self.refresh_preview();
                }
                let build_enabled = self.include_radar;
                if ui
                    .add_enabled(build_enabled, egui::Button::new("Build Radar Loop"))
                    .clicked()
                {
                    match self.radar_plan() {
                        Ok(plan) => {
                            self.status = format!(
                                "Queued {} {} to {}",
                                plan.site_id,
                                plan.start_utc.format("%H:%MZ"),
                                plan.end_utc.format("%H:%MZ")
                            );
                            action = Some(EventLoopBuilderAction::BuildRadar(plan));
                        }
                        Err(err) => {
                            self.status = err;
                        }
                    }
                }
                if ui.button("Now -4h").clicked() {
                    self.set_range_ending(Utc::now());
                    self.refresh_preview();
                }
                if ui.button("Last Night 5-9p").clicked() {
                    self.set_last_night_evening();
                    self.refresh_preview();
                }
            });

            if !self.status.is_empty() {
                ui.label(egui::RichText::new(&self.status).strong());
            }
            if let Some(preview) = &self.preview {
                preview_ui(ui, preview, self.time_zone);
            }
        });
        action
    }

    fn time_range_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Time Range").strong());
        egui::Grid::new("event_loop_time_grid")
            .num_columns(4)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Start");
                ui.add_sized(
                    [104.0, 22.0],
                    egui::TextEdit::singleline(&mut self.start_date),
                );
                ui.add_sized(
                    [64.0, 22.0],
                    egui::TextEdit::singleline(&mut self.start_time),
                );
                ui.label(self.time_zone.abbreviation_for_inputs());
                ui.end_row();

                ui.label("End");
                ui.add_sized(
                    [104.0, 22.0],
                    egui::TextEdit::singleline(&mut self.end_date),
                );
                ui.add_sized([64.0, 22.0], egui::TextEdit::singleline(&mut self.end_time));
                egui::ComboBox::from_id_salt("event_loop_time_zone")
                    .selected_text(self.time_zone.label())
                    .show_ui(ui, |ui| {
                        for zone in LoopTimeZone::ALL {
                            ui.selectable_value(&mut self.time_zone, zone, zone.label());
                        }
                    });
                ui.end_row();
            });
    }

    fn target_ui(&mut self, ui: &mut egui::Ui, context: &EventLoopBuilderContext) {
        ui.label(egui::RichText::new("Area + Radar").strong());
        ui.horizontal(|ui| {
            ui.label("Site");
            ui.add_sized([78.0, 22.0], egui::TextEdit::singleline(&mut self.site_id));
            if ui.button("Use Selected").clicked() {
                self.site_id = context.current_site_id.clone();
                self.site_label = context.current_site_label.clone();
            }
            ui.checkbox(&mut self.center_on_site, "Center");
        });
        if !self.site_label.is_empty() {
            ui.weak(&self.site_label);
        }
    }

    fn layer_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Layers").strong());
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.include_radar, "Radar");
            ui.checkbox(&mut self.include_warnings, "Warnings");
            ui.checkbox(&mut self.include_models, "Models");
        });
    }

    fn timeline_ui(&mut self, ui: &mut egui::Ui, context: &EventLoopBuilderContext) {
        ui.label(egui::RichText::new("Timeline").strong());
        ui.horizontal(|ui| {
            ui.label("Step");
            ui.add(
                egui::DragValue::new(&mut self.step_minutes)
                    .range(1..=30)
                    .suffix(" min"),
            );
            ui.label("Radar cap");
            ui.add(
                egui::DragValue::new(&mut self.max_radar_frames)
                    .range(1..=2000)
                    .suffix(" scans"),
            );
            ui.checkbox(&mut self.auto_play, "Play");
        });
        ui.weak(format!(
            "Current radar history: {} frame(s)",
            context.loaded_radar_frames
        ));
    }

    fn refresh_preview(&mut self) {
        match self.preview_from_inputs() {
            Ok(preview) => {
                self.status = "Preview ready".to_owned();
                self.preview = Some(preview);
            }
            Err(err) => {
                self.status = err;
                self.preview = None;
            }
        }
    }

    fn radar_plan(&mut self) -> Result<EventLoopRadarPlan, String> {
        let preview = self.preview_from_inputs()?;
        self.preview = Some(preview.clone());
        let site_id = self.site_id.trim().to_ascii_uppercase();
        if site_id.is_empty() {
            return Err("Choose a radar site first".to_owned());
        }
        Ok(EventLoopRadarPlan {
            site_id,
            start_utc: preview.start_utc,
            end_utc: preview.end_utc,
            max_frames: usize::from(self.max_radar_frames),
            auto_play: self.auto_play,
            center_on_site: self.center_on_site,
            include_warnings: self.include_warnings,
            include_models: self.include_models,
            timeline_step_minutes: self.step_minutes,
        })
    }

    fn preview_from_inputs(&self) -> Result<EventLoopPreview, String> {
        let start_utc = self.parse_endpoint(&self.start_date, &self.start_time)?;
        let end_utc = self.parse_endpoint(&self.end_date, &self.end_time)?;
        if end_utc <= start_utc {
            return Err("End time must be after start time".to_owned());
        }
        let duration_minutes = (end_utc - start_utc).num_minutes().max(1);
        let step = i64::from(self.step_minutes.max(1));
        let ticks = (duration_minutes / step + 1).max(1) as usize;
        let radar_estimate = ((duration_minutes / 5) + 1)
            .max(1)
            .min(i64::from(self.max_radar_frames)) as usize;
        let mut layers = Vec::new();
        layers.push(EventLoopLayerSummary {
            label: "Radar",
            detail: if self.include_radar {
                format!(
                    "{} scan(s) estimated, capped at {}",
                    radar_estimate, self.max_radar_frames
                )
            } else {
                "disabled".to_owned()
            },
            enabled: self.include_radar,
        });
        layers.push(EventLoopLayerSummary {
            label: "Warnings",
            detail: if self.include_warnings {
                "valid-time overlay enabled".to_owned()
            } else {
                "disabled".to_owned()
            },
            enabled: self.include_warnings,
        });
        layers.push(EventLoopLayerSummary {
            label: "Models",
            detail: if self.include_models {
                "model context requested".to_owned()
            } else {
                "disabled".to_owned()
            },
            enabled: self.include_models,
        });
        Ok(EventLoopPreview {
            start_utc,
            end_utc,
            ticks,
            radar_estimate,
            layers,
        })
    }

    fn parse_endpoint(&self, date: &str, time: &str) -> Result<DateTime<Utc>, String> {
        let date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
            .map_err(|_| "Dates must be YYYY-MM-DD".to_owned())?;
        let time = parse_time_input(time.trim())?;
        Ok(self.time_zone.local_to_utc(NaiveDateTime::new(date, time)))
    }

    fn set_range_ending(&mut self, end_utc: DateTime<Utc>) {
        let start_utc = end_utc - Duration::hours(DEFAULT_LOOP_HOURS);
        self.set_range_utc(start_utc, end_utc);
    }

    fn set_last_night_evening(&mut self) {
        let now_local = self.time_zone.utc_to_local(Utc::now());
        let yesterday = now_local.date() - Duration::days(1);
        let start = NaiveDateTime::new(yesterday, NaiveTime::from_hms_opt(17, 0, 0).unwrap());
        let end = NaiveDateTime::new(yesterday, NaiveTime::from_hms_opt(21, 0, 0).unwrap());
        self.set_range_utc(
            self.time_zone.local_to_utc(start),
            self.time_zone.local_to_utc(end),
        );
    }

    fn set_range_utc(&mut self, start_utc: DateTime<Utc>, end_utc: DateTime<Utc>) {
        let start = self.time_zone.utc_to_local(start_utc);
        let end = self.time_zone.utc_to_local(end_utc);
        self.start_date = start.format("%Y-%m-%d").to_string();
        self.start_time = start.format("%H:%M").to_string();
        self.end_date = end.format("%Y-%m-%d").to_string();
        self.end_time = end.format("%H:%M").to_string();
    }
}

fn preview_ui(ui: &mut egui::Ui, preview: &EventLoopPreview, zone: LoopTimeZone) {
    ui.add_space(4.0);
    ui.label(egui::RichText::new("Preview").strong());
    ui.label(format!(
        "{} to {}  |  {} ticks  |  ~{} radar scans",
        zone.format_date_hm(preview.start_utc),
        zone.format_date_hm(preview.end_utc),
        preview.ticks,
        preview.radar_estimate
    ));
    ui.weak(format!(
        "UTC {} to {}",
        preview.start_utc.format("%Y-%m-%d %H:%MZ"),
        preview.end_utc.format("%Y-%m-%d %H:%MZ")
    ));
    for layer in &preview.layers {
        let marker = if layer.enabled { "on" } else { "off" };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(layer.label).strong());
            ui.weak(marker);
            ui.label(&layer.detail);
        });
    }
}

fn parse_time_input(input: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(input, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(input, "%H:%M"))
        .map_err(|_| "Times must be HH:MM or HH:MM:SS".to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopTimeZone {
    Utc,
    Eastern,
    Central,
    Mountain,
    Pacific,
}

impl LoopTimeZone {
    const ALL: [Self; 5] = [
        Self::Utc,
        Self::Eastern,
        Self::Central,
        Self::Mountain,
        Self::Pacific,
    ];

    fn from_slug(slug: &str) -> Self {
        match slug.trim().to_ascii_lowercase().as_str() {
            "eastern" | "et" | "est" | "edt" => Self::Eastern,
            "central" | "ct" | "cst" | "cdt" => Self::Central,
            "mountain" | "mt" | "mst" | "mdt" => Self::Mountain,
            "pacific" | "pt" | "pst" | "pdt" => Self::Pacific,
            _ => Self::Utc,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Utc => "UTC",
            Self::Eastern => "Eastern",
            Self::Central => "Central",
            Self::Mountain => "Mountain",
            Self::Pacific => "Pacific",
        }
    }

    fn standard_offset_hours(self) -> i32 {
        match self {
            Self::Utc => 0,
            Self::Eastern => -5,
            Self::Central => -6,
            Self::Mountain => -7,
            Self::Pacific => -8,
        }
    }

    fn daylight_offset_hours(self) -> i32 {
        self.standard_offset_hours() + 1
    }

    fn transition_start_utc_hour(self) -> u32 {
        match self {
            Self::Utc => 0,
            Self::Eastern => 7,
            Self::Central => 8,
            Self::Mountain => 9,
            Self::Pacific => 10,
        }
    }

    fn transition_end_utc_hour(self) -> u32 {
        match self {
            Self::Utc => 0,
            Self::Eastern => 6,
            Self::Central => 7,
            Self::Mountain => 8,
            Self::Pacific => 9,
        }
    }

    fn is_dst_at(self, time_utc: DateTime<Utc>) -> bool {
        if self == Self::Utc {
            return false;
        }
        let year = time_utc.year();
        let start_day = nth_weekday_of_month(year, 3, Weekday::Sun, 2);
        let end_day = nth_weekday_of_month(year, 11, Weekday::Sun, 1);
        let start = Utc
            .with_ymd_and_hms(year, 3, start_day, self.transition_start_utc_hour(), 0, 0)
            .unwrap();
        let end = Utc
            .with_ymd_and_hms(year, 11, end_day, self.transition_end_utc_hour(), 0, 0)
            .unwrap();
        time_utc >= start && time_utc < end
    }

    fn fixed_offset_hours_at(self, time_utc: DateTime<Utc>) -> i32 {
        if self.is_dst_at(time_utc) {
            self.daylight_offset_hours()
        } else {
            self.standard_offset_hours()
        }
    }

    fn abbreviation_at(self, time_utc: DateTime<Utc>) -> &'static str {
        match (self, self.is_dst_at(time_utc)) {
            (Self::Utc, _) => "UTC",
            (Self::Eastern, false) => "EST",
            (Self::Eastern, true) => "EDT",
            (Self::Central, false) => "CST",
            (Self::Central, true) => "CDT",
            (Self::Mountain, false) => "MST",
            (Self::Mountain, true) => "MDT",
            (Self::Pacific, false) => "PST",
            (Self::Pacific, true) => "PDT",
        }
    }

    fn abbreviation_for_inputs(self) -> &'static str {
        self.abbreviation_at(Utc::now())
    }

    fn utc_to_local(self, time_utc: DateTime<Utc>) -> NaiveDateTime {
        if self == Self::Utc {
            return time_utc.naive_utc();
        }
        time_utc
            .checked_add_signed(Duration::hours(i64::from(
                self.fixed_offset_hours_at(time_utc),
            )))
            .unwrap_or(time_utc)
            .naive_utc()
    }

    fn local_to_utc(self, local: NaiveDateTime) -> DateTime<Utc> {
        if self == Self::Utc {
            return Utc.from_utc_datetime(&local);
        }
        let daylight = utc_from_local_and_offset(local, self.daylight_offset_hours());
        if self.is_dst_at(daylight) {
            daylight
        } else {
            utc_from_local_and_offset(local, self.standard_offset_hours())
        }
    }

    fn format_date_hm(self, time_utc: DateTime<Utc>) -> String {
        if self == Self::Utc {
            return time_utc.format("%Y-%m-%d %H:%MZ").to_string();
        }
        let local = self.utc_to_local(time_utc);
        format!(
            "{} {}",
            local.format("%Y-%m-%d %H:%M"),
            self.abbreviation_at(time_utc)
        )
    }
}

fn utc_from_local_and_offset(local: NaiveDateTime, offset_hours: i32) -> DateTime<Utc> {
    Utc.from_utc_datetime(&(local - Duration::hours(i64::from(offset_hours))))
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: Weekday, nth: u32) -> u32 {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid DST transition month");
    let first_weekday = first.weekday().num_days_from_sunday() as i32;
    let target = weekday.num_days_from_sunday() as i32;
    let first_match = 1 + (target - first_weekday).rem_euclid(7) as u32;
    first_match + (nth.saturating_sub(1) * 7)
}
