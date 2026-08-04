//! Forecast-time diagnostics for one model field.
//!
//! Point history deliberately reads only the 2 x 2 window needed by the
//! map's curvilinear interpolation stencil. Domain extrema come from
//! rw-store's indexed per-tile statistics and do not decompress field data.
//! Both paths run on a dedicated worker so opening the chart cannot block an
//! egui frame.

use eframe::egui;
use rustwx_products::viewer::UnitConvert;
use rw_ui::{FieldData, HourKey, StoreView};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crate::model_layer::{FieldSampleStencil, InverseLut, sample_stencils_for_point};

/// A normal operational run is far shorter; this bound matters for local
/// exact-time stores carrying thousands of minute-cadence outputs. Long runs
/// use a contiguous window centered on the selected forecast time and label
/// that truncation in the UI.
const MAX_POINT_HISTORY_TIMES: usize = 256;
// Domain extrema come from each hour's cached statistics and therefore avoid
// the point probe's repeated cell-window reads. Keep enough exact timesteps for
// a full multi-hour, minute-cadence convection simulation in one graph.
const MAX_DOMAIN_HISTORY_TIMES: usize = 4_096;

#[derive(Clone)]
pub(super) struct PointProbeRequest {
    pub lut: Arc<InverseLut>,
    pub lat: f32,
    pub lon: f32,
}

#[derive(Clone)]
pub(super) struct ProbeHistoryRequest {
    pub store_root: PathBuf,
    pub field: Arc<FieldData>,
    /// Optional fixed geographic sample. Domain extrema deliberately do not
    /// require one: their values come from the whole stored grid at each
    /// forecast time.
    pub point: Option<PointProbeRequest>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HistoryMetric {
    #[default]
    Point,
    DomainMinimum,
    DomainMaximum,
}

impl HistoryMetric {
    const ALL: [Self; 3] = [Self::Point, Self::DomainMinimum, Self::DomainMaximum];

    const fn label(self) -> &'static str {
        match self {
            Self::Point => "Fixed point",
            Self::DomainMinimum => "Domain minimum",
            Self::DomainMaximum => "Domain maximum",
        }
    }

    const fn short_label(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::DomainMinimum => "min",
            Self::DomainMaximum => "max",
        }
    }

    fn value(self, sample: &HistorySample) -> Option<f32> {
        match self {
            Self::Point => sample.point,
            Self::DomainMinimum => sample.domain_min,
            Self::DomainMaximum => sample.domain_max,
        }
        .filter(|value| value.is_finite())
    }
}

#[derive(Clone, Debug)]
struct HistorySample {
    hour: HourKey,
    point: Option<f32>,
    domain_min: Option<f32>,
    domain_max: Option<f32>,
}

#[derive(Clone, Debug)]
struct HistorySeries {
    source_hour: HourKey,
    variable: String,
    units: String,
    point: Option<(f32, f32)>,
    samples: Vec<HistorySample>,
    failed_hours: usize,
    stored_hours_total: usize,
}

enum TaskMessage {
    Progress { done: usize, total: usize },
    Finished(Result<HistorySeries, String>),
}

struct HistoryTask {
    rx: mpsc::Receiver<TaskMessage>,
    cancel: Arc<AtomicBool>,
}

impl HistoryTask {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
pub(super) struct ModelProbeHistoryPanel {
    open: bool,
    metric: HistoryMetric,
    task: Option<HistoryTask>,
    request: Option<ProbeHistoryRequest>,
    progress: Option<(usize, usize)>,
    series: Option<HistorySeries>,
    error: Option<String>,
}

impl Drop for ModelProbeHistoryPanel {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.cancel();
        }
    }
}

impl ModelProbeHistoryPanel {
    pub(super) fn open(
        &mut self,
        request: ProbeHistoryRequest,
        repaint: egui::Context,
    ) -> Result<(), String> {
        validate_request(&request)?;
        self.metric = if request.point.is_some() {
            HistoryMetric::Point
        } else {
            HistoryMetric::DomainMinimum
        };
        self.start(request, repaint)
    }

    fn start(
        &mut self,
        request: ProbeHistoryRequest,
        repaint: egui::Context,
    ) -> Result<(), String> {
        if let Some(task) = &self.task {
            task.cancel();
        }
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker_request = request.clone();
        let _worker = std::thread::Builder::new()
            .name("model-probe-history".to_owned())
            .spawn(move || {
                let result = load_history(&worker_request, &worker_cancel, &tx, &repaint);
                let _ = tx.send(TaskMessage::Finished(result));
                repaint.request_repaint();
            })
            .map_err(|error| format!("Could not start model history worker: {error}"))?;

        self.open = true;
        self.task = Some(HistoryTask { rx, cancel });
        self.request = Some(request);
        self.progress = Some((0, 0));
        self.series = None;
        self.error = None;
        Ok(())
    }

    pub(super) fn show(&mut self, ctx: &egui::Context) -> Option<HourKey> {
        self.poll();
        if !self.open {
            return None;
        }

        if self.task.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let mut open = true;
        let mut reload = false;
        let mut selected_hour = None;
        let point_available = self
            .request
            .as_ref()
            .is_some_and(|request| request.point.is_some());
        let title = if point_available {
            "Model point / domain history"
        } else {
            "Model domain extrema"
        };
        egui::Window::new(title)
            .open(&mut open)
            .default_size([680.0, 390.0])
            .min_size([440.0, 300.0])
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for metric in HistoryMetric::ALL
                        .into_iter()
                        .filter(|metric| *metric != HistoryMetric::Point || point_available)
                    {
                        ui.selectable_value(&mut self.metric, metric, metric.label());
                    }
                    ui.separator();
                    reload = ui
                        .small_button("Refresh")
                        .on_hover_text("Re-read this run from the local model store")
                        .clicked();
                });

                if let Some((done, total)) = self.progress {
                    let fraction = if total == 0 {
                        0.0
                    } else {
                        done as f32 / total as f32
                    };
                    ui.add(egui::ProgressBar::new(fraction).show_percentage().text(
                        if total == 0 {
                            "Reading run inventory".to_owned()
                        } else {
                            format!("Reading {done}/{total} stored times")
                        },
                    ));
                }

                if let Some(error) = &self.error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                if let Some(series) = &self.series {
                    history_header(ui, series, self.metric);
                    ui.add_space(4.0);
                    selected_hour = history_chart(ui, series, self.metric);
                    ui.add_space(4.0);
                    ui.weak(history_io_summary(series));
                    if series.failed_hours > 0 {
                        ui.weak(format!(
                            "{} of {} charted times lacked a readable 2-D '{}' field.",
                            series.failed_hours,
                            series.samples.len(),
                            series.variable
                        ));
                    }
                }
            });

        if reload
            && let Some(request) = self.request.clone()
            && let Err(error) = self.start(request, ctx.clone())
        {
            self.error = Some(error);
        }
        if !open {
            if let Some(task) = &self.task {
                task.cancel();
            }
            self.task = None;
            self.open = false;
        }
        selected_hour
    }

    fn poll(&mut self) {
        let mut finished = None;
        let Some(task) = &self.task else {
            return;
        };
        loop {
            match task.rx.try_recv() {
                Ok(TaskMessage::Progress { done, total }) => self.progress = Some((done, total)),
                Ok(TaskMessage::Finished(result)) => {
                    finished = Some(result);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished = Some(Err("Model history worker stopped unexpectedly".to_owned()));
                    break;
                }
            }
        }
        if let Some(result) = finished {
            self.task = None;
            self.progress = None;
            match result {
                Ok(series) => {
                    self.series = Some(series);
                    self.error = None;
                }
                Err(error) if error == "cancelled" => {}
                Err(error) => {
                    self.series = None;
                    self.error = Some(error);
                }
            }
        }
    }
}

fn validate_request(request: &ProbeHistoryRequest) -> Result<(), String> {
    if request.field.key.var.trim().is_empty() {
        return Err("The selected model field has no variable name".to_owned());
    }
    if let Some(point) = &request.point {
        let grid = request
            .field
            .grid
            .as_ref()
            .ok_or_else(|| "The selected model field has no geographic grid".to_owned())?;
        if grid.nx != request.field.nx || grid.ny != request.field.ny {
            return Err("The selected model field's grid dimensions do not match".to_owned());
        }
        let Some(seed) = point.lut.lookup(point.lat, point.lon) else {
            return Err("The fixed probe is outside the selected model field".to_owned());
        };
        let seed_lat = grid
            .lat
            .get(seed)
            .copied()
            .filter(|value| value.is_finite());
        let seed_lon = grid
            .lon
            .get(seed)
            .copied()
            .filter(|value| value.is_finite());
        let seed_matches_grid = seed_lat.zip(seed_lon).is_some_and(|(seed_lat, seed_lon)| {
            let lon_delta = (seed_lon - point.lon).abs().rem_euclid(360.0);
            (seed_lat - point.lat).abs() <= 2.0 && lon_delta.min(360.0 - lon_delta) <= 2.0
        });
        if !seed_matches_grid {
            return Err("The model probe lookup belongs to a different or stale grid".to_owned());
        }
    }
    Ok(())
}

fn load_history(
    request: &ProbeHistoryRequest,
    cancel: &AtomicBool,
    tx: &mpsc::Sender<TaskMessage>,
    repaint: &egui::Context,
) -> Result<HistorySeries, String> {
    let view = StoreView::new(&request.store_root);
    let tree = view.enumerate();
    let source_hour = request.field.key.hour.clone();
    let run = tree
        .run(&source_hour.model, &source_hour.run)
        .ok_or_else(|| {
            format!(
                "Run {}/{} is no longer present in the local model store",
                source_hour.model, source_hour.run
            )
        })?;
    if run.hours.is_empty() {
        return Err("The selected model run has no stored forecast times".to_owned());
    }

    let variable = request.field.key.var.clone();
    let source_reader = view
        .open_hour(&source_hour.model, &source_hour.run, source_hour.hour)
        .map_err(|error| format!("Could not open the selected model time: {error}"))?;
    if source_reader.meta().exact_time() != source_hour.exact_time {
        return Err("The selected model time no longer matches its store manifest".to_owned());
    }
    let source_variable = source_reader.variable(&variable).ok_or_else(|| {
        format!(
            "'{}' is generated rather than a stored 2-D field; its history needs a dedicated evaluator",
            variable
        )
    })?;
    if source_variable.kind != "surface2d" {
        return Err(format!(
            "'{}' is a {} field; this forecast graph currently supports stored 2-D fields",
            variable, source_variable.kind
        ));
    }
    let stored_units = source_variable.units.clone();
    let source_nx = source_reader.meta().nx;
    let source_ny = source_reader.meta().ny;
    drop(source_reader);

    let point_sampling = request.point.as_ref().map(|point| {
        let grid = request
            .field
            .grid
            .as_ref()
            .expect("point request validation requires a grid");
        let nearest = point
            .lut
            .lookup(point.lat, point.lon)
            .expect("point request validation requires an in-domain point");
        let stencils = sample_stencils_for_point(grid, nearest, point.lat, point.lon);
        (nearest, grid.nx, stencils)
    });
    let conversion = request
        .field
        .style
        .as_ref()
        .map(|style| style.convert)
        .unwrap_or(UnitConvert::None);
    let stored_hours_total = run.hours.len();
    let selected_position = run
        .hours
        .iter()
        .position(|entry| entry.hour == source_hour.hour)
        .unwrap_or(0);
    let history_limit = history_time_limit(request.point.is_some());
    let history_range = history_window(stored_hours_total, selected_position, history_limit);
    let total = history_range.len();
    let progress_stride = (total / 50).max(1);
    let mut failed_hours = 0usize;
    let mut samples = Vec::with_capacity(total);

    for (index, entry) in run.hours[history_range].iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".to_owned());
        }
        let hour = HourKey {
            model: source_hour.model.clone(),
            run: source_hour.run.clone(),
            hour: entry.hour,
            exact_time: entry.exact_time,
        };
        let loaded = view
            .open_hour(&hour.model, &hour.run, hour.hour)
            .ok()
            .filter(|reader| reader.meta().exact_time() == hour.exact_time)
            .and_then(|reader| {
                let meta = reader.variable(&variable)?;
                if meta.kind != "surface2d"
                    || meta.units != stored_units
                    || reader.meta().nx != source_nx
                    || reader.meta().ny != source_ny
                {
                    return None;
                }
                let stats = reader.stats_2d(&variable).ok();
                let point = point_sampling.as_ref().and_then(|(nearest, nx, stencils)| {
                    read_point_value(&reader, &variable, *nearest, *nx, stencils)
                });
                Some((
                    point.map(|value| conversion.apply(value)),
                    stats
                        .as_ref()
                        .and_then(|stats| stats.finite_min)
                        .map(|value| conversion.apply(value)),
                    stats
                        .as_ref()
                        .and_then(|stats| stats.finite_max)
                        .map(|value| conversion.apply(value)),
                ))
            });
        let (point, domain_min, domain_max) = loaded.unwrap_or_else(|| {
            failed_hours += 1;
            (None, None, None)
        });
        samples.push(HistorySample {
            hour,
            point,
            domain_min,
            domain_max,
        });

        let done = index + 1;
        if done == total || done % progress_stride == 0 {
            let _ = tx.send(TaskMessage::Progress { done, total });
            repaint.request_repaint();
        }
    }

    if samples.iter().all(|sample| {
        sample.point.is_none() && sample.domain_min.is_none() && sample.domain_max.is_none()
    }) {
        return Err(format!(
            "'{}' is not a readable stored 2-D field across this run; generated formulas and unsupported pressure-level fields need a dedicated history evaluator",
            variable
        ));
    }
    Ok(HistorySeries {
        source_hour,
        variable,
        units: request.field.units.clone(),
        point: request.point.as_ref().map(|point| (point.lat, point.lon)),
        samples,
        failed_hours,
        stored_hours_total,
    })
}

fn read_point_value(
    reader: &rw_store::reader::HourReader,
    variable: &str,
    nearest: usize,
    nx: usize,
    stencils: &[Option<FieldSampleStencil>; 4],
) -> Option<f32> {
    for stencil in stencils.iter().flatten().copied() {
        let (x0, y0, x1, y1) = stencil.window_bounds();
        let Ok(window) = reader.read_window_2d(variable, x0, y0, x1, y1) else {
            continue;
        };
        if window.nx == 2
            && window.ny == 2
            && window.values.len() == 4
            && let Ok(values) = window.values.try_into()
            && let Some(value) = stencil.sample(values)
        {
            return Some(value);
        }
    }
    let x = nearest % nx;
    let y = nearest / nx;
    reader
        .read_window_2d(variable, x, y, x + 1, y + 1)
        .ok()?
        .values
        .first()
        .copied()
        .filter(|value| value.is_finite())
}

fn history_window(total: usize, selected: usize, limit: usize) -> std::ops::Range<usize> {
    if total <= limit || limit == 0 {
        return 0..total;
    }
    let selected = selected.min(total - 1);
    let start = selected
        .saturating_sub(limit / 2)
        .min(total.saturating_sub(limit));
    start..start + limit
}

fn history_time_limit(has_point: bool) -> usize {
    if has_point {
        MAX_POINT_HISTORY_TIMES
    } else {
        MAX_DOMAIN_HISTORY_TIMES
    }
}

fn history_io_summary(series: &HistorySeries) -> String {
    let coverage = if series.samples.len() < series.stored_hours_total {
        format!(
            "{} of {} stored times (window centered on selected time)",
            series.samples.len(),
            series.stored_hours_total
        )
    } else {
        format!("{} stored times", series.samples.len())
    };
    if series.point.is_some() {
        format!(
            "{coverage} · point reads use tiny cell windows; extrema use cached store statistics."
        )
    } else {
        format!("{coverage} · extrema use cached store statistics; no map pin is required.")
    }
}

fn history_header(ui: &mut egui::Ui, series: &HistorySeries, metric: HistoryMetric) {
    ui.horizontal_wrapped(|ui| {
        ui.strong(format!(
            "{} · {} · {}",
            series.source_hour.model.to_uppercase(),
            series.source_hour.run,
            series.variable
        ));
        ui.weak(format!("({})", series.units));
    });
    match metric {
        HistoryMetric::Point => {
            if let Some((lat, lon)) = series.point {
                ui.weak(format!("Fixed at {lat:.4}°, {lon:.4}°"));
            } else {
                ui.weak("No fixed point is attached to this domain-extrema graph");
            }
        }
        HistoryMetric::DomainMinimum | HistoryMetric::DomainMaximum => {
            ui.weak("Whole stored model grid for each forecast time");
        }
    }
}

fn history_chart(
    ui: &mut egui::Ui,
    series: &HistorySeries,
    metric: HistoryMetric,
) -> Option<HourKey> {
    let size = egui::vec2(ui.available_width().max(360.0), 255.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals().clone();
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    painter.rect_stroke(
        rect,
        4.0,
        visuals.widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let plot = egui::Rect::from_min_max(
        rect.left_top() + egui::vec2(58.0, 16.0),
        rect.right_bottom() - egui::vec2(12.0, 30.0),
    );
    if plot.width() < 80.0 || plot.height() < 80.0 {
        return None;
    }

    let points = chart_values(series, metric);
    if points.is_empty() {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            format!("No {} values in this run", metric.label().to_lowercase()),
            egui::FontId::proportional(13.0),
            visuals.weak_text_color(),
        );
        return None;
    }
    let x_min = points
        .iter()
        .map(|point| point.axis)
        .fold(f64::INFINITY, f64::min);
    let mut x_max = points
        .iter()
        .map(|point| point.axis)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut x_min = x_min;
    if x_max <= x_min {
        x_min -= 0.5;
        x_max += 0.5;
    }
    let raw_min = points
        .iter()
        .map(|point| point.value)
        .fold(f32::INFINITY, f32::min);
    let raw_max = points
        .iter()
        .map(|point| point.value)
        .fold(f32::NEG_INFINITY, f32::max);
    let span = (raw_max - raw_min).abs();
    let pad = if span > 0.0 {
        span * 0.1
    } else {
        raw_min.abs().max(1.0) * 0.05
    };
    let y_min = raw_min - pad;
    let y_max = raw_max + pad;
    let x_screen =
        |value: f64| plot.left() + (((value - x_min) / (x_max - x_min)) as f32) * plot.width();
    let y_screen = |value: f32| {
        plot.bottom() - ((value - y_min) / (y_max - y_min)).clamp(0.0, 1.0) * plot.height()
    };

    let grid = visuals
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.7);
    for step in 0..=4 {
        let fraction = step as f32 / 4.0;
        let y = plot.bottom() - fraction * plot.height();
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(1.0, grid),
        );
        let value = y_min + fraction * (y_max - y_min);
        painter.text(
            egui::pos2(plot.left() - 7.0, y),
            egui::Align2::RIGHT_CENTER,
            format_chart_value(value),
            egui::FontId::monospace(10.0),
            visuals.weak_text_color(),
        );
    }
    painter.text(
        egui::pos2(plot.left(), plot.bottom() + 8.0),
        egui::Align2::LEFT_TOP,
        points.first().expect("non-empty").time_label.as_str(),
        egui::FontId::monospace(9.0),
        visuals.weak_text_color(),
    );
    painter.text(
        egui::pos2(plot.right(), plot.bottom() + 8.0),
        egui::Align2::RIGHT_TOP,
        points.last().expect("non-empty").time_label.as_str(),
        egui::FontId::monospace(9.0),
        visuals.weak_text_color(),
    );

    let line_color = match metric {
        HistoryMetric::Point => egui::Color32::from_rgb(57, 189, 248),
        HistoryMetric::DomainMinimum => egui::Color32::from_rgb(52, 211, 153),
        HistoryMetric::DomainMaximum => egui::Color32::from_rgb(248, 113, 113),
    };
    for segment in contiguous_chart_segments(series, metric) {
        if segment.len() >= 2 {
            painter.add(egui::Shape::line(
                segment
                    .iter()
                    .map(|point| egui::pos2(x_screen(point.axis), y_screen(point.value)))
                    .collect(),
                egui::Stroke::new(1.8, line_color),
            ));
        } else if let Some(point) = segment.first() {
            painter.circle_filled(
                egui::pos2(x_screen(point.axis), y_screen(point.value)),
                2.0,
                line_color,
            );
        }
    }

    if let Some(current) = points.iter().find(|point| point.hour == series.source_hour) {
        let position = egui::pos2(x_screen(current.axis), y_screen(current.value));
        painter.line_segment(
            [
                egui::pos2(position.x, plot.top()),
                egui::pos2(position.x, plot.bottom()),
            ],
            egui::Stroke::new(
                1.0,
                egui::Color32::from_rgb(250, 204, 21).gamma_multiply(0.65),
            ),
        );
        painter.circle_filled(position, 3.5, egui::Color32::from_rgb(250, 204, 21));
    }

    let extremum = match metric {
        HistoryMetric::DomainMinimum => points
            .iter()
            .min_by(|left, right| left.value.total_cmp(&right.value)),
        HistoryMetric::DomainMaximum => points
            .iter()
            .max_by(|left, right| left.value.total_cmp(&right.value)),
        HistoryMetric::Point => None,
    };
    if let Some(extremum) = extremum {
        let position = egui::pos2(x_screen(extremum.axis), y_screen(extremum.value));
        painter.circle_filled(position, 3.2, visuals.text_color());
        painter.text(
            position + egui::vec2(6.0, -5.0),
            egui::Align2::LEFT_BOTTOM,
            format!(
                "{} {} {} @ {}",
                metric.short_label(),
                format_chart_value(extremum.value),
                series.units,
                extremum.time_label,
            ),
            egui::FontId::monospace(10.0),
            visuals.text_color(),
        );
    }

    let hovered = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|position| response.hovered() && plot.contains(*position))
        .and_then(|position| {
            points.iter().min_by(|left, right| {
                (x_screen(left.axis) - position.x)
                    .abs()
                    .total_cmp(&(x_screen(right.axis) - position.x).abs())
            })
        });
    if let Some(point) = hovered {
        let position = egui::pos2(x_screen(point.axis), y_screen(point.value));
        painter.circle_filled(position, 3.4, line_color);
        response.clone().on_hover_ui(|ui| {
            ui.strong(point.time_label.as_str());
            ui.monospace(format!(
                "{}: {} {}",
                metric.label(),
                format_chart_value(point.value),
                series.units
            ));
            ui.weak("Click to load this model timestep");
        });
    }
    response
        .clicked()
        .then(|| hovered.map(|point| point.hour.clone()))
        .flatten()
}

#[derive(Clone)]
struct ChartPoint {
    hour: HourKey,
    axis: f64,
    value: f32,
    time_label: String,
}

fn chart_values(series: &HistorySeries, metric: HistoryMetric) -> Vec<ChartPoint> {
    series
        .samples
        .iter()
        .filter_map(|sample| {
            Some(ChartPoint {
                hour: sample.hour.clone(),
                axis: history_axis(&sample.hour),
                value: metric.value(sample)?,
                time_label: sample.hour.time_label(),
            })
        })
        .collect()
}

fn contiguous_chart_segments(
    series: &HistorySeries,
    metric: HistoryMetric,
) -> Vec<Vec<ChartPoint>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    for sample in &series.samples {
        if let Some(value) = metric.value(sample) {
            current.push(ChartPoint {
                hour: sample.hour.clone(),
                axis: history_axis(&sample.hour),
                value,
                time_label: sample.hour.time_label(),
            });
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn history_axis(hour: &HourKey) -> f64 {
    hour.exact_time
        .map(|exact| exact.lead_seconds as f64 / 3_600.0)
        .unwrap_or(f64::from(hour.hour))
}

fn format_chart_value(value: f32) -> String {
    let magnitude = value.abs();
    if magnitude >= 100.0 {
        format!("{value:.1}")
    } else if magnitude >= 1.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.4}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> HistorySample {
        HistorySample {
            hour: HourKey {
                model: "wrf".to_owned(),
                run: "local".to_owned(),
                hour: 7,
                exact_time: None,
            },
            point: Some(12.0),
            domain_min: Some(3.0),
            domain_max: Some(20.0),
        }
    }

    #[test]
    fn metric_chooses_point_and_domain_extrema_independently() {
        let sample = sample();
        assert_eq!(HistoryMetric::Point.value(&sample), Some(12.0));
        assert_eq!(HistoryMetric::DomainMinimum.value(&sample), Some(3.0));
        assert_eq!(HistoryMetric::DomainMaximum.value(&sample), Some(20.0));
    }

    #[test]
    fn feedback_v03412_domain_extrema_requires_neither_grid_nor_map_pin() {
        let request = ProbeHistoryRequest {
            store_root: PathBuf::from("unused"),
            field: Arc::new(FieldData {
                key: rw_ui::FieldKey {
                    hour: sample().hour,
                    var: "pressure_surface".to_owned(),
                },
                units: "Pa".to_owned(),
                nx: 2,
                ny: 2,
                values: vec![100_000.0; 4],
                range: Some((100_000.0, 100_000.0)),
                grid: None,
                lat_descending: false,
                style: None,
            }),
            point: None,
        };

        assert!(validate_request(&request).is_ok());
    }

    #[test]
    fn feedback_v03412_domain_extrema_axis_uses_exact_lead_seconds() {
        let hour = HourKey {
            model: "wrf".to_owned(),
            run: "local".to_owned(),
            hour: 99,
            exact_time: Some(rw_store::RwsExactTime::new(900, 1_800)),
        };
        assert!((history_axis(&hour) - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_values_split_chart_lines_instead_of_bridging_gaps() {
        let mut samples = vec![sample(), sample(), sample()];
        samples[0].hour.hour = 0;
        samples[1].hour.hour = 1;
        samples[1].point = None;
        samples[2].hour.hour = 2;
        let series = HistorySeries {
            source_hour: samples[0].hour.clone(),
            variable: "temperature_2m".to_owned(),
            units: "F".to_owned(),
            point: Some((35.0, -97.0)),
            samples,
            failed_hours: 1,
            stored_hours_total: 3,
        };
        let segments = contiguous_chart_segments(&series, HistoryMetric::Point);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].len(), 1);
        assert_eq!(segments[1].len(), 1);
    }

    #[test]
    fn feedback_v03412_domain_extrema_summary_does_not_claim_a_point_probe() {
        let series = HistorySeries {
            source_hour: sample().hour,
            variable: "pressure_surface".to_owned(),
            units: "hPa".to_owned(),
            point: None,
            samples: vec![sample()],
            failed_hours: 0,
            stored_hours_total: 1,
        };

        let summary = history_io_summary(&series);
        assert!(summary.contains("no map pin is required"));
        assert!(!summary.contains("point reads"));
    }

    #[test]
    fn very_long_runs_are_windowed_around_the_selected_time() {
        assert_eq!(history_window(1_000, 500, 256), 372..628);
        assert_eq!(history_window(1_000, 3, 256), 0..256);
        assert_eq!(history_window(1_000, 999, 256), 744..1_000);
        assert_eq!(history_window(10, 5, 256), 0..10);
    }

    #[test]
    fn feedback_v03412_cached_domain_extrema_keep_a_longer_minute_cadence_run() {
        assert_eq!(history_time_limit(true), 256);
        assert_eq!(history_time_limit(false), 4_096);
    }
}
