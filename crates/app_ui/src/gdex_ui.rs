//! GDEX (NSF NCAR CONUS II) in-app catalog browser — Stage 1b UI.
//!
//! Stage 1a landed the UI-free plumbing in [`data_source::gdex`] (THREDDS
//! catalog crawl, NCSS subset URL builder, resumable download). This module is
//! the user-facing browser that makes it usable: a lazy, in-app catalog tree
//! rooted at CONUS II (`d612005`) that walks the catalog one level at a time,
//! downloads a file (or an NCSS subset), and hands the local path to the model
//! dock's existing importer.
//!
//! ## Async architecture (respect the freeze lesson)
//!
//! Every GDEX core call is BLOCKING and the server is flaky (503s, empty
//! bodies — the core retries internally). None of it runs on the egui update
//! thread. Three [`WorkerSlot`]s carry the work:
//!
//! - `catalog_slot` — one catalog-level fetch at a time ([`gdex::fetch_and_parse_catalog`]),
//!   fed from `catalog_queue` so a burst of expand clicks is serialized (polite
//!   to the flaky server; matches the crawl concurrency cap in the core).
//! - `ncss_slot` — the subset form's grid metadata fetch ([`gdex::fetch_ncss_dataset`]).
//! - `download_slot` — one download at a time ([`gdex::download_to_path`]).
//!
//! The core's `download_to_path` streams internally with no progress callback,
//! so the progress row reads the `.download` temp file's size each frame — an
//! honest bytes/total without touching the proven core.
//!
//! ## Download -> import handoff
//!
//! [`GdexBrowser::poll`] returns the local path of a completed download; the
//! model dock feeds it to `local_import::spawn_import_paths` — the SAME import
//! pump, progress, store write, and plot path as the manual model import. A
//! CONUS II `.nc` (wrf2d surface or wrf3d TK/Z/P) imports unchanged.
//!
//! ## Testing
//!
//! The pure logic — tree-node state machine, leaf-row formatting, the NCSS
//! form -> subset builder, the download-result transition, the import-handoff
//! decision — lives in plain functions with unit tests (the nodes are
//! headless). The egui widget code is owner-RC-judged.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use eframe::egui;

use data_source::gdex::{
    self, DownloadOutcome, LatLonBox, Leaf, NcssGridDataset, NcssSubset, ParsedCatalog,
};
use ui_core::worker_slot::{SlotPoll, WorkerSlot};

/// The dataset the browser opens on: CONUS II (present + future WRF).
pub const CONUS_II_DATASET_ID: &str = "d612005";

// ---------------------------------------------------------------------------
// Tree-node state machine (pure, unit-tested)
// ---------------------------------------------------------------------------

/// A resolved child catalog: its absolute `catalog.xml` URL and a display
/// title derived from the URL (the core drops the `<catalogRef>` title).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChildRef {
    catalog_url: String,
    title: String,
}

/// Load state of one catalog level (keyed by its catalog URL).
#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeLoad {
    /// Never fetched (or reset for retry). Expanding it kicks a fetch.
    NotLoaded,
    /// A fetch is queued or in flight.
    Loading,
    /// Fetched: the child catalogs to recurse into, and the file leaves here.
    Loaded {
        children: Vec<ChildRef>,
        leaves: Vec<Leaf>,
    },
    /// The fetch failed (retryable — the flaky server 503s; the core already
    /// retried, so this is the message after exhaustion).
    Error(String),
}

/// Derive a node's display title from its catalog URL: the last path segment
/// before `/catalog.xml` (e.g. `.../future2D/catalog.xml` -> `future2D`,
/// `.../future2D/208001/catalog.xml` -> `208001`). Falls back to the full URL
/// if the shape is unexpected.
fn catalog_title_from_url(catalog_url: &str) -> String {
    let trimmed = catalog_url
        .strip_suffix("/catalog.xml")
        .unwrap_or(catalog_url);
    trimmed
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(catalog_url)
        .to_owned()
}

/// Build the child references from a parsed catalog level, titling each by its
/// URL. Child order is preserved from the core (already sorted/deduped).
fn children_from_parsed(parsed: &ParsedCatalog) -> Vec<ChildRef> {
    parsed
        .child_catalog_urls
        .iter()
        .map(|url| ChildRef {
            catalog_url: url.clone(),
            title: catalog_title_from_url(url),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Leaf-row formatting (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Human-readable byte size (decimal units, matching THREDDS `<dataSize>`).
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1_000.0;
    const MB: f64 = 1_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Trim a modification timestamp to its date part for the compact row
/// (`2024-03-25T21:34:51.095Z` -> `2024-03-25`).
fn short_date(date: &str) -> String {
    date.split('T').next().unwrap_or(date).to_owned()
}

/// One leaf's compact row label: name, then human size and short date when the
/// catalog advertised them.
fn leaf_row_label(leaf: &Leaf) -> String {
    let mut trailer = String::new();
    if let Some(bytes) = leaf.size_bytes {
        trailer.push_str(" — ");
        trailer.push_str(&human_size(bytes));
    }
    if let Some(date) = &leaf.date {
        trailer.push_str(" — ");
        trailer.push_str(&short_date(date));
    }
    format!("{}{trailer}", leaf.name)
}

// ---------------------------------------------------------------------------
// NCSS subset form (pure builder is unit-tested)
// ---------------------------------------------------------------------------

/// Load state of the subset form's grid metadata.
#[derive(Clone, Debug, PartialEq)]
enum MetaLoad {
    Loading,
    Loaded(NcssGridDataset),
    Error(String),
}

/// Editable state for the "Subset (NCSS)" form for one leaf.
struct SubsetForm {
    leaf: Leaf,
    meta: MetaLoad,
    /// Selected variable names (each becomes a `var=` in the subset URL).
    selected: BTreeSet<String>,
    /// Substring filter over the (large) variable list.
    var_filter: String,
    /// Narrow to a bounding box (else the full grid).
    use_bbox: bool,
    north: String,
    south: String,
    east: String,
    west: String,
    /// Narrow to a single time (else all times in the file).
    use_time: bool,
    time: String,
    /// Whether the bbox/time fields have been seeded from the metadata yet.
    seeded: bool,
}

impl SubsetForm {
    fn new(leaf: Leaf) -> Self {
        Self {
            leaf,
            meta: MetaLoad::Loading,
            selected: BTreeSet::new(),
            var_filter: String::new(),
            use_bbox: false,
            north: String::new(),
            south: String::new(),
            east: String::new(),
            west: String::new(),
            use_time: false,
            time: String::new(),
            seeded: false,
        }
    }

    /// Seed the bbox/time fields from the parsed metadata (once), so toggling
    /// "bounding box" starts from the full-grid extent and "single time" from
    /// the file's first time.
    fn seed_from_meta(&mut self, meta: &NcssGridDataset) {
        if self.seeded {
            return;
        }
        self.seeded = true;
        if let Some(bbox) = &meta.lat_lon_box {
            self.north = fmt_coord(bbox.north);
            self.south = fmt_coord(bbox.south);
            self.east = fmt_coord(bbox.east);
            self.west = fmt_coord(bbox.west);
        }
        if let Some(span) = &meta.time_span {
            self.time = span.begin.clone();
        }
    }

    /// Build the [`NcssSubset`] from the current selection, or a user-facing
    /// error when the form is not ready to submit. Pure — unit-tested.
    fn build_subset(&self) -> Result<NcssSubset, String> {
        if self.selected.is_empty() {
            return Err("Select at least one variable to subset.".to_owned());
        }
        let bbox = if self.use_bbox {
            Some(LatLonBox {
                north: parse_coord(&self.north, "north")?,
                south: parse_coord(&self.south, "south")?,
                east: parse_coord(&self.east, "east")?,
                west: parse_coord(&self.west, "west")?,
            })
        } else {
            None
        };
        let time = if self.use_time && !self.time.trim().is_empty() {
            Some(self.time.trim().to_owned())
        } else {
            None
        };
        Ok(NcssSubset {
            vars: self.selected.iter().cloned().collect(),
            bbox,
            time,
            ..NcssSubset::default()
        })
    }

    /// Local destination filename for this subset download (sanitized for
    /// NTFS). Distinct from the full-file name so a subset never clobbers a
    /// full download of the same leaf.
    fn subset_dest_name(&self) -> String {
        let stem = self
            .leaf
            .name
            .strip_suffix(".nc")
            .unwrap_or(&self.leaf.name);
        let tag = self
            .selected
            .iter()
            .next()
            .map(|first| {
                if self.selected.len() > 1 {
                    format!("{first}+{}", self.selected.len() - 1)
                } else {
                    first.clone()
                }
            })
            .unwrap_or_else(|| "subset".to_owned());
        gdex::sanitize_leaf_filename(&format!("{stem}_subset_{tag}.nc"))
    }
}

/// Format a coordinate for the editable field (drop a trailing `.0`).
fn fmt_coord(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// Parse an edited coordinate, naming the field in the error.
fn parse_coord(text: &str, field: &str) -> Result<f64, String> {
    text.trim()
        .parse::<f64>()
        .map_err(|_| format!("{field} bound “{}” is not a number.", text.trim()))
}

// ---------------------------------------------------------------------------
// Download-result transition + import-handoff decision (pure, unit-tested)
// ---------------------------------------------------------------------------

/// The path to import once a download completes, or `None` when the outcome is
/// not importable (empty, or not a model file). The import-handoff decision.
fn import_path_for_outcome(outcome: &DownloadOutcome) -> Option<PathBuf> {
    if outcome.bytes == 0 {
        return None;
    }
    if crate::local_import::is_supported_model_file(&outcome.path) {
        Some(outcome.path.clone())
    } else {
        None
    }
}

/// What a finished download resolves to for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
enum DownloadResolution {
    /// Success — hand this path to the importer, with a status note.
    Import { path: PathBuf, note: String },
    /// Success but nothing to import (shouldn't happen for `.nc`).
    Done(String),
    /// Failed — a retryable status message.
    Failed(String),
}

/// Resolve a finished download into a UI outcome. Pure — unit-tested.
fn resolve_download(result: Result<DownloadOutcome, String>) -> DownloadResolution {
    match result {
        Ok(outcome) => {
            let how = if outcome.cache_hit {
                "already downloaded"
            } else if outcome.resumed {
                "resumed"
            } else {
                "downloaded"
            };
            match import_path_for_outcome(&outcome) {
                Some(path) => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    DownloadResolution::Import {
                        path,
                        note: format!("{name}: {how} ({}) — importing…", human_size(outcome.bytes)),
                    }
                }
                None => DownloadResolution::Done(format!(
                    "Downloaded {} ({}) but it is not an importable model file.",
                    outcome.path.display(),
                    human_size(outcome.bytes)
                )),
            }
        }
        Err(message) => DownloadResolution::Failed(format!("Download failed: {message}")),
    }
}

// ---------------------------------------------------------------------------
// Worker messages
// ---------------------------------------------------------------------------

/// One catalog level fetched off-thread.
struct CatalogFetch {
    catalog_url: String,
    result: Result<ParsedCatalog, String>,
}

/// One NCSS grid metadata document fetched off-thread.
struct NcssFetch {
    url_path: String,
    result: Result<NcssGridDataset, String>,
}

/// One finished download.
struct DownloadDone {
    result: Result<DownloadOutcome, String>,
}

/// A download in flight — enough to render an honest progress row by reading
/// the `.download` temp size each frame.
struct ActiveDownload {
    label: String,
    temp_path: PathBuf,
    dest: PathBuf,
    /// Total bytes when known (full files advertise a size; subsets don't).
    total: Option<u64>,
}

/// An intent produced by rendering (applied after, to keep `self` borrows
/// disjoint from egui's closures).
enum Action {
    ToggleNode(String),
    RetryNode(String),
    DownloadFull(Leaf),
    OpenSubset(Leaf),
}

/// The subset form's submit/cancel intent.
enum SubsetIntent {
    Download {
        url: String,
        dest_name: String,
        label: String,
    },
    Error(String),
    Cancel,
}

// ---------------------------------------------------------------------------
// The browser
// ---------------------------------------------------------------------------

/// In-app GDEX catalog browser. Owned by the model dock; its window is drawn
/// from the dock's `ui()` and its workers polled from the dock's `pump()`.
pub struct GdexBrowser {
    /// Window visibility (toggled from the dock's import controls).
    pub open: bool,
    /// First-open bootstrap flag (auto-expand + load the root).
    initialized: bool,
    /// The dataset's root catalog URL.
    root_url: String,
    /// Per-catalog-level load state, keyed by catalog URL (in-memory cache of
    /// expanded levels for the session).
    nodes: BTreeMap<String, NodeLoad>,
    /// Which catalog URLs are expanded in the tree.
    expanded: BTreeSet<String>,
    /// One catalog fetch in flight; the rest wait here (serialized, polite).
    catalog_slot: WorkerSlot<CatalogFetch>,
    catalog_queue: VecDeque<String>,
    /// The subset form's grid-metadata fetch.
    ncss_slot: WorkerSlot<NcssFetch>,
    /// One download in flight.
    download_slot: WorkerSlot<DownloadDone>,
    active_download: Option<ActiveDownload>,
    /// The open subset form, if any.
    subset_form: Option<SubsetForm>,
    /// A short status/error line (flaky-server failures surface here).
    status: Option<String>,
}

impl Default for GdexBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl GdexBrowser {
    pub fn new() -> Self {
        Self {
            open: false,
            initialized: false,
            root_url: gdex::dataset_catalog_url(CONUS_II_DATASET_ID),
            nodes: BTreeMap::new(),
            expanded: BTreeSet::new(),
            catalog_slot: WorkerSlot::idle("gdex-catalog"),
            catalog_queue: VecDeque::new(),
            ncss_slot: WorkerSlot::idle("gdex-ncss"),
            download_slot: WorkerSlot::idle("gdex-download"),
            active_download: None,
            subset_form: None,
            status: None,
        }
    }

    /// True while any GDEX work is in flight (drives the dock's spinner hint).
    pub fn busy(&self) -> bool {
        self.catalog_slot.in_flight()
            || self.ncss_slot.in_flight()
            || self.download_slot.in_flight()
            || !self.catalog_queue.is_empty()
    }

    // -- state-machine helpers (pure-ish; no egui) --------------------------

    /// Ensure a node will be fetched: set it Loading and enqueue it (unless it
    /// is already Loaded, Loading, or queued).
    fn ensure_load(&mut self, catalog_url: &str) {
        if matches!(
            self.nodes.get(catalog_url),
            Some(NodeLoad::Loaded { .. }) | Some(NodeLoad::Loading)
        ) {
            return;
        }
        self.nodes.insert(catalog_url.to_owned(), NodeLoad::Loading);
        if !self.catalog_queue.iter().any(|url| url == catalog_url) {
            self.catalog_queue.push_back(catalog_url.to_owned());
        }
    }

    /// Spawn the next queued catalog fetch when the slot is free.
    fn pump_catalog_queue(&mut self, ctx: &egui::Context) {
        if self.catalog_slot.in_flight() {
            return;
        }
        let Some(catalog_url) = self.catalog_queue.pop_front() else {
            return;
        };
        let url = catalog_url.clone();
        self.catalog_slot.spawn(ctx, move |tx| {
            let result = gdex::fetch_and_parse_catalog(&url).map_err(|err| err.to_string());
            let _ = tx.send(CatalogFetch {
                catalog_url: url,
                result,
            });
        });
    }

    // -- polling (drains workers; returns a path to import) -----------------

    /// Drain the workers. Returns the local path of a completed download so the
    /// dock can hand it to the importer. Called every frame from the dock's
    /// `pump()`, so downloads finish and import even with the window closed.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        // Catalog levels.
        match self.catalog_slot.poll() {
            SlotPoll::Ready(fetch) => self.apply_catalog(fetch),
            SlotPoll::Disconnected => {
                self.status = Some("Catalog worker stopped unexpectedly.".to_owned());
            }
            SlotPoll::Idle | SlotPoll::Pending => {}
        }
        self.pump_catalog_queue(ctx);

        // Subset-form metadata.
        match self.ncss_slot.poll() {
            SlotPoll::Ready(fetch) => self.apply_ncss(fetch),
            SlotPoll::Disconnected => {
                self.status = Some("NCSS metadata worker stopped unexpectedly.".to_owned());
            }
            SlotPoll::Idle | SlotPoll::Pending => {}
        }

        // Download.
        let mut import_path = None;
        match self.download_slot.poll() {
            SlotPoll::Ready(done) => {
                self.active_download = None;
                match resolve_download(done.result) {
                    DownloadResolution::Import { path, note } => {
                        self.status = Some(note);
                        import_path = Some(path);
                    }
                    DownloadResolution::Done(note) | DownloadResolution::Failed(note) => {
                        self.status = Some(note);
                    }
                }
            }
            SlotPoll::Disconnected => {
                self.active_download = None;
                self.status = Some("Download worker stopped unexpectedly.".to_owned());
            }
            SlotPoll::Idle | SlotPoll::Pending => {}
        }
        import_path
    }

    fn apply_catalog(&mut self, fetch: CatalogFetch) {
        let state = match fetch.result {
            Ok(parsed) => NodeLoad::Loaded {
                children: children_from_parsed(&parsed),
                leaves: parsed.leaves,
            },
            Err(message) => NodeLoad::Error(message),
        };
        self.nodes.insert(fetch.catalog_url, state);
    }

    fn apply_ncss(&mut self, fetch: NcssFetch) {
        let Some(form) = self.subset_form.as_mut() else {
            return;
        };
        if form.leaf.url_path != fetch.url_path {
            return; // the user moved to a different leaf while it loaded
        }
        form.meta = match fetch.result {
            Ok(meta) => {
                form.seed_from_meta(&meta);
                MetaLoad::Loaded(meta)
            }
            Err(message) => MetaLoad::Error(message),
        };
    }

    /// Start a download (full file or subset). Refuses when one is already in
    /// flight (one at a time — the files are large).
    fn start_download(
        &mut self,
        ctx: &egui::Context,
        url: String,
        dest: PathBuf,
        label: String,
        total: Option<u64>,
    ) {
        if self.download_slot.in_flight() {
            self.status = Some("A download is already in progress.".to_owned());
            return;
        }
        let temp_path = dest.with_extension("download");
        self.active_download = Some(ActiveDownload {
            label,
            temp_path,
            dest: dest.clone(),
            total,
        });
        self.status = None;
        self.download_slot.spawn(ctx, move |tx| {
            let result = gdex::download_to_path(&url, &dest).map_err(|err| err.to_string());
            let _ = tx.send(DownloadDone { result });
        });
    }

    // -- rendering ----------------------------------------------------------

    /// Draw the browser window(s). `cache_dir` is where downloads land (the
    /// dock passes `settings::gdex_cache_dir()`).
    pub fn ui(&mut self, ui: &mut egui::Ui, cache_dir: &Path) {
        if !self.open {
            return;
        }
        let ctx = ui.ctx().clone();
        if !self.initialized {
            self.initialized = true;
            let root = self.root_url.clone();
            self.expanded.insert(root.clone());
            self.ensure_load(&root);
            self.pump_catalog_queue(&ctx);
        }

        let mut actions: Vec<Action> = Vec::new();
        let mut open = self.open;
        egui::Window::new("🌐 GDEX — NSF NCAR CONUS II")
            .open(&mut open)
            .default_size([460.0, 520.0])
            .show(&ctx, |ui| {
                self.window_body(ui, &mut actions);
            });
        self.open = open;

        for action in actions {
            self.apply_action(action, &ctx, cache_dir);
        }

        // The subset form (its own window), if any.
        if self.subset_form.is_some() {
            self.render_subset_form(&ctx, cache_dir);
        }

        self.pump_catalog_queue(&ctx);
    }

    /// The main window body: attribution header, status/progress, then the
    /// lazy tree.
    fn window_body(&self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.label(
            egui::RichText::new(
                "NSF NCAR GDEX · CONUS II regional climate WRF (present + future) · \
                 CC-BY 4.0 · DOI 10.5065/49SN-8E08",
            )
            .small()
            .weak(),
        );
        ui.separator();

        // Live download progress row.
        if let Some(active) = &self.active_download {
            let downloaded = std::fs::metadata(&active.temp_path)
                .map(|meta| meta.len())
                .ok()
                .or_else(|| std::fs::metadata(&active.dest).map(|meta| meta.len()).ok())
                .unwrap_or(0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(egui::RichText::new(&active.label).small());
            });
            match active.total {
                Some(total) if total > 0 => {
                    let fraction = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
                    ui.add(egui::ProgressBar::new(fraction).text(format!(
                        "{} / {}",
                        human_size(downloaded),
                        human_size(total)
                    )));
                }
                _ => {
                    ui.label(
                        egui::RichText::new(format!("{} downloaded", human_size(downloaded)))
                            .small()
                            .weak(),
                    );
                }
            }
            ui.separator();
        }

        if let Some(status) = &self.status {
            ui.label(egui::RichText::new(status).small());
            ui.separator();
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                render_node(
                    &self.nodes,
                    &self.expanded,
                    ui,
                    &self.root_url,
                    "CONUS II (d612005)",
                    0,
                    actions,
                );
            });
    }

    fn apply_action(&mut self, action: Action, ctx: &egui::Context, cache_dir: &Path) {
        match action {
            Action::ToggleNode(url) => {
                if self.expanded.contains(&url) {
                    self.expanded.remove(&url);
                } else {
                    self.expanded.insert(url.clone());
                    self.ensure_load(&url);
                }
            }
            Action::RetryNode(url) => {
                self.nodes.insert(url.clone(), NodeLoad::NotLoaded);
                self.expanded.insert(url.clone());
                self.ensure_load(&url);
            }
            Action::DownloadFull(leaf) => {
                let dest = gdex::local_path_for_leaf(cache_dir, &leaf);
                let label = format!("Downloading {}", leaf.name);
                self.start_download(ctx, leaf.download_url.clone(), dest, label, leaf.size_bytes);
            }
            Action::OpenSubset(leaf) => {
                let url_path = leaf.url_path.clone();
                self.subset_form = Some(SubsetForm::new(leaf));
                self.ncss_slot.cancel();
                self.ncss_slot.spawn(ctx, move |tx| {
                    let result = gdex::fetch_ncss_dataset(&url_path).map_err(|err| err.to_string());
                    let _ = tx.send(NcssFetch { url_path, result });
                });
            }
        }
    }

    /// The subset form window. Mutates `self.subset_form` directly (a disjoint
    /// field) and returns the submit/cancel intent, applied afterward.
    fn render_subset_form(&mut self, ctx: &egui::Context, cache_dir: &Path) {
        let mut open = true;
        let mut intent: Option<SubsetIntent> = None;
        if let Some(form) = self.subset_form.as_mut() {
            egui::Window::new("NCSS subset")
                .open(&mut open)
                .default_size([360.0, 440.0])
                .show(ctx, |ui| {
                    intent = subset_form_body(form, ui);
                });
        }

        match intent {
            Some(SubsetIntent::Cancel) => {
                self.subset_form = None;
                self.ncss_slot.cancel();
            }
            Some(SubsetIntent::Error(message)) => {
                self.status = Some(message);
            }
            Some(SubsetIntent::Download {
                url,
                dest_name,
                label,
            }) => {
                let dest = cache_dir.join(dest_name);
                self.subset_form = None;
                self.start_download(ctx, url, dest, label, None);
            }
            None => {}
        }
        if !open {
            self.subset_form = None;
            self.ncss_slot.cancel();
        }
    }
}

/// Render one catalog node and (when expanded) its children and leaves.
/// Free function so the caller keeps `self.nodes`/`self.expanded` borrowed
/// immutably while pushing intents into `actions`.
fn render_node(
    nodes: &BTreeMap<String, NodeLoad>,
    expanded: &BTreeSet<String>,
    ui: &mut egui::Ui,
    catalog_url: &str,
    title: &str,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let is_expanded = expanded.contains(catalog_url);
    let load = nodes.get(catalog_url);
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0);
        let arrow = if is_expanded { "▼" } else { "▶" };
        if ui
            .selectable_label(false, format!("{arrow} {title}"))
            .clicked()
        {
            actions.push(Action::ToggleNode(catalog_url.to_owned()));
        }
        if matches!(load, Some(NodeLoad::Loading)) {
            ui.spinner();
        } else if matches!(load, Some(NodeLoad::Error(_))) {
            ui.label(egui::RichText::new("⚠").color(egui::Color32::YELLOW));
        }
    });

    if !is_expanded {
        return;
    }
    match load {
        Some(NodeLoad::Loaded { children, leaves }) => {
            for child in children {
                render_node(
                    nodes,
                    expanded,
                    ui,
                    &child.catalog_url,
                    &child.title,
                    depth + 1,
                    actions,
                );
            }
            for leaf in leaves {
                render_leaf(ui, leaf, depth + 1, actions);
            }
            if children.is_empty() && leaves.is_empty() {
                indented_weak(ui, depth + 1, "(empty)");
            }
        }
        Some(NodeLoad::Error(message)) => {
            ui.horizontal(|ui| {
                ui.add_space((depth + 1) as f32 * 14.0);
                ui.label(
                    egui::RichText::new(message)
                        .small()
                        .color(egui::Color32::LIGHT_RED),
                );
                if ui.small_button("Retry").clicked() {
                    actions.push(Action::RetryNode(catalog_url.to_owned()));
                }
            });
        }
        _ => indented_weak(ui, depth + 1, "loading…"),
    }
}

/// Render one file leaf: its label plus the two per-file actions.
fn render_leaf(ui: &mut egui::Ui, leaf: &Leaf, depth: usize, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0);
        ui.label(egui::RichText::new(leaf_row_label(leaf)).small());
        if ui
            .small_button("Download full")
            .on_hover_text("Download the whole file, then import it into the model store.")
            .clicked()
        {
            actions.push(Action::DownloadFull(leaf.clone()));
        }
        if ui
            .small_button("Subset (NCSS)")
            .on_hover_text("Choose variables / bbox / time, then download just that subset.")
            .clicked()
        {
            actions.push(Action::OpenSubset(leaf.clone()));
        }
    });
}

fn indented_weak(ui: &mut egui::Ui, depth: usize, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(depth as f32 * 14.0);
        ui.label(egui::RichText::new(text).small().weak());
    });
}

/// The subset form's body; returns the submit/cancel intent.
fn subset_form_body(form: &mut SubsetForm, ui: &mut egui::Ui) -> Option<SubsetIntent> {
    ui.label(egui::RichText::new(&form.leaf.name).strong());
    ui.separator();

    let meta = match &form.meta {
        MetaLoad::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Loading variables…");
            });
            return form_footer(ui, form, false);
        }
        MetaLoad::Error(message) => {
            ui.label(egui::RichText::new(message).color(egui::Color32::LIGHT_RED));
            return form_footer(ui, form, false);
        }
        MetaLoad::Loaded(meta) => meta.clone(),
    };

    ui.label(format!("{} variables", meta.variables.len()));
    ui.horizontal(|ui| {
        ui.label("Filter:");
        ui.text_edit_singleline(&mut form.var_filter);
    });
    let filter = form.var_filter.trim().to_ascii_lowercase();
    egui::ScrollArea::vertical()
        .id_salt("gdex_subset_vars")
        .max_height(180.0)
        .show(ui, |ui| {
            for var in &meta.variables {
                if !filter.is_empty() && !var.name.to_ascii_lowercase().contains(&filter) {
                    continue;
                }
                let mut checked = form.selected.contains(&var.name);
                let label = match (&var.units, &var.description) {
                    (Some(units), Some(desc)) => format!("{} — {desc} [{units}]", var.name),
                    (Some(units), None) => format!("{} [{units}]", var.name),
                    (None, Some(desc)) => format!("{} — {desc}", var.name),
                    (None, None) => var.name.clone(),
                };
                if ui.checkbox(&mut checked, label).changed() {
                    if checked {
                        form.selected.insert(var.name.clone());
                    } else {
                        form.selected.remove(&var.name);
                    }
                }
            }
        });

    ui.separator();
    ui.checkbox(&mut form.use_bbox, "Bounding box (else full grid)");
    if form.use_bbox {
        egui::Grid::new("gdex_subset_bbox")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("North");
                ui.text_edit_singleline(&mut form.north);
                ui.end_row();
                ui.label("South");
                ui.text_edit_singleline(&mut form.south);
                ui.end_row();
                ui.label("East");
                ui.text_edit_singleline(&mut form.east);
                ui.end_row();
                ui.label("West");
                ui.text_edit_singleline(&mut form.west);
                ui.end_row();
            });
    }
    ui.checkbox(&mut form.use_time, "Single time (else all times in file)");
    if form.use_time {
        ui.horizontal(|ui| {
            ui.label("Time");
            ui.text_edit_singleline(&mut form.time);
        });
    }

    form_footer(ui, form, true)
}

/// The subset form's action row (Download / Cancel).
fn form_footer(ui: &mut egui::Ui, form: &SubsetForm, ready: bool) -> Option<SubsetIntent> {
    ui.separator();
    let mut intent = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("Download subset"))
            .clicked()
        {
            intent = Some(match form.build_subset() {
                Ok(subset) => {
                    let url = gdex::ncss_subset_url(&form.leaf.url_path, &subset);
                    SubsetIntent::Download {
                        url,
                        dest_name: form.subset_dest_name(),
                        label: format!("Downloading subset of {}", form.leaf.name),
                    }
                }
                Err(message) => SubsetIntent::Error(message),
            });
        }
        if ui.button("Cancel").clicked() {
            intent = Some(SubsetIntent::Cancel);
        }
    });
    intent
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_source::gdex::{NcssVariable, TimeSpan};

    fn leaf(name: &str, size: Option<u64>, date: Option<&str>) -> Leaf {
        Leaf {
            name: name.to_owned(),
            url_path: format!("files/g/d612005/future2D/208001/{name}"),
            download_url: format!("https://tds.gdex.ucar.edu/thredds/fileServer/x/{name}"),
            ncss_url: format!("https://tds.gdex.ucar.edu/thredds/ncss/grid/x/{name}"),
            size_bytes: size,
            date: date.map(str::to_owned),
        }
    }

    #[test]
    fn catalog_title_is_the_last_directory_segment() {
        assert_eq!(
            catalog_title_from_url(
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/catalog.xml"
            ),
            "future2D"
        );
        assert_eq!(
            catalog_title_from_url(
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/208001/catalog.xml"
            ),
            "208001"
        );
    }

    #[test]
    fn children_from_parsed_titles_each_child() {
        let parsed = ParsedCatalog {
            catalog_url: "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/catalog.xml"
                .to_owned(),
            child_catalog_urls: vec![
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/INVARIANT/catalog.xml"
                    .to_owned(),
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/catalog.xml"
                    .to_owned(),
            ],
            leaves: vec![],
        };
        let children = children_from_parsed(&parsed);
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].title, "INVARIANT");
        assert_eq!(children[1].title, "future2D");
        assert!(children[1].catalog_url.ends_with("/future2D/catalog.xml"));
    }

    #[test]
    fn human_size_uses_decimal_units() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(12_000), "12 KB");
        assert_eq!(human_size(163_700_000), "163.7 MB");
        assert_eq!(human_size(2_400_000_000), "2.4 GB");
    }

    #[test]
    fn leaf_row_label_includes_size_and_short_date_when_present() {
        let full = leaf(
            "wrf2d_d01_2080-01-01_00:00:00.nc",
            Some(163_700_000),
            Some("2024-03-25T21:34:51.095Z"),
        );
        assert_eq!(
            leaf_row_label(&full),
            "wrf2d_d01_2080-01-01_00:00:00.nc — 163.7 MB — 2024-03-25"
        );
        // Missing size/date degrade gracefully to just the name.
        let bare = leaf("x.nc", None, None);
        assert_eq!(leaf_row_label(&bare), "x.nc");
    }

    #[test]
    fn node_state_machine_expand_load_resolve_and_retry() {
        let mut browser = GdexBrowser::new();
        let url = browser.root_url.clone();

        // Expanding a fresh node marks it Loading and enqueues exactly one fetch.
        browser.expanded.insert(url.clone());
        browser.ensure_load(&url);
        assert_eq!(browser.nodes.get(&url), Some(&NodeLoad::Loading));
        assert_eq!(browser.catalog_queue.len(), 1);
        // A second ensure_load while Loading does not double-enqueue.
        browser.ensure_load(&url);
        assert_eq!(browser.catalog_queue.len(), 1);

        // A successful fetch resolves to Loaded with titled children.
        let parsed = ParsedCatalog {
            catalog_url: url.clone(),
            child_catalog_urls: vec![
                "https://tds.gdex.ucar.edu/thredds/catalog/files/g/d612005/future2D/catalog.xml"
                    .to_owned(),
            ],
            leaves: vec![leaf(
                "wrf2d_d01_2080-01-01_00:00:00.nc",
                Some(1_000_000),
                None,
            )],
        };
        browser.apply_catalog(CatalogFetch {
            catalog_url: url.clone(),
            result: Ok(parsed),
        });
        match browser.nodes.get(&url) {
            Some(NodeLoad::Loaded { children, leaves }) => {
                assert_eq!(children.len(), 1);
                assert_eq!(children[0].title, "future2D");
                assert_eq!(leaves.len(), 1);
            }
            other => panic!("want Loaded, got {other:?}"),
        }
        // ensure_load on a Loaded node is a no-op (no re-enqueue).
        browser.catalog_queue.clear();
        browser.ensure_load(&url);
        assert!(browser.catalog_queue.is_empty());

        // An error resolves to Error; retry resets it to Loading + re-enqueues.
        browser.apply_catalog(CatalogFetch {
            catalog_url: url.clone(),
            result: Err("503".to_owned()),
        });
        assert!(matches!(browser.nodes.get(&url), Some(NodeLoad::Error(_))));
        browser.nodes.insert(url.clone(), NodeLoad::NotLoaded);
        browser.ensure_load(&url);
        assert_eq!(browser.nodes.get(&url), Some(&NodeLoad::Loading));
        assert_eq!(browser.catalog_queue.len(), 1);
    }

    #[test]
    fn subset_form_builds_expected_requests() {
        let mut form = SubsetForm::new(leaf("wrf2d_d01_2080-01-01_00:00:00.nc", None, None));
        // Empty selection is rejected.
        assert!(form.build_subset().is_err());

        // Vars only.
        form.selected.insert("T2".to_owned());
        let subset = form.build_subset().expect("vars-only subset");
        assert_eq!(subset.vars, vec!["T2".to_owned()]);
        assert!(subset.bbox.is_none());
        assert!(subset.time.is_none());

        // Seed from metadata, then bbox + single time.
        let meta = NcssGridDataset {
            variables: vec![NcssVariable {
                name: "T2".to_owned(),
                units: Some("K".to_owned()),
                description: Some("TEMP at 2 M".to_owned()),
            }],
            lat_lon_box: Some(LatLonBox {
                north: 73.2915,
                south: 15.0072,
                east: -40.2612,
                west: -156.8905,
            }),
            time_span: Some(TimeSpan {
                begin: "2080-01-01T00:00:00Z".to_owned(),
                end: "2080-01-01T00:00:00Z".to_owned(),
            }),
        };
        form.seed_from_meta(&meta);
        assert_eq!(form.time, "2080-01-01T00:00:00Z");
        form.use_bbox = true;
        form.use_time = true;
        let subset = form.build_subset().expect("full subset");
        let bbox = subset.bbox.expect("bbox present");
        assert!((bbox.north - 73.2915).abs() < 1e-6);
        assert_eq!(subset.time.as_deref(), Some("2080-01-01T00:00:00Z"));

        // A non-numeric bbox edit is a user-facing error.
        form.north = "abc".to_owned();
        assert!(form.build_subset().is_err());
    }

    #[test]
    fn subset_dest_name_is_distinct_and_ntfs_safe() {
        let mut form = SubsetForm::new(leaf("wrf2d_d01_2080-01-01_00:00:00.nc", None, None));
        form.selected.insert("T2".to_owned());
        let name = form.subset_dest_name();
        assert_eq!(name, "wrf2d_d01_2080-01-01_00_00_00_subset_T2.nc");
        assert!(!name.contains(':'), "colon must be sanitized for NTFS");
    }

    #[test]
    fn resolve_download_imports_success_and_reports_failure() {
        // A real .nc download resolves to an import path with a note.
        let ok = resolve_download(Ok(DownloadOutcome {
            path: PathBuf::from("/cache/wrf2d_d01.nc"),
            bytes: 163_700_000,
            resumed: false,
            cache_hit: false,
        }));
        match ok {
            DownloadResolution::Import { path, note } => {
                assert_eq!(path, PathBuf::from("/cache/wrf2d_d01.nc"));
                assert!(note.contains("importing"));
            }
            other => panic!("want Import, got {other:?}"),
        }

        // A resumed download names the resume in its note.
        let resumed = resolve_download(Ok(DownloadOutcome {
            path: PathBuf::from("/cache/wrf2d_d01.nc"),
            bytes: 10,
            resumed: true,
            cache_hit: false,
        }));
        assert!(
            matches!(resumed, DownloadResolution::Import { note, .. } if note.contains("resumed"))
        );

        // An empty download is not importable.
        let empty = resolve_download(Ok(DownloadOutcome {
            path: PathBuf::from("/cache/wrf2d_d01.nc"),
            bytes: 0,
            resumed: false,
            cache_hit: false,
        }));
        assert!(matches!(empty, DownloadResolution::Done(_)));

        // A non-model file is not imported even when non-empty.
        let wrong = resolve_download(Ok(DownloadOutcome {
            path: PathBuf::from("/cache/notes.txt"),
            bytes: 100,
            resumed: false,
            cache_hit: false,
        }));
        assert!(matches!(wrong, DownloadResolution::Done(_)));

        // A failure is a retryable status.
        let failed = resolve_download(Err("status 503".to_owned()));
        assert!(matches!(failed, DownloadResolution::Failed(msg) if msg.contains("503")));
    }

    #[test]
    fn import_decision_matches_the_importer_and_size() {
        let good = DownloadOutcome {
            path: PathBuf::from("/cache/wrf3d_d01_2080.nc"),
            bytes: 1,
            resumed: false,
            cache_hit: false,
        };
        assert_eq!(
            import_path_for_outcome(&good),
            Some(PathBuf::from("/cache/wrf3d_d01_2080.nc"))
        );
        let empty = DownloadOutcome {
            bytes: 0,
            ..good.clone()
        };
        assert_eq!(import_path_for_outcome(&empty), None);
    }
}
