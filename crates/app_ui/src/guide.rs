//! In-app Guide: a reference the user opens when they want to learn —
//! never a forced tour. Left nav + content pane, all plain egui.
//!
//! Every feature claim in here was verified against the code in
//! `main.rs`/`model_data.rs`/`sat_worker.rs` (v0.8.2), and every science
//! entry carries its primary citation, matching the project convention
//! (docs/products-guide.md, docs/hail-wind-algo-spec.md).

use eframe::egui;

// Same constants the sidebar reads (ui_theme.rs) — the Guide can't drift
// from the chrome it documents.
use crate::ui_theme::ACCENT_COLOR as KEY_COLOR;
use crate::ui_theme::SUBHEAD_COLOR;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GuideSection {
    #[default]
    GettingStarted,
    Products,
    Layers,
    ModelData,
    Satellite,
    Archive,
    Tools,
    Shortcuts,
    Sources,
}

impl GuideSection {
    const ALL: [GuideSection; 9] = [
        Self::GettingStarted,
        Self::Products,
        Self::Layers,
        Self::ModelData,
        Self::Satellite,
        Self::Archive,
        Self::Tools,
        Self::Shortcuts,
        Self::Sources,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::GettingStarted => "Getting started",
            Self::Products => "Products explained",
            Self::Layers => "Layers",
            Self::ModelData => "Model data & soundings",
            Self::Satellite => "Satellite",
            Self::Archive => "Archive & events",
            Self::Tools => "Tools & inspector",
            Self::Shortcuts => "Keyboard shortcuts",
            Self::Sources => "Data sources & credits",
        }
    }
}

/// The Guide window. Pure function of `open`; the selected section lives in
/// egui temp memory so the caller carries no extra state.
pub fn guide_window(ctx: &egui::Context, open: &mut bool) {
    if !*open {
        return;
    }
    let section_id = egui::Id::new("bowecho_guide_section");
    let mut section: GuideSection = ctx.data(|d| d.get_temp(section_id)).unwrap_or_default();
    egui::Window::new("Guide")
        .open(open)
        .default_size([840.0, 600.0])
        .min_size([600.0, 380.0])
        .resizable(true)
        .show(ctx, |ui| {
            egui::Panel::left("guide_nav")
                .resizable(false)
                .exact_size(172.0)
                .show_inside(ui, |ui| {
                    ui.add_space(4.0);
                    for candidate in GuideSection::ALL {
                        if ui
                            .selectable_label(section == candidate, candidate.label())
                            .clicked()
                        {
                            section = candidate;
                        }
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(
                        egui::RichText::new("Reference, not a tour —\nopen it whenever.")
                            .small()
                            .weak(),
                    );
                });
            egui::CentralPanel::default().show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("guide_content")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add_space(2.0);
                        match section {
                            GuideSection::GettingStarted => getting_started(ui),
                            GuideSection::Products => products(ui),
                            GuideSection::Layers => layers(ui),
                            GuideSection::ModelData => model_data(ui),
                            GuideSection::Satellite => satellite(ui),
                            GuideSection::Archive => archive(ui),
                            GuideSection::Tools => tools(ui),
                            GuideSection::Shortcuts => shortcuts(ui),
                            GuideSection::Sources => sources(ui),
                        }
                        ui.add_space(10.0);
                    });
            });
        });
    ctx.data_mut(|d| d.insert_temp(section_id, section));
}

// ---------------------------------------------------------------------------
// Shared building blocks (mirrors the sidebar's visual rhythm).

/// Small uppercase-ish section header — same look as the sidebar's.
fn subhead(ui: &mut egui::Ui, label: &str) {
    ui.add_space(8.0);
    ui.separator();
    ui.label(
        egui::RichText::new(label)
            .small()
            .strong()
            .color(SUBHEAD_COLOR),
    );
    ui.add_space(2.0);
}

/// A wrapped paragraph.
fn para(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(text).wrap());
    ui.add_space(2.0);
}

/// "**Action** — explanation" bullet; the bold lead is the thing to click/press.
fn action(ui: &mut egui::Ui, lead: &str, rest: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        ui.strong(lead);
        ui.label(rest);
    });
    ui.add_space(2.0);
}

/// Research citation line — small, weak, italic.
fn cite(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).small().weak().italics()).wrap());
}

/// Key binding row: colored monospace keycap + what it does.
fn key_row(ui: &mut egui::Ui, key: &str, what: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.label(
            egui::RichText::new(format!("{key:<18}"))
                .monospace()
                .color(KEY_COLOR),
        );
        ui.label(what);
    });
}

/// One product entry: collapsed by default so the list stays scannable.
fn product_entry(ui: &mut egui::Ui, title: &str, shows: &str, read_it: &str, citation: &str) {
    egui::CollapsingHeader::new(title)
        .default_open(false)
        .show(ui, |ui| {
            action(ui, "Shows:", shows);
            if !read_it.is_empty() {
                action(ui, "Reading it:", read_it);
            }
            if !citation.is_empty() {
                cite(ui, citation);
            }
        });
}

// ---------------------------------------------------------------------------
// 1. Getting started

fn getting_started(ui: &mut egui::Ui) {
    ui.heading("Getting started");
    para(
        ui,
        "BowEcho is the radar map plus the sidebar on the right. The sidebar has five tabs: \
         Radar (site, products, tilts, loop, algorithms, tools — live operations), Layers \
         (everything drawn over the map, one uniform list), Severe (NWS warning polygons + \
         SPC outlooks and reports), Data (archive days, live poll feeds, the model store, \
         local files), and \u{2699} Settings (display, color tables, hotkeys, performance). \
         Collapsible sections remember whether you left them open.",
    );
    para(
        ui,
        "The top bar holds one-shot actions on the left (Reset View, Reload, Screenshot, \
         Annotate) and, on the right, the Windows \u{25be} menu — every data window (Model, \
         Satellite, WoFS, FARM, 3D Volume, Sounding) opens from there — plus this Guide. \
         Status chips (a green FARM LIVE chip when a mobile radar is plotting, an \
         update-available notice) appear beside the menus. Tab hides all chrome for a clean \
         capture; Tab or Esc brings it back.",
    );

    subhead(ui, "PICK A RADAR");
    action(
        ui,
        "Site dropdown",
        "— Radar tab \u{25b8} SITE. Pick a site, then Load Latest (newest volume) or \
         Load Loop (recent history). Center recenters the map on it. The site you load is \
         remembered as the startup site for next launch.",
    );
    action(
        ui,
        "Right-click the map",
        "— opens \"Lowest beam here\": the three WSR-88Ds whose 0.5° beam is lowest over that \
         point (4/3-Earth geometry), with beam height and distance. One click switches there \
         and loads the latest volume. Right-clicking also jumps to the nearest site directly.",
    );
    action(
        ui,
        "Click a site marker",
        "— selects that site without loading anything.",
    );

    subhead(ui, "GO LIVE");
    action(
        ui,
        "Live",
        "— tick it in SITE to auto-refresh from the real-time chunk feed. With Chunks on, \
         partial tilts draw as they arrive instead of waiting for a complete low tilt.",
    );
    para(
        ui,
        "The chip on the canvas always reads LIVE / ARCHIVE / STALE — you can never mistake an \
         old frame for live data.",
    );

    subhead(ui, "PRODUCTS, TILT, LOOP");
    action(
        ui,
        "PRODUCTS grid",
        "— every moment and derived product the loaded volume supports. Buttons are prefixed \
         with their number-key hotkey; \u{2190}/\u{2192} also step through products.",
    );
    action(
        ui,
        "TILT list",
        "— each elevation cut with its angle, radial count, and scan time; \u{2191}/\u{2193} \
         step it. Tilts that can't show the selected product are greyed.",
    );
    action(
        ui,
        "LOOP",
        "— after Load Loop: play/pause, step buttons, a scrub slider, and the frame cap \
         (3\u{2013}30 frames).",
    );

    subhead(ui, "PANES & THE MAP");
    action(
        ui,
        "Panes 1 / 2 / 4",
        "— synced multi-pane grids (shared pan/zoom/tilt, independent product per pane; the \
         quad defaults to REF / VEL / CC / ZDR). Click a pane to focus it — the sidebar and \
         arrow keys then edit that pane; the main (top-left) pane edits everything.",
    );
    para(
        ui,
        "Drag pans, the scroll wheel zooms about the cursor, Reset View (top bar) recenters on \
         the site. The colorbar for the active product draws on-canvas.",
    );
}

// ---------------------------------------------------------------------------
// 2. Products explained

fn products(ui: &mut egui::Ui) {
    ui.heading("Products explained");
    para(
        ui,
        "What each product shows, how to read it, and the research it comes from. Threshold \
         numbers are guidance from the cited literature — context beats any single number.",
    );

    subhead(ui, "BASE & DUAL-POL MOMENTS");
    product_entry(
        ui,
        "REF — Reflectivity (dBZ)",
        "precipitation intensity. The default Analyst Reflectivity HD palette keeps magenta \
         for \u{2265}65 dBZ (hail cores); more palettes in Settings \u{25b8} Color tables.",
        "the workhorse. Watch for bright-banding near the melting layer and ground clutter \
         close to the radar.",
        "",
    );
    product_entry(
        ui,
        "VEL — Radial velocity (m/s)",
        "wind toward the radar (negative, green side) or away (positive, red side) — along \
         the beam only; flow across the beam is invisible.",
        "keep Unfold VEL on. Raw velocity folds at the Nyquist speed and a folded gate reads \
         as a fake opposite-direction couplet — the inspector warns on near-Nyquist gates. \
         Unfolding uses a region-based dealiaser; a tilt-cascade engine (beta) is selectable \
         for VCPs with high-Nyquist upper tilts.",
        "Dealiasing: Jing & Wiener 1993 (JTECH 10); Feldmann et al. 2020, R2D2 \
         (JTECH-D-20-0054.1); Helmus & Collis 2016 (Py-ART).",
    );
    product_entry(
        ui,
        "DVEL — Dealiased velocity",
        "the continuity-unfolded velocity field as its own product — what the shear and wind \
         algorithms consume.",
        "",
        "",
    );
    product_entry(
        ui,
        "SRV / DSRV — Storm-relative velocity",
        "velocity with storm motion subtracted (raw / dealiased), so rotation stands out \
         inside a moving storm.",
        "set Motion in the sidebar, or click \u{2190}tracks to take the storm tracker's mean \
         fitted motion (SCIT's default-motion idea).",
        "Johnson et al. 1998, Wea. Forecasting 13, 263\u{2013}276 (SCIT).",
    );
    product_entry(
        ui,
        "SW — Spectrum width",
        "velocity spread within a gate: turbulence, shear, boundaries.",
        "high SW co-located with a couplet adds confidence in rotation; uniformly high SW \
         means a noisy estimate.",
        "",
    );
    product_entry(
        ui,
        "ZDR — Differential reflectivity (dB)",
        "drop shape: oblate rain reads positive; tumbling hail reads near zero.",
        "ZDR columns mark updrafts; near-zero ZDR inside a high-REF core suggests hail.",
        "",
    );
    product_entry(
        ui,
        "RHO (CC) — Correlation coefficient",
        "how uniform the scatterers are: rain/snow near 1.0, mixtures lower, non-met \
         (birds, chaff, debris) much lower.",
        "a compact CC hole inside high reflectivity, co-located with a tight velocity \
         couplet, is a tornadic debris signature (TDS). The Analyst CC palette packs its \
         resolution into 0.80\u{2013}1.00 where the meteorology lives.",
        "",
    );
    product_entry(
        ui,
        "PHI — Differential phase (°)",
        "accumulated phase difference along the beam — the raw field KDP is derived from.",
        "mostly diagnostic; trends matter more than values.",
        "",
    );
    product_entry(
        ui,
        "KDP — Specific differential phase (°/km)",
        "liquid-water content along the beam (when present in the volume).",
        "warm KDP = heavy rain loading; KDP stays low in pure hail.",
        "",
    );

    subhead(ui, "DERIVED PRODUCTS");
    para(
        ui,
        "Computed from the whole volume and drawn on the lowest tilt (CREF, ET, VIL, VILD, \
         MEHS, POSH, POH, MARC, Gust) or computed per-tilt (AzShr, Div). All are in the \
         product grid whenever their source moment exists.",
    );
    product_entry(
        ui,
        "CREF — Composite reflectivity (dBZ)",
        "column-maximum reflectivity over all tilts — the fullest picture of cores, including \
         elevated ones the lowest beam misses.",
        "great for situational awareness; it deliberately hides vertical structure, so check \
         tilts or a cross-section before calling a core surface-based.",
        "NWS composite-reflectivity (NCR) heritage.",
    );
    product_entry(
        ui,
        "ET — Echo tops (m, palette labeled in kft)",
        "height of the highest beam with \u{2265}18.3 dBZ, 4/3-Earth beam geometry.",
        "reads low in the cone of silence right over the radar (no high tilts), and gets \
         coarse far out where tilt gaps widen.",
        "NWS echo-tops convention; beam height per Doviak & Zrni\u{107} 1993, eq. 2.28b.",
    );
    product_entry(
        ui,
        "VIL — Vertically integrated liquid (kg/m²)",
        "the liquid-water column integral with the 56 dBZ hail cap, lowest beam extended to \
         the ground.",
        "traces convective cores and bows well; raw values depend on storm depth — VILD \
         normalizes that.",
        "Greene & Clark 1972, Mon. Wea. Rev. 100, 548\u{2013}552; hail cap per Witt et al. \
         1998.",
    );
    product_entry(
        ui,
        "VILD — VIL density (g/m³)",
        "VIL divided by echo-top depth — hail potential normalized by storm depth.",
        "\u{2273}3.5 g/m³ flags large-hail candidates; more size-selective than raw VIL.",
        "Amburn & Wolf 1997, Wea. Forecasting 12, 473\u{2013}478.",
    );
    product_entry(
        ui,
        "MEHS — Maximum expected hail size (mm)",
        "the WSR-88D Hail Detection Algorithm: hail kinetic-energy flux (40\u{2013}50 dBZ \
         ramp) weighted between the 0°C and \u{2212}20°C levels and integrated into the \
         Severe Hail Index, then sized with Witt's calibration MESH = 2.54\u{b7}\u{221a}SHI \
         (the calibration operational MRMS still ships).",
        "MEHS \u{2265} 29 mm is the climatological severe (1 in.) threshold. Set the \
         0°C/\u{2212}20°C heights first — the From HRRR button samples the model profile at \
         the radar site; MESH is sensitive to them. A 75th-percentile fit by design: it \
         underestimates giant hail.",
        "Witt et al. 1998, Wea. Forecasting 13, 286\u{2013}303; severe threshold: Cintineo \
         et al. 2012, Wea. Forecasting 27, 1235\u{2013}1248; recalibrations: Murillo & \
         Homeyer 2019, JAMC 58 + 2021 corrigendum (JAMC 60(3)).",
    );
    product_entry(
        ui,
        "POSH — Probability of severe hail (%)",
        "SHI measured against a melting-level-dependent warning threshold; exactly 50% when \
         SHI equals the threshold.",
        "read it to the nearest 10% — that's how it was designed. Same environment-height \
         sensitivity as MEHS; low melting levels (winter) over-detect.",
        "Witt et al. 1998, Wea. Forecasting 13, 286\u{2013}303.",
    );
    product_entry(
        ui,
        "POH — Probability of hail, any size (%)",
        "how far the 45 dBZ echo top extends above the 0°C level, mapped through the \
         hailpad-validated Waldvogel curve.",
        "POH says hail of some size aloft is likely — pair with MEHS/VILD for severity.",
        "Waldvogel, Federer & Grimm 1979, J. Appl. Meteor. 18, 1521\u{2013}1525.",
    );
    product_entry(
        ui,
        "MARC — Mid-altitude radial convergence (m/s)",
        "the max inbound-vs-outbound \u{394}V within 6 km along a single radial, composited \
         over the 3\u{2013}7 km layer — the classic bow-echo / QLCS damaging-wind precursor \
         (deep convergence marks the rear-inflow jet before it descends).",
        "\u{394}V \u{2265} 25 m/s (50 kt), persistent and deep-layered, precedes damaging \
         surface winds by 15\u{2013}20 min (\u{2248}38 m/s observed in the 14 May 1995 \
         Kentucky bow echo). Caveat: masked where mid-level flow runs normal to the beam — \
         a precursor aid, not truth. Values above 70 m/s are rejected as dealias artifacts.",
        "Schmocker, Przybylinski & Lin 1996, 15th Conf. Wea. Analysis & Forecasting, \
         306\u{2013}311; Przybylinski 1995, Wea. Forecasting 10, 203\u{2013}218.",
    );
    product_entry(
        ui,
        "Gust — Low-level gust proxy (m/s)",
        "|dealiased radial wind| on the lowest velocity tilt, only where the beam center is \
         below 1 km — NWS research practice maps low-beam radial wind \u{2248}1:1 to surface \
         gusts.",
        "\u{2265} 25 m/s \u{2248} severe (50 kt) gust equivalence. Treat it as a floor: \
         microburst outflow peaks below the beam (Hjelmfelt 1988). Gates without \u{2265}10 \
         dBZ reflectivity support are masked (clear-air biota would otherwise fabricate \
         gusts), and the product honestly stops at the range where the beam tops 1 km.",
        "Smith, Elmore & Dulin 2004, Wea. Forecasting 19, 240\u{2013}250; Hjelmfelt 1988, \
         J. Appl. Meteor. 27, 900\u{2013}927.",
    );
    product_entry(
        ui,
        "AzShr — Azimuthal shear (\u{d7}10\u{207b}\u{b3}/s)",
        "local linear-least-squares derivative of dealiased velocity across the radial — \
         rotation strength without eyeballing couplets. Warm = cyclonic.",
        "computed on the selected tilt; noise grows with range as the beam broadens, so \
         confirm distant signatures on more than one tilt or volume.",
        "Smith & Elmore 2004 (11th Conf. ARAM, P5.6); Mahalik et al. 2019, Wea. Forecasting \
         34, 415\u{2013}434 (LLSD).",
    );
    product_entry(
        ui,
        "Div — Radial divergence (\u{d7}10\u{207b}\u{b3}/s)",
        "the same LLSD derivative along the radial: gust-front convergence reads cool, \
         downburst/outflow divergence reads warm.",
        "a divergence bullseye at the lowest tilt under a collapsing core is a downburst \
         signature.",
        "Smith & Elmore 2004 (11th Conf. ARAM, P5.6).",
    );

    subhead(ui, "DISPLAY AIDS");
    para(
        ui,
        "Gate filter hides velocity/dual-pol gates whose same-tilt reflectivity is weak (the \
         standard VEL declutter; REF itself is never filtered). \"Hide below\" is a per-family \
         render threshold — data stays intact and the inspector still reads it. Smooth display \
         (Settings) applies a GR2-style binomial kernel once per product, so pans stay fast.",
    );
}

// ---------------------------------------------------------------------------
// 2b. Layers — the rail

fn layers(ui: &mut egui::Ui) {
    ui.heading("Layers");
    para(
        ui,
        "The Layers tab is one uniform list of everything drawn over the map: the primary \
         radar, overlay radars, rotation tracks + TDS, GOES, model and mesoanalysis fields, \
         WoFS and FARM drapes, surface obs, lightning, SPC outlooks and reports, warning \
         polygons, and placefiles. Layers draw bottom-to-top in list order.",
    );

    subhead(ui, "THE ROW");
    para(
        ui,
        "Every layer wears the same row: a visibility checkbox, the name (hover it for \
         details), a state dot where the layer has a lifecycle (live / loading / paused), an \
         opacity slider where the layer has one, \u{2191}/\u{2193} reorder buttons where \
         order matters (model fields), then the row's one or two earned inline extras, a \
         \u{2699} gear, and \u{2715} remove.",
    );
    action(
        ui,
        "\u{2699} gear",
        "— opens the layer's owning surface: the Model/Satellite/WoFS/FARM window for window \
         layers, the Severe tab for SPC and warnings, or a small popover for layers with \
         only a few options (surface-obs networks, lightning). Appearance controls land in \
         these popovers next.",
    );
    action(
        ui,
        "+ Add layer \u{25be}",
        "— the single front door for every map data type: radar overlays, model fields, \
         satellite, WoFS/FARM drapes, mesoanalysis composites, surface obs, placefiles. You \
         never need to know which window a layer is born in.",
    );

    subhead(ui, "ANALYSIS (OA)");
    para(
        ui,
        "At the bottom of the tab: compute that EMITS layers. Analyze obs runs a Bratseth \
         objective analysis of the model surface field against live obs; Compute composites \
         builds the full SPC mesoanalysis suite (SCP, STP, SHIP, EHI, …) — each field then \
         adds as an instant \"(OA)\" layer, also reachable from + Add layer \u{25b8} \
         Mesoanalysis (OA).",
    );
    cite(
        ui,
        "Bratseth 1986 (Tellus 38A); Bothwell et al. 2002 (SPC mesoanalysis); ADAS weights.",
    );

    subhead(ui, "WHERE THE OLD TOGGLES WENT");
    para(
        ui,
        "Everything that used to hide in the Radar tab's Layers fold lives here now. The \
         Radar tab keeps a one-line \"Layers: N \u{2192}\" link; Poll-URL feeds moved to the \
         Data tab (they replace the volume source — acquisition, not a layer); SPC \
         day/kind config moved to the Severe tab.",
    );
}

// ---------------------------------------------------------------------------
// 3. Model data & soundings

fn model_data(ui: &mut egui::Ui) {
    ui.heading("Model data & soundings");
    para(
        ui,
        "HRRR fields and skew-T soundings, layered straight onto the radar map. Enable the \
         master switch first: \u{2699} Settings \u{25b8} Model \u{25b8} Model data (off = \
         pure radar app). Windows \u{25be} \u{25b8} Model data opens the Model window.",
    );

    subhead(ui, "GETTING DATA");
    action(
        ui,
        "Fetch latest",
        "(Layers row) — ingests the freshest HRRR init, next 3 forecast hours, sounding-grade \
         profile, then prunes the store to the newest runs. About a minute, throttled below \
         the UI so frames never stutter.",
    );
    action(
        ui,
        "Download…",
        "— the full window: any init date/cycle, an hours spec (\"0-3\" or \"2,4,6\"), profile \
         choice, with a live size estimate before you commit. Other models are listed but \
         disabled until ingest supports them.",
    );
    action(
        ui,
        "Keep runs",
        "— store retention; the newest N runs survive each fetch and startup (default 2, \
         \u{2248}1.5 GB on disk).",
    );

    subhead(ui, "THE MODEL WINDOW & MAP LAYER");
    para(
        ui,
        "Runs tree on the left, field viewer in the middle. Show on radar map renders the \
         selected field as a layer under the radar.",
    );
    action(
        ui,
        "Layer row in Layers",
        "— the checkbox hides the layer without losing it (a hidden layer still feeds the \
         inspector and Alt+click soundings); \u{25c0} \u{25b6} step the forecast hour; the \
         slider sets layer opacity; \u{2715} removes it. The Radar opacity slider above lets \
         the model field show through the radar.",
    );

    subhead(ui, "SOUNDINGS");
    action(
        ui,
        "Alt+click",
        "anywhere on the map — a native skew-T for that exact point opens in the Sounding \
         window, computed from the HRRR profile.",
    );
    action(
        ui,
        "Ctrl+Alt + move the mouse",
        "— FOLLOW MODE: the skew-T streams live under the cursor. No buttons are involved, so \
         the map never pans; requests coalesce (latest wins) while each profile computes in \
         ~100 ms. Sweep across a front or dryline and watch the profile transform — the \
         fastest way to feel an environment gradient.",
    );
    para(
        ui,
        "Both need Model data enabled and at least one ingested run on disk.",
    );

    subhead(ui, "HAIL LEVELS FROM HRRR");
    action(
        ui,
        "From HRRR",
        "— with MEHS, POSH, or POH selected, the \"Hail 0°C/\u{2212}20°C\" row appears under \
         PRODUCTS. From HRRR samples the model temperature profile at the radar site and sets \
         both crossing heights — the same environmental inputs MRMS takes from model \
         analyses. The hail products are sensitive to these; use it instead of the defaults \
         whenever model data is loaded.",
    );

    subhead(ui, "INSPECTOR");
    para(
        ui,
        "With \"Model value\" ticked in Inspector\u{2026}, the cursor card reads the HRRR \
         field under the cursor — even while the map layer is hidden.",
    );
}

// ---------------------------------------------------------------------------
// 4. Satellite

fn satellite(ui: &mut egui::Ui) {
    ui.heading("Satellite");
    para(
        ui,
        "Windows \u{25be} \u{25b8} Satellite opens the GOES window: a live follow engine on \
         top, a frame player below.",
    );

    subhead(ui, "LIVE FOLLOW");
    para(
        ui,
        "Pick the satellite (GOES-19 East, GOES-18 West, GOES-16), sector (CONUS, Full disk, \
         Meso 1, Meso 2), and any of the 16 ABI bands. Start polls the NOAA open-data bucket \
         at the sector's native cadence and keeps a rolling local store. BowEcho uses its own \
         sat store, so it can run alongside other tools without corrupting their caches.",
    );

    subhead(ui, "FRAME PLAYER");
    para(
        ui,
        "Browse the fetched runs and step or play through frames below the follow panel; the \
         refresh control re-scans the store for frames written since.",
    );

    subhead(ui, "SHOW ON RADAR MAP");
    action(
        ui,
        "Show on radar map",
        "— puts the current frame under the radar as a map layer. Opacity and removal live in \
         the Layers tab's GOES row.",
    );

    subhead(ui, "BAND PICKS");
    para(
        ui,
        "Red 0.64 µm for daytime detail; Clean IR Window 10.3 µm for cloud tops day or night \
         (overshooting tops above warning-grade updrafts); the 6.2/6.9/7.3 µm water-vapor \
         trio for mid/upper-level moisture and jet structure.",
    );
}

// ---------------------------------------------------------------------------
// 5. Archive & events

fn archive(ui: &mut egui::Ui) {
    ui.heading("Archive & events");
    para(
        ui,
        "The Data tab's Archive section replays any day in the Level II record for the \
         selected site — the loop transport sits at the top of the tab so you never switch \
         tabs to play what you just loaded. Data also holds Live feeds (GR2A-style dir.list \
         polling for research/mobile radars), the Model store summary, and local file/folder \
         openers.",
    );

    subhead(ui, "BROWSING A DAY");
    action(
        ui,
        "Date row",
        "— a UTC date (YYYY-MM-DD) with \u{25c0} \u{25b6} day steps and a Today button; \
         stepping re-lists immediately. List fetches every volume for that date, grouped by \
         hour — click a minute chip to load it.",
    );
    action(
        ui,
        "On click: Loop / Single",
        "— Loop loads a loop of volumes ending at the chosen scan (count set by Frames, \
         1\u{2013}30); Single loads just that scan. +5 earlier extends a loaded loop further \
         back in time.",
    );

    subhead(ui, "SPC TORNADO EVENTS");
    action(
        ui,
        "Tornadoes (SPC) \u{25b8} Fetch",
        "— pulls the SPC filtered tornado reports for the date (SPC's 12Z\u{2013}12Z \
         convective day). Each report shows time, EF rating when rated, and location.",
    );
    action(
        ui,
        "Click a report",
        "— BowEcho picks the radar with the lowest beam over the report location, centers the \
         map there, and loads the loop at the report time. One click from \"tornado near X\" \
         to the right radar at the right minute.",
    );
}

// ---------------------------------------------------------------------------
// 6. Tools & inspector

fn tools(ui: &mut egui::Ui) {
    ui.heading("Tools & inspector");

    subhead(ui, "INSPECTOR CARD");
    para(
        ui,
        "The floating card at the cursor reads the data under it: product value with units; \
         on velocity products an in/outbound arrow at the probed gate plus an automatic \
         couplet probe (Vrot, \u{394}V, separation) when one is nearby; raw VEL with the \
         Nyquist and a fold warning; range @ azimuth and tilt; beam height; and the HRRR \
         value. Pick exactly which lines you want via the Inspector\u{2026} menu in TOOLS.",
    );
    action(
        ui,
        "Shift+click",
        "— pins the card to a geo point: it sticks through pan/zoom and re-reads every new \
         volume (watch one spot evolve through a loop). Shift+click near the pin releases it. \
         In grid layouts the pin works on the main pane.",
    );

    subhead(ui, "BEST RADAR");
    action(
        ui,
        "Right-click the map",
        "— the \"Lowest beam here\" menu: the three WSR-88Ds with the lowest 0.5° beam over \
         that point, each with beam height (kft) and distance; click to switch and load. \
         Right-clicking also jumps to the nearest site directly.",
    );
    action(
        ui,
        "Ctrl+right-click",
        "— adds the nearest radar as an extra overlay layer instead of switching, for \
         multi-radar mosaics. Manage overlays (opacity, refresh, promote, remove) in Layers.",
    );

    subhead(ui, "VROT TOOL");
    action(
        ui,
        "Vrot tool",
        "(TOOLS) — arm it, then on a velocity product click the max inbound gate, then the \
         max outbound gate of a couplet. The readout is Vrot = (|Vin| + |Vout|) / 2 in kt, \
         couplet diameter in nm, and beam height in kft — the numbers a warning desk reads. \
         Right-click clears; the connecting line doubles as a two-point distance measure.",
    );

    subhead(ui, "CROSS-SECTION");
    action(
        ui,
        "Cross-section",
        "(TOOLS) — arm it, click endpoint A then B on the map: a vertical slice opens in a \
         bottom panel (heights to 18 km, 4/3-Earth beam geometry). Velocity products slice \
         velocity; everything else slices reflectivity. Right-click resets the endpoints; \
         Clear XS removes the panel.",
    );

    subhead(ui, "ALGORITHM OVERLAYS");
    action(
        ui,
        "Rotation markers",
        "— MDA/TDA-style circulation detection on a background thread: pale ring = weak, \
         orange = moderate, double gold = mesocyclone, red triangle = TVS; zoom in for rank \
         and Vrot.",
    );
    cite(
        ui,
        "Stumpf et al. 1998, Wea. Forecasting 13, 304\u{2013}326 (MDA); Mitchell et al. \
         1998, Wea. Forecasting 13, 352\u{2013}366 (TDA).",
    );
    action(
        ui,
        "Rotation tracks",
        "— per-pixel MAXIMUM low-level (0\u{2013}2 km) cyclonic azimuthal shear accumulated \
         across the loaded loop: the swath a translating mesocyclone paints. Transparent \
         below 0.003 s\u{207b}\u{00b9}, magenta at 0.02 s\u{207b}\u{00b9}; scrubbing shows \
         the accumulation up to the viewed frame; Reset restarts at the newest frame.",
    );
    cite(
        ui,
        "Mahalik et al. 2019, Wea. Forecasting 34, 1423\u{2013}1447 (LLSD azimuthal shear); \
         Miller et al. 2013, 28th Conf. IIPS (rotation tracks); Smith et al. 2016, BAMS 97, \
         1617\u{2013}1630 (MRMS).",
    );
    action(
        ui,
        "TDS flag",
        "— tornado debris signature, a deterministic dual-pol physics flag (never a \
         probability): \u{03c1}hv < 0.82 inside > 30 dBZ echo within 5 km of a rank \u{2265} 3 \
         circulation at the lowest tilt. White/magenta gates at the viewed frame; the magenta \
         trail is the debris track across the loop.",
    );
    cite(
        ui,
        "Ryzhkov et al. 2005, J. Appl. Meteor. 44, 557\u{2013}570; Van Den Broeke & Jauernic \
         2014, J. Appl. Meteor. Climatol. 53, 2217\u{2013}2231; Snyder & Ryzhkov 2015, \
         J. Appl. Meteor. Climatol. 54, 1861\u{2013}1870.",
    );
    action(
        ui,
        "Storm tracks",
        "— SCIT-style cell identification and tracking with a least-squares motion fit; dots \
         extrapolate +15/+30/+45 min. SRV\u{2190}tracks feeds the fitted motion into the \
         storm-relative products.",
    );
    cite(
        ui,
        "Johnson et al. 1998, Wea. Forecasting 13, 263\u{2013}276 (SCIT).",
    );
}

// ---------------------------------------------------------------------------
// 7. Keyboard shortcuts

fn shortcuts(ui: &mut egui::Ui) {
    ui.heading("Keyboard shortcuts");
    para(
        ui,
        "The complete list — there are deliberately few. Keys are ignored while a text box \
         has focus, and in grid layouts they act on the focused (last-clicked) pane.",
    );

    subhead(ui, "KEYS");
    key_row(ui, "\u{2190} / \u{2192}", "previous / next product");
    key_row(ui, "\u{2191} / \u{2193}", "step up / down the tilt list");
    key_row(
        ui,
        "1 \u{2026} 9, 0",
        "product hotkeys — defaults: 1 REF · 2 VEL · 3 SRV · 4 RHO · 5 ZDR · 6 SW · \
         7 CREF · 8 ET · 9 VIL · 0 VILD",
    );
    key_row(
        ui,
        "F12",
        "screenshot — full window to the clipboard + a PNG in Pictures/BowEcho",
    );
    key_row(ui, "Shift+F12", "screenshot cropped to the map only");
    key_row(
        ui,
        "Tab",
        "clean screen — hide all toolbars/panels for pure-radar captures (Tab or Esc restores)",
    );
    para(
        ui,
        "Rebind the number row in config.json — Settings \u{25b8} Hotkeys shows the current \
         map and the file path. Product buttons display their assigned key.",
    );

    subhead(ui, "MOUSE");
    key_row(ui, "drag", "pan (all panes stay in sync)");
    key_row(ui, "scroll", "zoom about the cursor");
    key_row(ui, "click", "select a site marker or warning polygon");
    key_row(
        ui,
        "right-click",
        "\"Lowest beam here\" menu + jump to the nearest site",
    );
    key_row(
        ui,
        "Ctrl+right-click",
        "add the nearest radar as an overlay layer",
    );
    key_row(ui, "Shift+click", "pin / release the inspector card");
    key_row(
        ui,
        "Alt+click",
        "skew-T sounding at that point (model data enabled)",
    );
    key_row(
        ui,
        "Ctrl+Alt+hover",
        "follow mode — streaming skew-T under the cursor",
    );
    key_row(
        ui,
        "armed tools",
        "cross-section / Vrot own the clicks: left places points, right clears",
    );
    key_row(
        ui,
        "Annotate mode",
        "click drops a crosshair; box/arrow/freehand are drags; Esc exits, \
         Clear wipes — annotations are geo-anchored and show up in \
         screenshots and recordings",
    );
}

// ---------------------------------------------------------------------------
// 8. Data sources & credits

fn sources(ui: &mut egui::Ui) {
    ui.heading("Data sources & credits");

    subhead(ui, "RADAR");
    para(
        ui,
        "NEXRAD Level II from Unidata's AWS Open Data buckets — unidata-nexrad-level2 \
         (archive) and unidata-nexrad-level2-chunks (real-time chunks). No keys, no \
         accounts. The site directory comes from api.weather.gov/radar/stations.",
    );

    subhead(ui, "HAZARDS & REPORTS");
    para(
        ui,
        "Warnings: NWS active alerts (api.weather.gov) plus hot NWS text products and SPC \
         mesoscale discussions. Tornado events: SPC storm-report CSVs \
         (spc.noaa.gov/climo/reports).",
    );

    subhead(ui, "MODEL & SATELLITE");
    para(
        ui,
        "HRRR (NOAA High-Resolution Rapid Refresh) ingested into a local store by the \
         rusty-weather stack (rw-ingest / rw-ui); the native skew-T is verified against \
         sharprs. GOES-16/18/19 ABI imagery from NOAA open-data buckets via rw-sat.",
    );

    subhead(ui, "BASEMAPS");
    para(
        ui,
        "The default dark vector basemap is built in (offline). Tile styles: imagery © Esri, \
         Maxar, Earthstar Geographics; Streets/Topo map tiles © Esri and contributors.",
    );

    subhead(ui, "RESEARCH");
    para(
        ui,
        "BowEcho cites its science. The algorithms in this app come from:",
    );
    for citation in [
        "Witt, Eilts, Stumpf, Johnson, Mitchell & Thomas 1998: An Enhanced Hail Detection \
         Algorithm for the WSR-88D. Wea. Forecasting 13, 286\u{2013}303 — SHI / MESH / POSH, \
         VIL hail cap.",
        "Murillo & Homeyer 2019, JAMC 58, 947\u{2013}970, with the 2021 corrigendum (JAMC \
         60(3)) — MESH recalibrations.",
        "Cintineo, Smith, Lakshmanan, Brooks & Ortega 2012, Wea. Forecasting 27, \
         1235\u{2013}1248 — the \u{2265}29 mm severe-MESH climatology threshold.",
        "Waldvogel, Federer & Grimm 1979, J. Appl. Meteor. 18, 1521\u{2013}1525 — POH.",
        "Greene & Clark 1972, Mon. Wea. Rev. 100, 548\u{2013}552 — vertically integrated \
         liquid.",
        "Amburn & Wolf 1997, Wea. Forecasting 12, 473\u{2013}478 — VIL density.",
        "Schmocker, Przybylinski & Lin 1996, 15th Conf. Wea. Analysis & Forecasting, \
         306\u{2013}311 — MARC; Przybylinski 1995, Wea. Forecasting 10, 203\u{2013}218 — \
         bow-echo review.",
        "Smith, Elmore & Dulin 2004, Wea. Forecasting 19, 240\u{2013}250 — low-altitude \
         severe-gust equivalence; Hjelmfelt 1988, J. Appl. Meteor. 27, 900\u{2013}927 — \
         microburst outflow structure.",
        "Smith & Elmore 2004, 11th Conf. ARAM, P5.6; Mahalik et al. 2019, Wea. Forecasting \
         34, 415\u{2013}434 — LLSD shear/divergence.",
        "Jing & Wiener 1993, JTECH 10; Feldmann et al. 2020 (R2D2, JTECH-D-20-0054.1); \
         Helmus & Collis 2016 (Py-ART, JORS) — region-based velocity dealiasing.",
        "Johnson et al. 1998, Wea. Forecasting 13, 263\u{2013}276 — SCIT storm tracking.",
        "Stumpf et al. 1998, Wea. Forecasting 13, 304\u{2013}326 — mesocyclone detection; \
         Mitchell et al. 1998, Wea. Forecasting 13, 352\u{2013}366 — TVS detection.",
        "Doviak & Zrni\u{107} 1993: Doppler Radar and Weather Observations — 4/3-Earth beam \
         height (eq. 2.28b).",
        "Thyng et al. 2016 (cmocean) and Kovesi 2015 — the CVD-safe Balance VEL palette.",
    ] {
        cite(ui, citation);
        ui.add_space(2.0);
    }
    ui.add_space(4.0);
    para(
        ui,
        "Deeper write-ups live in the repo: docs/products-guide.md and \
         docs/hail-wind-algo-spec.md.",
    );
}
