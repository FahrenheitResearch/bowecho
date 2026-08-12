// SPDX-License-Identifier: Apache-2.0

//! The live-fire driver: pushes the REAL app through its OWN action
//! pipeline — the same functions the user's gestures call — so the
//! acceptance path can be proven against the real engine without a
//! human at the mouse. Three modes:
//!
//! - `launch`: draw the domain (via the map pane's own drag→domain
//!   function), wait for the live estimate, open the review (live
//!   resolve), launch, then CLOSE THE APP MID-RUN (SurviveStudio proof).
//! - `watch`: reattach (startup heartbeat path, or the Runs registry
//!   path), watch to terminal, screenshot, close.
//! - `reopen`: reopen the completed run from the registry, let the model
//!   layer load a frame, screenshot, close.
//!
//! Every transition prints a `livefire:` evidence line to stderr.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::inspector::InspectorActions;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LivefireMode {
    Launch,
    Watch,
    Reopen,
}

impl LivefireMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "launch" => Some(Self::Launch),
            "watch" => Some(Self::Watch),
            "reopen" => Some(Self::Reopen),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Phase {
    Warmup { frames: u32 },
    Design,
    AwaitEstimate,
    AwaitReview,
    Launched { at: Instant },
    AwaitAttach,
    AwaitTerminal,
    AwaitFrameLoad { since: Instant },
    Screenshot { requested: bool },
    Closing,
}

pub struct LivefireDriver {
    pub mode: LivefireMode,
    phase: Phase,
    /// Seconds to keep watching after launch before the mid-run close.
    close_after: f64,
    /// Rehearsal: set `run_options.dry_run` so the engine resolves and
    /// stops before any device work (CPU-only pipeline proof).
    pub dry_run: bool,
    /// Forecast length the driver designs with.
    pub hours: f64,
    started: Instant,
    pub screenshot_path: Option<std::path::PathBuf>,
}

impl LivefireDriver {
    pub fn new(mode: LivefireMode, close_after: f64) -> Self {
        Self {
            mode,
            phase: Phase::Warmup { frames: 5 },
            close_after,
            dry_run: false,
            hours: 1.0,
            started: Instant::now(),
            screenshot_path: None,
        }
    }

    fn say(&self, message: &str) {
        eprintln!("livefire[{:?}]: {message}", self.mode);
    }
}

impl crate::app::StudioApp {
    /// One driver step per frame; fills `actions` exactly as the
    /// inspector would.
    pub fn livefire_step(&mut self, ctx: &egui::Context, actions: &mut InspectorActions) {
        let Some(mut driver) = self.livefire.take() else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(150));
        // Global timeout: a stuck phase is an honest failure, not a hang.
        if driver.started.elapsed() > Duration::from_secs(45 * 60) {
            driver.say("TIMEOUT after 45 min — closing");
            std::process::exit(3);
        }
        match &mut driver.phase {
            Phase::Warmup { frames } => {
                *frames -= 1;
                if *frames == 0 {
                    driver.phase = match driver.mode {
                        LivefireMode::Launch => Phase::Design,
                        LivefireMode::Watch | LivefireMode::Reopen => Phase::AwaitAttach,
                    };
                }
            }
            Phase::Design => {
                // The map pane's own drag→domain function: the exact code
                // a real drag calls, minus the raw mouse deltas.
                let domain = crate::map_pane::domain_from_drag(
                    (-98.6, 34.6),
                    (-96.4, 36.4),
                    self.draft.dx_m(),
                )
                .expect("livefire drag rectangle is a valid domain");
                driver.say(&format!(
                    "drew domain: sketch center {:.3},{:.3} ({}x{} @ {} m sketch)",
                    domain.ref_lat, domain.ref_lon, domain.nx, domain.ny, domain.dx_m
                ));
                self.view.center_lat = domain.ref_lat as f32;
                self.view.center_lon = domain.ref_lon as f32;
                self.view.scale = 30.0;
                self.draft.domain = Some(domain);
                self.draft.name = format!(
                    "livefire-{}{}",
                    if driver.dry_run { "dry-" } else { "" },
                    chrono::Utc::now().format("%H%M%S")
                );
                self.draft.length_hours = driver.hours;
                self.draft.history_interval_s = 900;
                self.draft.dry_run = driver.dry_run;
                driver.phase = Phase::AwaitEstimate;
            }
            Phase::AwaitEstimate => match &self.estimate {
                Some(Ok(estimate)) => {
                    driver.say(&format!(
                        "live estimate: vram {:.2} GiB, {} frames",
                        estimate.vram.estimate_gib.unwrap_or_default(),
                        estimate
                            .disk
                            .total_frames
                            .map(|frames| frames.to_string())
                            .unwrap_or_else(|| "?".into())
                    ));
                    actions.open_review = true;
                    driver.phase = Phase::AwaitReview;
                }
                Some(Err(error)) => {
                    driver.say(&format!("ESTIMATE FAILED: {error}"));
                    std::process::exit(3);
                }
                None => {}
            },
            Phase::AwaitReview => {
                if let Some(review) = &self.review {
                    match &review.fitted {
                        Some(fitted) => driver.say(&format!(
                            "live resolve: fitted {}x{} @ {:.0} m",
                            fitted.nx, fitted.ny, fitted.dx_m
                        )),
                        None => driver.say("live resolve: no fitted geometry (?)"),
                    }
                    actions.launch = true;
                    driver.phase = Phase::Launched { at: Instant::now() };
                }
            }
            Phase::Launched { at } => {
                if let Some(session) = &self.session {
                    let stage = session
                        .stages
                        .last()
                        .map(|stage| stage.id.as_str())
                        .unwrap_or("-");
                    // Dry-run rehearsal: the run finishes almost at once;
                    // report the terminal instead of a mid-run close.
                    if driver.dry_run && session.is_finished() {
                        driver.say(&format!(
                            "dry-run terminal: {} · stages {:?}",
                            session.liveness().0,
                            session
                                .stages
                                .iter()
                                .map(|stage| stage.id.clone())
                                .collect::<Vec<_>>()
                        ));
                        driver.phase = Phase::Closing;
                    } else if at.elapsed() > Duration::from_secs_f64(driver.close_after) {
                        driver.say(&format!(
                            "closing MID-RUN (SurviveStudio): stage {stage}, {} frames, liveness {}",
                            session.outputs.len(),
                            session.liveness().0
                        ));
                        driver.phase = Phase::Closing;
                    }
                }
            }
            Phase::AwaitAttach => {
                if self.session.is_some() {
                    driver.say("attached via startup heartbeat reattach");
                    driver.phase = match driver.mode {
                        LivefireMode::Reopen => Phase::AwaitFrameLoad {
                            since: Instant::now(),
                        },
                        _ => Phase::AwaitTerminal,
                    };
                } else {
                    // Heartbeat says finished (or missing) — open the
                    // newest registry entry through the Runs action.
                    if !self.runs_cache.is_empty() {
                        driver.say("no live heartbeat — opening newest run from the registry");
                        actions.open_run = Some(0);
                        driver.phase = match driver.mode {
                            LivefireMode::Reopen => Phase::AwaitFrameLoad {
                                since: Instant::now(),
                            },
                            _ => Phase::AwaitTerminal,
                        };
                    }
                }
            }
            Phase::AwaitTerminal => {
                if let Some(session) = &self.session
                    && session.is_finished()
                {
                    driver.say(&format!(
                        "terminal: {} · {} frames · stages {:?}",
                        session.liveness().0,
                        session.outputs.len(),
                        session
                            .stages
                            .iter()
                            .map(|stage| format!("{}:{:?}", stage.id, stage.status))
                            .collect::<Vec<_>>()
                    ));
                    driver.phase = Phase::AwaitFrameLoad {
                        since: Instant::now(),
                    };
                }
            }
            Phase::AwaitFrameLoad { since } => {
                let loaded = self
                    .model
                    .loaded
                    .as_ref()
                    .map(|(field, _)| format!("{}x{} {}", field.nx, field.ny, field.units));
                let give_up = since.elapsed() > Duration::from_secs(30);
                if let Some(description) = loaded {
                    driver.say(&format!("model field loaded: maxdbz {description}"));
                    // The hover lookup, driven at the domain center: the
                    // exact value_at() call the cursor readout makes,
                    // against the run's own wrfout.
                    if let (Some((field, _)), Some(domain)) = (
                        self.model.loaded.as_ref(),
                        self.session.as_ref().and_then(|session| session.domain),
                    ) {
                        match field.value_at(domain.ref_lat as f32, domain.ref_lon as f32) {
                            Some(value) => driver.say(&format!(
                                "hover value at domain center {:.3},{:.3}: MAXDBZ {value:.1} {}",
                                domain.ref_lat, domain.ref_lon, field.units
                            )),
                            None => driver.say("hover value at domain center: MISS (?)"),
                        }
                    }
                    driver.phase = Phase::Screenshot { requested: false };
                } else if give_up {
                    let error = self
                        .model
                        .error
                        .as_ref()
                        .map(|(_, error)| error.clone())
                        .unwrap_or_else(|| "no frame wanted/loaded".into());
                    driver.say(&format!("model field NOT loaded after 30 s: {error}"));
                    driver.phase = Phase::Screenshot { requested: false };
                }
            }
            Phase::Screenshot { requested } => {
                if !*requested {
                    *requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                        egui::UserData::default(),
                    ));
                } else {
                    let image = ctx.input(|input| {
                        input.events.iter().find_map(|event| match event {
                            egui::Event::Screenshot { image, .. } => Some(image.clone()),
                            _ => None,
                        })
                    });
                    if let Some(image) = image {
                        let path = std::env::temp_dir().join(format!(
                            "arwen-livefire-{:?}-{}.png",
                            driver.mode,
                            chrono::Utc::now().format("%H%M%S")
                        ));
                        match save_screenshot(&image, &path) {
                            Ok(()) => {
                                driver.say(&format!("screenshot {}", path.display()));
                                driver.screenshot_path = Some(path);
                            }
                            Err(error) => driver.say(&format!("screenshot failed: {error}")),
                        }
                        driver.phase = Phase::Closing;
                    }
                }
            }
            Phase::Closing => {
                driver.say("clean self-close");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        self.livefire = Some(driver);
    }
}

pub(crate) fn save_screenshot(
    image: &egui::ColorImage,
    path: &std::path::Path,
) -> Result<(), String> {
    let [width, height] = image.size;
    let mut rgba = Vec::with_capacity(width * height * 4);
    for pixel in &image.pixels {
        rgba.extend_from_slice(&pixel.to_array());
    }
    let buffer: image::RgbaImage = image::ImageBuffer::from_raw(width as u32, height as u32, rgba)
        .ok_or("screenshot buffer size mismatch")?;
    buffer
        .save(path)
        .map_err(|error| format!("save {}: {error}", path.display()))
}
