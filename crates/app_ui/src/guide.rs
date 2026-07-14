//! In-app Guide: a reference the user opens when they want to learn —
//! never a forced tour. Left nav + content pane, all plain egui.
//!
//! Feature claims in this reference track the current BowEcho workspaces and
//! their owning modules (`main.rs`, `model_data.rs`, `formula_lab.rs`,
//! `wrf_radar.rs`, `simsat_ui.rs`, and `sat_worker.rs`). Science entries keep
//! the implementation's honesty boundaries visible and point to the deeper
//! repository references where appropriate.

use eframe::egui;

// Same constants the sidebar reads (ui_theme.rs) — the Guide can't drift
// from the chrome it documents.
use crate::ui_theme::{accent_color, subhead_color};

const GUIDE_TOP_BAR_TEXT: &str = "The top bar leads with LIVE plus one Timeline menu: \
    LIVE follows the current radar's newest data and backfills a loop in one click. \
    Timeline brings archive-loop loading, archive-day browsing, sweep playback, and \
    the full Unified Player into one place; live and archive entry both arm synced \
    warnings by default. Reload, Screenshot, Annotate, and Workflows remain one click \
    away. View contains Reset map view and Map only. \
    On the right, the Windows menu opens every data workspace (Models, WRF, \
    CM1, Formula Lab, Algorithm Truth Lab, Radar overlays, Satellite, SimSat, WoFS, \
    FARM, 3D Volume, Sounding) beside this Guide. Status chips appear \
    beside the menus. Map Only hides chrome for a clean capture; Tab or Esc \
    brings it back.";
const GUIDE_PANES_LABEL: &str = "Panes 1 / 2 / 3 / 4";
const GUIDE_PANES_TEXT: &str = "— synced multi-pane grids (shared pan/zoom/tilt, \
    independent product per pane; three-pane is REF / DVEL / CC for warning \
    review, and quad defaults to REF / VEL / CC / ZDR). Click a pane to focus it \
    — the sidebar and arrow keys then edit that pane; the main (top-left) pane \
    edits everything.";
const GUIDE_DEBUG_CASES_TEXT: &str = "— one-click repro launchers for known radar \
    issues. The built-ins include KBMX Tuscaloosa 2011 22:15Z and 22:19Z DVEL \
    scans, and they use the same archive loader as normal case review.";
const GUIDE_CUSTOM_LAYER_ROW_LABEL: &str = "Layer row in Map";
const GUIDE_CUSTOM_OVERLAY_TEXT: &str = "add the nearest radar — US or international, whichever is closer — as an overlay layer. Manage overlays (opacity, refresh, promote, remove) in Map.";

/// Every equation printed as a worked Formula Lab example. Keeping these in
/// one list lets the guide test compile them against the pinned engine instead
/// of allowing documentation syntax to drift.
const GUIDE_FORMULA_EXAMPLES: [&str; 8] = [
    "sqrt(u_10m^2 + v_10m^2)",
    "temperature_2m - dewpoint_2m",
    r#"interpolate_z(temperature_iso, height_iso, quantity(3000, "m"))"#,
    "div(grid_vector(U10, V10))",
    "curl(grid_vector(U10, V10))",
    r#"interpolate_z(tk, z, quantity(3000, "m"))"#,
    "dt(T2)",
    "z_to_dbz(dbz_to_z(composite_reflectivity) * 2)",
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GuideSection {
    #[default]
    GettingStarted,
    Products,
    Layers,
    ModelData,
    Wrf,
    Cm1,
    FormulaLab,
    Satellite,
    SimSat,
    Archive,
    Player,
    Tools,
    Volume3d,
    CaptureBrand,
    Shortcuts,
    Sources,
}

impl GuideSection {
    const ALL: [GuideSection; 16] = [
        Self::GettingStarted,
        Self::Products,
        Self::Layers,
        Self::ModelData,
        Self::Wrf,
        Self::Cm1,
        Self::FormulaLab,
        Self::Satellite,
        Self::SimSat,
        Self::Archive,
        Self::Player,
        Self::Tools,
        Self::Volume3d,
        Self::CaptureBrand,
        Self::Shortcuts,
        Self::Sources,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::GettingStarted => "Getting started",
            Self::Products => "Products explained",
            Self::Layers => "Map & layers",
            Self::ModelData => "Models & soundings",
            Self::Wrf => "WRF & simulated radar",
            Self::Cm1 => "CM1",
            Self::FormulaLab => "Formula Lab",
            Self::Satellite => "Satellite",
            Self::SimSat => "SimSat",
            Self::Archive => "Archive & events",
            Self::Player => "Unified Player",
            Self::Tools => "Tools & inspector",
            Self::Volume3d => "3D Volume",
            Self::CaptureBrand => "Capture & brand",
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
                    egui::ScrollArea::vertical()
                        .id_salt("guide_nav_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
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
                            GuideSection::Wrf => wrf(ui),
                            GuideSection::Cm1 => cm1(ui),
                            GuideSection::FormulaLab => formula_lab(ui),
                            GuideSection::Satellite => satellite(ui),
                            GuideSection::SimSat => simsat(ui),
                            GuideSection::Archive => archive(ui),
                            GuideSection::Player => unified_player(ui),
                            GuideSection::Tools => tools(ui),
                            GuideSection::Volume3d => volume_3d(ui),
                            GuideSection::CaptureBrand => capture_brand(ui),
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
            .color(subhead_color()),
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

/// A compact, selectable-looking formula example. The text itself lives in
/// [`GUIDE_FORMULA_EXAMPLES`] so tests can compile every displayed equation.
fn formula_example(ui: &mut egui::Ui, lead: &str, formula: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        ui.strong(lead);
        ui.label(
            egui::RichText::new(formula)
                .monospace()
                .color(accent_color()),
        );
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
                .color(accent_color()),
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
         Radar (site, products, tilts, loop, algorithms, tools — live operations), Map \
         (map layers, added overlays, analysis overlays, appearance), Alerts (NWS warning polygons + \
         SPC outlooks and reports), Data (archive days, live poll feeds, the model store, \
         local files), and \u{2699} Settings (display, security and updates, alerts, hotkeys, performance). \
         Collapsible sections remember whether you left them open.",
    );
    para(ui, GUIDE_TOP_BAR_TEXT);

    subhead(ui, "DISK CACHE SAFETY");
    para(
        ui,
        "Settings > Performance caps BowEcho's rebuildable downloaded cache at 32 GiB by \
         default. The cap is shared by downloaded Level-II/archive/live radar files and map \
         tiles; when needed, the oldest files recycle first. Set the limit to 0 only when \
         you explicitly want unlimited cache growth.",
    );
    action(
        ui,
        "Repair/reset downloaded cache…",
        "— uses a confirmation step, then safely recreates only those radar/map cache \
         folders. Models, satellite data, local imports, settings, palettes, annotations, \
         screenshots, and recordings are never removed. This clears damaged downloaded \
         copies so BowEcho can fetch them again; it does not modify or repair a user source \
         file.",
    );

    subhead(ui, "WORKFLOWS");
    action(
        ui,
        "Workflows \u{25be}",
        "\u{2014} applies one of the built-in operating setups: warning desk, \
         velocity & rotation, model & WRF, satellite & tropical, or archive review. \
         The current workflow marker is only a label for the latest preset; manual tweaks \
         stay yours, Restore previous setup undoes the latest preset's setup changes, and Clear \
         marker hides only the label.",
    );

    subhead(ui, "PICK A RADAR");
    action(
        ui,
        "Site dropdown",
        "— Radar tab \u{25b8} SITE lists the US NEXRAD network. Pick a site, then Load Latest \
         (newest volume) or Load Loop (recent history). When an international feed owns the \
         view, Load Latest / Load Loop target that feed instead: providers with recent \
         catalogs load real loops, single-frame providers start at the newest scan and grow \
         live. Center recenters the map on it. The site you load is remembered as the \
         startup site for next launch.",
    );
    action(
        ui,
        "International radars",
        "— the amber markers. Click one to live-poll that feed on the provider's cadence; \
         Data \u{25b8} Radar coverage is the provider-aware picker, with archive access \
         where a provider offers it.",
    );
    action(
        ui,
        "Right-click the map",
        "— opens \"Lowest beam here\": the nearby radars — US and international alike — whose \
         0.5° beam is lowest over that point (4/3-Earth geometry), with beam height and \
         distance. One click switches there and loads (or live-polls) the latest data. \
         Right-clicking also jumps to the nearest site directly.",
    );
    action(
        ui,
        "Click a site marker",
        "— selects that site without loading anything.",
    );

    subhead(ui, "GO LIVE");
    action(
        ui,
        "LIVE (top bar)",
        "— one click: follow the current radar's newest data and backfill a recent loop. \
         US sites arm the real-time chunk feed; international sites resume the provider \
         poll. Synced warnings arm automatically.",
    );
    action(
        ui,
        "Live",
        "— tick it in SITE to auto-refresh from the US NEXRAD real-time chunk feed. With \
         Chunks on, partial tilts draw as they arrive instead of waiting for a complete low \
         tilt. International sites go live by polling instead — click an amber marker or a \
         right-click menu row and frames refresh on the provider's cadence.",
    );
    para(
        ui,
        "The chip on the canvas reads LIVE / ARCHIVE / STALE — the source mode and frame age, \
         at a glance. The STALE threshold is tuned to US volume cadence, so slower \
         international feeds can flag STALE while healthy.",
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
        "— under Site, set Frames to load before pressing Load Loop; this persisted count controls \
         how many recent radar scans BowEcho fetches and keeps for playback. Raise it and press \
         Load Loop again to refill with more scans. After loading, use play/pause, step buttons, \
         and the scrub slider. Video & GIF export settings only record already-loaded frames; they \
         do not change the load count. Unified Player still supports the same limit up to 2000 frames.",
    );

    subhead(ui, "PANES & THE MAP");
    action(ui, GUIDE_PANES_LABEL, GUIDE_PANES_TEXT);
    para(
        ui,
        "Drag pans, the scroll wheel zooms about the cursor, and View > Reset map view \
         recenters on the site. The colorbar for the active product draws on-canvas.",
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
         for \u{2265}65 dBZ (hail cores); more palettes in Map \u{25b8} Appearance.",
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
         Two dealias engines: Region (default) unfolds each tilt from boundary evidence — \
         fast and honest, but an isolated echo's absolute branch can be ambiguous. \
         Analyst 3D (model-anchored) solves all velocity tilts of the volume jointly, adds \
         the previous volume as a temporal prior, and — for US CONUS sites — anchors the \
         absolute branch to a RAP analysis wind profile fetched in the background; \
         international sites run it without the model anchor. Slower than Region.",
        "Region: Jing & Wiener 1993 (JTECH 10); Feldmann et al. 2020, R2D2 \
         (JTECH-D-20-0054.1); Helmus & Collis 2016 (Py-ART, JORS). Analyst 3D adds: \
         Eilts & Smith 1990 (JTECH 7, environmental wind constraints); James & Houze 2001 \
         (JTECH 18, 4DD sounding initialization); Louf et al. 2020 (JTECH 37, UNRAVEL \
         repair checks).",
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

    subhead(ui, "ADVANCED ON-DEMAND SWEEP PRODUCTS");
    para(
        ui,
        "The PRODUCTS row's Derive advanced button computes extra per-tilt products for the \
         current visible tilt. KDP is still automatic; these products are computed only when \
         requested, then stay selectable as live/loop frames advance.",
    );
    product_entry(
        ui,
        "PHIF / KDP_SD / PHI_TEX / KDP_TEX — Phase diagnostics",
        "filtered differential phase, KDP uncertainty, and local texture of PHI/KDP.",
        "use PHIF to inspect a smoother phase field, KDP_SD as confidence context, and \
         texture fields to find noisy/mixed phase or non-meteorological gates.",
        "",
    );
    product_entry(
        ui,
        "AH / PIA / REFC — Reflectivity attenuation correction",
        "specific attenuation, path-integrated attenuation, and attenuation-corrected REF.",
        "most useful where heavy rain attenuates the beam; verify against adjacent radars or \
         higher tilts before treating corrected cores as ground truth.",
        "",
    );
    product_entry(
        ui,
        "ADP / PIDA / ZDRC — ZDR attenuation correction",
        "specific differential attenuation, path-integrated differential attenuation, and \
         attenuation-corrected ZDR.",
        "helps keep ZDR arcs/columns interpretable in heavy rain, but calibration and phase \
         quality still dominate.",
        "",
    );
    product_entry(
        ui,
        "RATE_Z / RATE_KDP / RATE — Rain-rate estimates",
        "Z-R rain rate, KDP rain rate, and a hybrid polarimetric rain-rate field.",
        "RATE is the most operator-friendly of the three; compare RATE_Z vs RATE_KDP when hail \
         contamination or attenuation is suspected.",
        "",
    );
    product_entry(
        ui,
        "LWC / HKE — Water and hail-loading proxies",
        "radar liquid-water-content proxy and hail kinetic-energy flux from reflectivity.",
        "diagnostic context for wet microbursts and hail cores; values depend strongly on \
         sampling height and beam filling.",
        "",
    );
    product_entry(
        ui,
        "CDR / L_RHO / MET_QI / MET_MASK — Hydrometeor quality diagnostics",
        "circular depolarization ratio, log correlation ratio, meteorological gate quality, \
         and a binary meteorological gate mask.",
        "use these as quality-control context before trusting small dual-pol signatures.",
        "",
    );
    product_entry(
        ui,
        "REF_TEX / VEL_TEX / SW_TEX / ZDR_TEX / RHO_TEX — Texture fields",
        "local variability around each gate for reflectivity, velocity, spectrum width, ZDR, \
         and CC.",
        "texture highlights noisy gates, boundaries, turbulence, debris, and mixed phase; it is \
         a signal amplifier, not a standalone diagnosis.",
        "",
    );
    product_entry(
        ui,
        "REF_GRAD_R / VEL_GRAD_R / TURB — Radial gradients and turbulence",
        "range-gradient proxies for REF/VEL plus a Doppler turbulence proxy.",
        "use gradients to find sharp cores, gust fronts, and shear boundaries; pair TURB with \
         SW and velocity texture.",
        "",
    );
    product_entry(
        ui,
        "TDS_SCORE / HAIL_SCORE — Dual-pol diagnostic scores",
        "compact percent-style scores for tornadic debris and hail signatures.",
        "treat as triage: check the raw REF, VEL/SRV, CC, ZDR, warning context, and scan height \
         before making a call.",
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
        "ET — Echo tops (kft)",
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
         0°C/\u{2212}20°C heights first — the From-model button samples the model profile at \
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
// 2b. Map — layers and appearance

fn layers(ui: &mut egui::Ui) {
    ui.heading("Map");
    para(
        ui,
        "The Map tab groups map layers, added overlays, analysis overlays, and appearance. \
         Map layers include the primary radar, overlay radars, rotation tracks + TDS, GOES, \
         model and mesoanalysis fields, WoFS and FARM drapes, surface obs, lightning, SPC \
         outlooks and reports, warning polygons, and placefiles. Layers draw bottom-to-top \
         in list order.",
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
         layers, the Alerts tab for SPC and warnings, or a small popover for layers with \
         only a few options (surface-obs networks, lightning). Broader appearance controls \
         live in Map > Appearance.",
    );
    para(
        ui,
        "Map \u{25b8} Appearance also owns deep visual tuning: map backdrop, warning-polygon \
         fill/width, per-family polygon colors and dash style, radar-age ring/marker arc/chip \
         colors and thresholds, radar product color tables, and built-in appearance profiles \
         such as GR2-classic, Chase dark, and Accessibility.",
    );
    subhead(ui, "ALERTS");
    para(
        ui,
        "The Alerts tab remembers Show, Active only, Auto-refresh, and every family filter \
         across restarts and upgrades. Enable Watch, then use TOR Watch, SVR Watch, PDS \
         Watch, and Other Watch to choose the exact watch polygons shown. PDS is a separate \
         bucket rather than being counted twice under TOR/SVR.",
    );
    para(
        ui,
        "Observed tornado warnings have their own border slot under Map > Appearance > \
         Warning polygons. PDS and tornado-emergency styles still take priority when those \
         stronger tags are present. SPC's CIG regions ride inside the ordinary outlook \
         products, so duplicate standalone CIG switches are intentionally omitted.",
    );
    action(
        ui,
        "Radar labels",
        "- switch site markers between compact IDs, full names, or hidden labels. Compact IDs \
         are the clean default for busy zoomed-out maps and multi-radar review.",
    );
    action(
        ui,
        "Brand Kit",
        "- changes the runtime identity used in the window title, screenshots, output paths, \
         share cards, watermarks, links, and colors. It does not replace signing identity or \
         installer metadata.",
    );
    action(
        ui,
        "+ Add layer ⏷",
        "— the single front door for every map data type: radar overlays, model fields, \
         satellite, WoFS/FARM drapes, mesoanalysis composites, surface obs, placefiles. You \
         never need to know which window a layer is born in.",
    );

    subhead(ui, "ANALYSIS (OA)");
    para(
        ui,
        "In Analysis overlays: compute that EMITS layers. Analyze obs runs a Bratseth \
         objective analysis of the model surface field against live obs; Compute composites \
         builds the full SPC mesoanalysis suite (SCP, STP, SHIP, EHI, …) — each field then \
         adds as an instant \"(OA)\" layer, also reachable from + Add layer > \
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
         Radar tab keeps a one-line \"Custom: N layers \u{2192}\" link; Poll-URL feeds moved to the \
         Data tab (they replace the volume source — acquisition, not a layer); SPC \
         day/kind config moved to the Alerts tab.",
    );
}

// ---------------------------------------------------------------------------
// 4. Models & soundings

fn model_data(ui: &mut egui::Ui) {
    ui.heading("Models & soundings");
    para(
        ui,
        "Model fields and skew-T soundings (HRRR/RAP/RRFS-style CONUS workflows plus GFS \
         worldwide, plus local WRF and NetCDF files you import yourself), layered straight \
         onto the radar map. Enable the \
         master switch first: \u{2699} Settings \u{25b8} Model \u{25b8} Model data (off = \
         pure radar app). Windows \u{25be} \u{25b8} Models opens stored runs and plotting; \
         Windows \u{25be} \u{25b8} WRF opens raw-WRF processing and simulated radar; \
         Windows \u{25be} \u{25b8} Formula Lab opens custom diagnostics.",
    );

    subhead(ui, "GETTING DATA");
    action(
        ui,
        "Latest HRRR f00 / f00–f01",
        "— fetches f00 from the newest published HRRR cycle, with an Include f01 option. \
         Processing is either Sounding (volumes plus the required surface fields) or Full \
         (all normal fields and derived diagnostics). Full quick processing explicitly keeps \
         eCAPE/heavy off. Work runs below UI priority.",
    );
    action(
        ui,
        "Custom download setup",
        "— any supported model, init date/cycle, hours spec (\"0-3\" or \"2,4,6\"), and \
         profile, with a live size estimate before you commit. Use displayed radar time can \
         seed this form from the radar frame currently on screen.",
    );
    action(
        ui,
        "Keep runs",
        "— store retention; the newest N runs survive each fetch and startup (default 2, \
         \u{2248}1.5 GB on disk).",
    );

    subhead(ui, "RELATED WORKSPACES");
    action(
        ui,
        "Windows \u{25be} \u{25b8} WRF",
        "— raw wrfout/NetCDF import, wrf-rust full diagnostics, GDEX climate archives, \
         and simulated radar. Finished model fields enter this same Models library.",
    );
    action(
        ui,
        "Windows \u{25be} \u{25b8} CM1",
        "— native NCAR CM1 inventory, explicit local-Cartesian placement, exact-time field \
         loops, native columns and scalar REF/VEL. CM1 does not pass through the WRF reader.",
    );
    action(
        ui,
        "Windows \u{25be} \u{25b8} Formula Lab",
        "— safe custom diagnostics over the selected stored run or a raw WRF file. Results \
         return here for maps, native plots, PNG output, and color-table work.",
    );

    subhead(ui, "UPPER-AIR (ISOBARIC) FIELDS");
    para(
        ui,
        "Temperature, Dewpoint, RH, Wind speed, and Height at 925 / 850 / 700 / 500 / 300 / \
         250 mb appear in the model field picker as ordinary map layers. They are synthesized \
         from the run's isobaric sounding volumes, so imported WRF runs and downloaded models \
         both gain them with no re-import; where a model already ships a real per-level field, \
         that field wins over the synthesized one. On WRF fields they draw through the \
         level-aware Solarpower07 model palettes — the resolver picks the table that matches \
         the level and units — instead of the plain fallback ramp.",
    );

    subhead(ui, "THE MODEL WINDOW & MAP LAYER");
    para(
        ui,
        "Runs tree on the left, field viewer in the middle. Show on radar map renders the \
         selected field as a layer under the radar.",
    );
    action(
        ui,
        GUIDE_CUSTOM_LAYER_ROW_LABEL,
        "— the checkbox hides the layer without losing it (a hidden layer still feeds the \
         inspector and Alt+click soundings); \u{25c0} \u{25b6} step the forecast hour; the \
         slider sets layer opacity; \u{2715} removes it. The Radar opacity slider above lets \
         the model field show through the radar.",
    );
    action(
        ui,
        "\u{1f4d0} Draw plot box",
        "— arms the radar map: the next click-drag draws a custom domain for the native plot \
         window, and nothing else fires (no pan, loupe, sounding, or 3D box). Esc or \
         right-click cancels; a finished box disarms. Ctrl+Shift+drag on the map is the \
         direct shortcut, and Shift-drag on the field viewer inside the Model window still \
         works.",
    );

    subhead(ui, "SOUNDINGS");
    action(
        ui,
        "Alt+click",
        "anywhere on the map — a native skew-T for that exact point opens in the Sounding \
         window, computed from the model profile.",
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
    action(
        ui,
        "Stretch / Fit",
        concat!(
            "— in a docked SHARPpy sounding, Stretch fills the pane independently in both ",
            "directions as you drag the workspace dividers; Fit preserves the desktop board's ",
            "aspect ratio. The choice persists.",
        ),
    );
    action(
        ui,
        "Text",
        concat!(
            "â€” choose the bundled Space Grotesk face, egui's clean proportional sans, or ",
            "technical monospace, and scale sounding text independently from 50â€“200%. Font ",
            "and size persist across model, box-mean, obs-adjusted, and RAOB soundings without ",
            "changing the panel geometry.",
        ),
    );
    action(
        ui,
        "Correct / Corrected",
        concat!(
            "— opens the display-only model-sounding correction editor. Add one or more ",
            "native levels, enter a target height AGL (BowEcho shows the exact model level ",
            "used), then enable and override T, Td, wind direction, or wind speed. Blend ",
            "sets a cosine-smoothed vertical transition around that level; 0 m changes only ",
            "the anchor. The skew-T, hodograph, parcels, and every diagnostic recalculate ",
            "immediately. MANUAL stays visible until Reset original restores the untouched ",
            "source column. This tool never edits downloaded/model-store data and is not ",
            "offered for observed RAOBs.",
        ),
    );
    action(
        ui,
        "⚙ Edit panel layout",
        concat!(
            "— choose which diagnostic occupies each cell, then drag the cyan shared borders ",
            "to resize the upper and bottom rows, skew-T and right side, hodograph and inset ",
            "row, narrow strips, individual insets, and bottom cells. Parcels & thermo, ",
            "Kinematics, SHIP, Severe indices, and Streamwiseness are separate movable panels. ",
            "Every split remains aligned and persists; reset restores the default.",
        ),
    );

    subhead(ui, "HAIL LEVELS FROM THE MODEL");
    action(
        ui,
        "From HRRR / From GFS",
        "— with MEHS, POSH, or POH selected, the \"Hail 0°C/\u{2212}20°C\" row appears under \
         PRODUCTS. The button (named for the loaded model) samples its temperature profile \
         at the radar site and sets \
         both crossing heights — the same environmental inputs MRMS takes from model \
         analyses. The hail products are sensitive to these; use it instead of the defaults \
         whenever model data is loaded.",
    );

    subhead(ui, "INSPECTOR");
    para(
        ui,
        "With \"Model value\" ticked in Inspector\u{2026}, the cursor card reads the model \
         field under the cursor — even while the map layer is hidden.",
    );
    subhead(ui, "EVENT AUTO-LOAD");
    para(
        ui,
        "Event-day tornado tracks and Archive tornado report clicks can auto-load the model \
         run/hour closest to that event time. This is meant for fast post-event soundings: \
         click the track/report, let the radar loop load, then Alt+click or Ctrl+Alt+hover \
         for nearby profiles.",
    );
}

// ---------------------------------------------------------------------------
// 5. WRF & simulated radar

fn wrf(ui: &mut egui::Ui) {
    ui.heading("WRF & simulated radar");
    para(
        ui,
        "Windows \u{25be} \u{25b8} WRF is the first-class workspace for local raw WRF: \
         quick import, the wrf-rust diagnostic suite, GDEX climate archives, simulated radar, \
         and a handoff to Formula Lab. It shares one backend and selected run with Models, so \
         stored output appears in Models without a second import.",
    );

    subhead(ui, "CHOOSE THE RIGHT WORKFLOW");
    action(
        ui,
        "Open WRF / NetCDF",
        "— the lighter store import. Use it for common 2-D surface fields and the isobaric \
         temperature/dewpoint/wind/height volumes that drive soundings. It accepts raw wrfout, \
         post-processed climate wrfout, and compatible NetCDF.",
    );
    action(
        ui,
        "Extract namelist…",
        "— reads one raw wrfout and saves an annotated partial reconstruction. Only exact \
         stored values that safely map to the represented domain become active assignments; \
         exact context, inferred values and unavailable settings remain comments. The result \
         is not the original namelist.input and cannot reproduce the run.",
    );
    action(
        ui,
        "WRF full diagnostics",
        "— the comprehensive wrf-rust route for CAPE/CIN, shear, SRH, STP/SCP/EHI, \
         LCL/LFC/EL, precipitation, radiation, soil and other diagnostics. This can take \
         minutes and several GiB per file on a large convection-resolving grid.",
    );
    action(
        ui,
        "WRF simulated radar",
        "— samples hydrometeors, winds and terrain into native polar radar volumes. Use it \
         when the desired output is a radar loop, not a model-store field.",
    );
    action(
        ui,
        "Formula Lab",
        "— builds a custom bounded diagnostic from the selected stored run or from one raw \
         WRF file. Grid-aware calculus requires the raw-WRF source.",
    );
    action(
        ui,
        "Browse GDEX catalog\u{2026}",
        "— downloads a whole file or an NCSS subset from NSF NCAR GDEX, including CONUS II \
         regional climate WRF, then imports it through the same local model-store path.",
    );

    subhead(ui, "FILES, FOLDERS & PROCESSING");
    para(
        ui,
        "File pickers are deliberately unfiltered: ordinary extensionless wrfout_* names from \
         every domain are valid. Open file imports one file; Process files and Build from files \
         multi-select one to hundreds. Folder actions find supported files and sort them before \
         work begins. Each WRF time becomes a store timestep for import, or normally one \
         radar-loop frame for simulated radar.",
    );
    action(
        ui,
        "Core surface fields + sounding",
        "— T2/Td2/RH, 10 m winds, pressure, PWAT, composite reflectivity, UH, precipitation, \
         terrain, and isobaric sounding volumes.",
    );
    action(
        ui,
        "Severe / thermo diagnostics",
        "— the normal 2-D getvar suite: CAPE/CIN, helicity, shear, composite indices and \
         parcel levels.",
    );
    action(
        ui,
        "Heavy eCAPE (slow)",
        "— entrainment-CAPE families and eCAPE composites. It is intentionally off by \
         default because it materially raises processing cost.",
    );
    action(
        ui,
        "Raw model extras",
        "— selected native fields such as PBL height, surface fluxes, radiation, skin/sea \
         temperature, snow and graupel.",
    );
    para(
        ui,
        "Only is an allow-list and Skip is applied afterward as a deny-list. The live preview \
         shows the store fields the current group/filter selection plans to write. Large grids \
         receive an explicit memory/time warning before either import route begins; narrowing \
         the field plan does not make a giant 3-D sounding interpolation free.",
    );
    para(
        ui,
        "Imported raw names receive readable labels and Solarpower07 model palettes where a \
         matching style exists. Automatically plot new imports is on by default and persisted; \
         light, full-diagnostic and GDEX imports can write every field/hour to the screenshots \
         folder. It is independent from the simulated-radar path.",
    );

    subhead(ui, "SIMULATED RADAR: BUILD, REFRESH & EXPORT");
    para(
        ui,
        "Pick one or more raw WRF files, or a folder. Every selected forecast time becomes a \
         frame in one radar loop unless the adjacent-scene Drop policy omits an unbracketed \
         frame; nothing is written to the model store. One loop must contain exactly one \
         compatible run, WRF domain and grid. Mixed d01/d02, remeshed grids, duplicate/restart \
         times and untimed scenes are rejected instead of silently merged. Completed elevations, \
         radials, gates and moments enter the ordinary radar viewer, so tilts, readouts, \
         cross-sections, derived products and velocity tools work through the same path as an \
         observed Level-II volume.",
    );
    action(
        ui,
        "Build from files\u{2026} / Build from folder\u{2026}",
        "— captures an ordered source set and runs the current recipe plus advanced controls.",
    );
    action(
        ui,
        "Refresh current frame(s)",
        "— rebuilds that same remembered source set with the controls exactly as they are now. \
         It does not reopen a picker, rescan the folder, or silently add new files. The source \
         snapshot lasts for this app session and the completed refresh replaces the current \
         simulated-radar loop.",
    );
    action(
        ui,
        "Replay displayed observed scan\u{2026}",
        "\u{2014} choose one raw wrfout file and run it through the scan geometry currently \
         displayed in BowEcho. Replay preserves the observed site's cuts, split cuts, rays, \
         acquisition times, gate layout, missing sectors, moment availability, radial status, \
         and per-ray Nyquist instead of approximating them with a named VCP. When the decoded \
         source supplies aligned ray-local instrument metadata, replay also copies PRT, \
         unambiguous range, pulse count and independent-sample count to the simulated rays.",
    );
    para(
        ui,
        "A completed replay opens one linked three-pane validation workspace: Observed, \
         Simulated and Difference. Tilt selection stays synchronized only where all three \
         volumes have identical cut geometry; the Difference pane is synthetic minus observed \
         on exact overlapping gates. An observed moment that is absent or cannot be compared is \
         reported unavailable, never fabricated. The observed radial timing owns ambiguity, so \
         replay disables custom coupled-PRF timing, manual folding and stage diagnostics.",
    );
    para(
        ui,
        "Missing ray-local instrument values stay missing: replay never derives PRT or \
         unambiguous range from a numbered VCP code or flattens differing rays to one scalar. \
         Custom coupled single-PRF scans instead stamp their resolved PRT, unambiguous range, \
         transmitted pulse count and effective independent-sample count on every generated ray.",
    );
    action(
        ui,
        "Export latest as CfRadial\u{2026}",
        "— one generated frame opens a .nc save dialog; a loop opens a folder picker and writes \
         one CfRadial-1 file per frame. Each file preserves available per-ray PRT, unambiguous \
         range, pulse count and independent-sample arrays alongside moments, acquisition timing, \
         calibration, model/microphysics identity and forward-operator provenance.",
    );

    subhead(ui, "OPERATIONAL HRRR/RRFS FORECAST RADAR");
    para(
        ui,
        "The Operational forecast radar card turns native hybrid-level HRRR or RRFS GRIB into \
         the same ordinary polar RadarVolume, loop, products, Refresh action and CfRadial export \
         as raw WRF. Latest HRRR can build f00, f01, or a same-cycle f00/f01 loop through \
         SimSat's existing publication-aware, resumable downloader. Cached HRRR discovery uses \
         the same SimSat input and model-cache directories; BowEcho does not create a second \
         cache. The local picker is unrestricted and accepts multiple native GRIB files.",
    );
    para(
        ui,
        "The reader crops around the chosen virtual radar, converts omega to geometric vertical \
         velocity, rotates grid-relative winds when declared, and streams native cloud liquid, \
         cloud ice, rain, snow and graupel through additive scattering. HRRR uses an explicit \
         Thompson-family category mapping. RRFS uses only the inventory carried by the file and \
         never invents a hidden scheme identity. Missing number fields use documented bulk-PSD \
         defaults. The result is bulk S-band dual-pol-like forecast guidance, not property \
         T-matrix science or observed calibration; incomplete/inconsistent inventories fail \
         closed and all assumptions are exported in provenance.",
    );

    subhead(ui, "BUILD 24 VCP & ATMOSPHERE TIME");
    para(
        ui,
        "The scan selector includes source-qualified Build 24 base patterns for VCP 12, 34, 35, \
         112, 212 and 215. All 94 Appendix C physical rows remain in source order, including \
         equal-elevation surveillance/Doppler split cuts and VCP 112's two fixed MPDA Doppler \
         cuts. Row elevation, azimuth rate, period, waveform, moment coverage, PRF code and pulse \
         count enter volume and CfRadial provenance.",
    );
    para(
        ui,
        "A numbered PRF code is not a PRF in Hz, so BowEcho does not invent PRT or unambiguous \
         range from it. These are versioned Build 24 base-pattern simulations, not a claim to \
         reproduce one site's live operational scan: SAILS, MRLE, AVSET, Add-MPDA and \
         site-specific low-tilt adaptations are outside the catalog.",
    );
    para(
        ui,
        "Find this at WRF simulated radar \u{2192} Radar location & fine tuning (advanced) \
         \u{2192} Instrument & propagation \u{2192} Atmosphere time. Ray timing and atmosphere \
         sampling are separate concepts, but either adjacent-scene choice also enables Timed \
         volume because instantaneous rays have no acquisition offsets. Frozen samples the \
         anchor scene. Linear adjacent is the fast path: every timed ray blends linear Z, winds \
         and additive polar scattering within the next compatible model-time bracket, then \
         derives ratios such as ZDR and rhoHV.",
    );
    para(
        ui,
        "Raw-state pre-closure is the slower P3/ISHMAEL research reference. At each \
         pulse-volume sample it combines the actual up-to-eight spatial/vertical contributors \
         from each scene with the ray's temporal weight, blends raw winds, TKE, pressure, \
         temperature, density and native property tuples, then performs one nonlinear closure \
         and one validated T-matrix/PSD evaluation. Endpoints are exact. Scheme, inventory, \
         category, rain, coverage or table mismatch stops the run instead of substituting the \
         additive-time path. Both adjacent modes never extrapolate; a missing or too-short \
         bracket follows the explicit hold, drop or error policy and the retained two-scene \
         state is checked against the configured memory budget.",
    );
    para(
        ui,
        "Raw-state pre-closure currently requires the BowEcho S research pack at exactly 2.8 GHz with \
         Full property rain/melting sensitivity. A validated local S/C/X pack or Frozen-only \
         sensitivity must use Frozen or Linear adjacent (derived/additive); the UI and backend \
         disable new incompatible Raw-state selection, and backend validation rejects any \
         retained incompatible combination.",
    );
    para(
        ui,
        "Interpolation changes sampling inside each output radar volume: later rays and cuts \
         blend slightly farther toward the next WRF scene. It does not create extra loop frames, \
         add low-level-update frames, run WRF forward, or update live. With N compatible scenes, \
         the first N\u{2212}1 frames can use their following scene; the last follows Hold last, Drop \
         or Error, so Drop can produce fewer than N output frames.",
    );

    subhead(ui, "START WITH A COMPLETE RECIPE");
    action(
        ui,
        "Storm view (fast)",
        "— sharp textured REF and clean unfolded VEL for browsing a run; no simulated \
         hardware effects.",
    );
    action(
        ui,
        "Clean model truth",
        "— artifact-free model REF/VEL: no texture, noise, folding, blockage or scan-time \
         effects.",
    );
    action(
        ui,
        "Clean dual-pol",
        "— polarimetric microphysics and propagation without noise, velocity folding or \
         terrain blockage.",
    );
    action(
        ui,
        "Real radar (balanced) — recommended",
        "— practical virtual S-band measurement with 9-point beam integration, fall speed, \
         terrain blockage, dual-pol propagation, sensitivity, timed rays and folded velocity.",
    );
    action(
        ui,
        "Maximum fidelity (slow)",
        "— the full 27-point pulse-volume rule for one frame or a short loop. Source-model \
         resolution still limits the real information content.",
    );
    action(
        ui,
        "P3/ISHMAEL T-matrix (research)",
        "— opt-in property-aware dual-pol using the on-demand BowEcho 2.8 GHz S research tables, \
         Full property sensitivity and Raw-state pre-closure. Exact supported inputs are \
         required; it never substitutes another table source or band. The only Rayleigh-limit \
         route is the named, audited bridge for exact P3 small dense spheres below the table \
         floor; every other unsupported lookup fails closed.",
    );
    para(
        ui,
        "Choosing a recipe resets every interacting physics, presentation, instrument and \
         calibration control to a compatible set while preserving antenna placement, range \
         and gate geometry. A later expert edit is labeled Custom tuning.",
    );
    para(
        ui,
        "Storm view, Clean model truth and Clean dual-pol start Frozen. Real radar and Maximum \
         fidelity start with Timed volume plus additive adjacent-scene interpolation and Hold \
         last. P3/ISHMAEL T-matrix starts with Timed volume plus the narrower Raw-state \
         pre-closure reference. You can change compatible temporal controls after choosing a \
         recipe.",
    );

    subhead(ui, "WILL THE P3/ISHMAEL RECIPE ACCEPT MY FILE?");
    para(
        ui,
        "The research recipe requires a raw WRF file whose global MP_PHYSICS value is P3 \
         50\u{2013}53 or ISHMAEL 55, and the file must retain the native property variables that \
         the matching reader needs. MP_PHYSICS alone is not sufficient. Thompson 8 and other \
         conventional schemes belong on Storm view, Clean model truth, Clean dual-pol, Real \
         radar or Maximum fidelity; the research recipe fails closed with a readable \
         property-reader error instead of guessing or falling back.",
    );

    subhead(ui, "WHAT THE MOMENTS MEAN");
    action(
        ui,
        "REF / REFC",
        "— observed horizontal reflectivity after propagation loss / intrinsic corrected \
         horizontal reflectivity before that loss.",
    );
    action(
        ui,
        "VEL / SW",
        "— scatterer-weighted radial velocity / spectrum width from pulse-volume variance, \
         terminal-speed diversity, optional model turbulence and the instrument floor.",
    );
    action(
        ui,
        "ZDR / ZDRC",
        "— observed / intrinsic differential reflectivity; the observed field includes the \
         configured ZDR bias and integrated differential attenuation.",
    );
    action(
        ui,
        "CC (rhoHV)",
        "— copolar correlation from the bulk species mixture, not an independently invented \
         display texture.",
    );
    action(
        ui,
        "KDP / PHI",
        "— specific differential phase / accumulated two-way differential phase, including \
         the configured system phase.",
    );
    action(
        ui,
        "AH / ADP, PIA / PIDA",
        "— specific horizontal/differential attenuation and their two-way radial integrals.",
    );

    subhead(ui, "QUALITY FIELDS & ALGORITHM TRUTH LAB");
    para(
        ui,
        "Gate support fields (MCOV / TUNB / MSIG) is enabled by default and persisted. \
         MCOV is the configured quadrature weight covered by the model domain; TUNB is the \
         nested fraction that also remains terrain-unblocked; MSIG is the still-smaller fraction \
         that contains meteorological signal. Minimum model coverage separately masks physical \
         moments below its persisted 0–1 MCOV threshold; the emitted quality fields stay \
         unmasked so the rejection remains auditable. The default threshold is zero.",
    );
    para(
        ui,
        "The coupled instrument can additionally retain six exact stage triples: IREF/IVEL/ISW/ \
         IZDR/IRHO/IKDP are Ideal pulse-volume moments after propagation and before receiver \
         effects; the matching M* fields add PRF/dwell/pulse-count/SNR censoring, uncertainty, \
         bias and PRF-derived velocity ambiguity; canonical REF/VEL/SW/ZDR/RHO/KDP are Presented \
         values after optional deterministic texture and stylized clutter. All three stages share \
         gate geometry, so comparisons require no regridding.",
    );
    action(
        ui,
        "Enable stage diagnostics",
        "\u{2014} in WRF simulated radar open Radar location & fine tuning (advanced) \
         \u{2192} Instrument & propagation. Use a custom scan, turn on Physically coupled \
         single-PRF moment estimator, then turn on Emit Ideal + Measured diagnostic moments. \
         Build or Refresh current frame(s). Named Build 24 VCP rows carry PRF identifiers rather \
         than authoritative Hz values, so they cannot enable this custom single-PRF estimator.",
    );
    action(
        ui,
        "Windows \u{25be} \u{25b8} Algorithm Truth Lab",
        "\u{2014} opens the active synthetic pane at its selected cut. Choose any retained cut; \
         the lab reports exact Ideal \u{2192} Measured, Measured \u{2192} Presented and end-to-end \
         bias/MAE/RMSE/p95/max scorecards. It also runs the selected production dealias engine for \
         that cut when VEL is present and scores fold exposure, recovered folds, wrong Nyquist \
         branches and false unfolds against IVEL. It deliberately does not invent VWP or GBVTD \
         truth metrics.",
    );

    subhead(ui, "FORWARD-OPERATOR SCIENCE");
    para(
        ui,
        "The scientific default interpolates and integrates linear equivalent reflectivity \
         Z = 10^(dBZ/10), then converts the received-power average back to dBZ. Legacy direct-dBZ \
         interpolation remains only to reproduce older renders. Pulse-volume choices are one \
         center sample, nine deterministic Gaussian-weighted samples, or a 3 \u{00d7} 3 \u{00d7} 3 \
         reference quadrature.",
    );
    para(
        ui,
        "The multi-sample operator accumulates linear Z, Z-weighted radial velocity and \
         variance, polarimetric covariance, and terrain occultation. Terminal fall speed can \
         make Doppler follow the scattering particles rather than air alone. Cumulative terrain \
         horizons remove blocked quadrature power instead of renormalizing it, so partial beam \
         blockage remains visible.",
    );
    para(
        ui,
        "Standard 4/3 Earth is the default beam geometry. WRF refractivity (research) instead \
         reads P, PB, T and QVAPOR at the actual virtual-radar site and ray-traces the gate \
         ground range and height through that model profile. Sampling and terrain blockage use \
         the same resolved path. Missing fields, incomplete vertical coverage or a mismatched \
         site stop the build rather than falling back silently. BowEcho records the refractivity \
         gradient and propagation regime and warns explicitly about ducting. Operational \
         HRRR/RRFS forecast radar remains on standard 4/3 Earth.",
    );
    para(
        ui,
        "The default dual-pol path is a scheme-aware bulk S-band Rayleigh operator. It closes available mass \
         mixing ratios and number concentrations into particle-size distributions, adds species \
         in linear scattering space, and then applies near-to-far phase and attenuation. Common \
         Lin/WSM/WDM, Thompson, Morrison, Milbrandt-Yau and NSSL bulk schemes are recognized; \
         the result records whether closure was full two-moment, partial, or assumption-heavy.",
    );

    subhead(ui, "T-MATRIX TABLE SOURCE & SENSITIVITY");
    para(
        ui,
        "BowEcho S research pack is the byte-exact research-v1 five-table source and works only at \
         exactly 2.8 GHz. Its first use downloads and hash-qualifies an optional 182.5 MiB asset. \
         Validated local pack requests one exact manifest-qualified pack at \
         2.8 GHz S, 5.6 GHz C or 9.4 GHz X; it never chooses a nearest band or falls back. No \
         validated C- or X-band pack ships with BowEcho, so those choices remain unavailable \
         until an evidence-backed local pack exists.",
    );
    para(
        ui,
        "The optional asset is bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip \
         from the BowEcho v0.34.1 release (191,400,602 bytes; SHA-256 \
         80b3a2c65ead59c0a951d491e966694e80bb0c49eeb1d3b1fc532bcadbcf507e). BowEcho \
         streams it into the displayed bowecho-simradar/tmatrix-packs cache, validates the ZIP \
         and every exact member, atomically installs the extracted files, then deletes the ZIP. \
         PyTMatrix 0.3.3 is MIT-licensed and its notice is included. These derived tables remain \
         research-only, not independently validated, and not an operational calibration.",
    );
    para(
        ui,
        "Local packs live below the displayed deterministic model-cache path \
         bowecho-simradar/tmatrix-packs. The reproducible \
         crates/radar_scattering/tools/pytmatrix-0.3.3/generate_band_pack.py tool emits exact \
         S/C/X five-role packs, but always marks them unvalidated_research; runtime requires a \
         separately reviewed validated_research manifest with matching sizes, SHA-256 hashes, \
         roles, science revision and exact frequency.",
    );
    para(
        ui,
        "Full property includes standalone/residual rain and qualified wet frozen/rain \
         coexistence. Frozen-only deliberately omits all rain and wet coexistence so only dry \
         frozen categories contribute. This is a sensitivity control, not a prognostic melting \
         model, and the selected mode is fingerprinted and exported.",
    );
    para(
        ui,
        "The opt-in property-aware T-matrix research contract accepts \
         P3 50–53 and ISHMAEL 55 raw tuples. The on-demand BowEcho PyTMatrix tables use exactly 2.8 GHz, \
         symmetric Bruggeman air/ice/water mixing, separate oblate/prolate shapes, and a fixed \
         mean-zero Gaussian canting distribution with 20\u{00b0} standard deviation and \
         deterministic 50-node (5 by 10) orientation integration. Solver-complete diameter \
         domains are \
         role-specific: dry oblate to 89 mm, dry prolate to 32.312 mm, wet oblate to 15 mm, and wet \
         prolate to 6.3 mm. Unsupported phase/shape coordinates are rejected rather than \
         clamped. Each active source cell is closed from \
         its native properties, then only additive scattering quantities are integrated through \
         space, pulse volume and adjacent model times; signed KDP remains signed.",
    );
    para(
        ui,
        "The frozen-particle path is deliberately scheme-specific. ISHMAEL dry frozen \
         categories reconstruct their native gamma PSD from each QICE/QNICE/QVOLI/QAOLI tuple. \
         P3 50–52 reconstruct lambda/mu from the exact official WRF v5.4 two-moment table; P3 53 \
         uses the exact triple-moment table and WRF M3/M6 iteration. BowEcho lazily downloads only \
         the required 1.6 or 17.9 MB file and accepts it only after pinned size, SHA-256, header, \
         layout, source-revision and scheme checks. Production P3 and dry ISHMAEL integrate their \
         reconstructed scheme-native PSDs per particle through the dry T-matrix tables. ISHMAEL \
         audits omitted number, mass and equivalent-volume sixth-moment tails. P3 audits omitted \
         number, mass and its scheme-consistent mass-squared (equivalent-ice-volume-squared) radar \
         weight; native P3 M6 remains the separate PSD/quadrature closure authority. \
         For source-compatible radar moments, the pinned P3 v5.4 operator uses its documented \
         40,000 by 2-micrometre integration grid through an 80 mm upper edge. BowEcho retains \
         analytic closure to infinity without renormalization, reports the excluded analytic \
         tail separately, sends no node above that edge to shape or scattering evaluation, and \
         applies shape/table coverage budgets only within that source domain. \
         Wet ISHMAEL PSD allocation remains unavailable rather than replaced by a characteristic \
         particle.",
    );
    para(
        ui,
        "P3 itself predicts Dmax, mass and projected area but not a unique spheroidal habit or \
         canting distribution. BowEcho leaves the exact PSD and native mass/area law unchanged. \
         Where its explicitly versioned projected-area-equivalent spheroid implies no more than \
         P3's 900 kg/m3 solid-ice density, source area and mass are preserved. At the empirical \
         small-sphere/dense-unrimed area transition, an exact-area homogeneous spheroid can imply \
         a higher density; that branch instead preserves source mass at 900 kg/m3, derives \
         Deq=(6m/(pi*900))^(1/3), and retains normalized source area as the aspect-ratio proxy. \
         This solid-ice-constrained node is audited and is not a table-coordinate clamp; it does \
         not claim exact absolute-area preservation. P3's one-percent rime-density convergence \
         test is a coefficient tolerance, not an area bound: its partially-rimed breakpoint is \
         computed before the final coefficient update and can retain a raw area larger than the \
         Dmax circle. BowEcho marks only that pinned transition artifact, preserves its raw area \
         in the audit, and closes scattering to a mass-preserving sphere at the true Dmax instead \
         of inventing a larger prolate axis. Closure count, maximum raw area ratio and radar-weight \
         fraction are audited; any unmarked overshoot remains an omission. Gaussian-20 canting \
         and every nonspherical habit remain external research assumptions. A strict \
         shape-authoritative mode evaluates only P3's genuinely \
         spherical regions under omission budgets. The old single-characteristic-particle \
         production dispatch is removed.",
    );
    para(
        ui,
        "A separately versioned route covers only exact P3 900 kg/m3 small dense spheres below \
         the dry spherical table's 50 micrometre diameter floor. It anchors the same material, \
         temperature, frequency and radar view at the first table diameter, then applies analytic \
         sphere scaling: D^6 for backscatter/covariance and D^3 absorption plus D^6 scattering \
         for extinction. Target-diameter terminal speed supplies the fall moments. Selection \
         happens before lookup and is recorded in the audit. Nonspherical particles and every \
         other diameter, density, aspect-ratio, temperature, frequency or view miss remain hard \
         errors; this is not an arbitrary or silent fallback.",
    );
    para(
        ui,
        "A research lookup has no clamping or extrapolation. Its -0.5\u{00b0} to 20\u{00b0} view axis \
         covers all 19 custom/named cut centers plus or minus the correctly converted \
         0.95\u{00b0}-FWHM Gaussian beam sigma; a wider custom beam fails closed. Within the \
         declared axes, Waterman T-matrix nodes retain nonspherical, non-Rayleigh/resonant \
         behavior. Missing or mismatched tables, properties, frequency, orientation, shape or \
         view coordinates are errors. The named exact-small-sphere sub-floor route above is \
         selected before lookup and cannot accept those misses; there is never silent Rayleigh fallback. \
         Both the scheme-native \
         ISHMAEL and P3 PSD branches are research-only, are not independently validated, and make \
         no operational claim; P3 shape/canting remains the named external assumption above.",
    );
    para(
        ui,
        "The on-demand BowEcho S research pack contains 2,640,848 grid points across five tables. Its \
         one frozen held-out request passed all 30 of 30 selected property-table nodes. The \
         shared eight-table report is not an all-eight pass: two nodes failed only in a \
         separate conventional dry-ice fixture that this path does not use. The bounded \
         depth-three design audit also remained failed, and the older all-node solver report \
         applies by exact hash only to the unchanged dry-prolate, wet-oblate, and wet-prolate \
         configs. These are transparent software/interpolation checks, not independent \
         scientific validation.",
    );
    cite(
        ui,
        "Implementation lineage: Jung et al. (2008, 2010) bulk polarimetric forward operators; \
         Brandes et al. rain-drop axis-ratio relation; 4/3-Earth beam geometry from Doviak & Zrni\u{107}.",
    );

    subhead(ui, "GEOMETRY, INSTRUMENT & PRESENTATION");
    para(
        ui,
        "The virtual antenna can sit at domain center, an explicit latitude/longitude, or a \
         catalog NEXRAD site; its altitude follows model terrain plus the tower height. Default \
         geometry is 230 km at 250 m gates on the standard 14-tilt ladder, with an optional \
         0.1\u{00b0} cut. Range can reach 1000 km, and gate spacing can follow WRF DX so a coarse \
         grid is not oversampled into many identical gates.",
    );
    para(
        ui,
        "Timed rays receive real acquisition offsets from custom rotation/inter-sweep controls \
         or from each selected Build 24 physical row. They can sample the frozen anchor or a \
         bounded adjacent-scene bracket. Sensitivity can rise with range, and velocity may be \
         folded into a chosen Nyquist interval for dealiasing practice. Reflectivity texture, \
         velocity wobble and ground clutter are deterministic presentation/instrument effects; \
         they are not additional model-resolved weather.",
    );

    subhead(ui, "HONEST LIMITS");
    para(
        ui,
        "Bulk Rayleigh remains the default. In that mode P3, ISHMAEL, incomplete hydrometeor \
         inputs, or unsupported closures fall back explicitly to scalar REF/VEL with a note. \
         The opt-in T-matrix contract is table-bounded research: it can represent nonspherical \
         and non-Rayleigh/resonant scattering within its declared axes. ISHMAEL and P3 frozen \
         categories use scheme-native PSD integration; P3's area/mass spheroid mapping, 900 \
         kg/m3 solid-ice constraint, Gaussian-20 canting, and exact-small-sphere sub-floor route \
         remain explicit external assumptions rather than a general fallback. Wet ISHMAEL PSD \
         allocation and a complete prognostic melting-layer model remain unavailable.",
    );
    para(
        ui,
        "The on-demand BowEcho 2.8 GHz S research bundle is the only first-party property table source. \
         BowEcho ships no validated C/X packs; exact 5.6/9.4 GHz selection fails closed until a \
         separately installed evidence-backed pack satisfies the validated local contract.",
    );
    para(
        ui,
        "Adjacent-scene sampling is bounded interpolation between two compatible WRF times, not \
         atmosphere integration. Build 24 selections reproduce checked base rows, not operational \
         adaptations or an observed volume. A fine radar gate cannot recover structure absent \
         from the WRF grid. The pinned NetCDF writer also cannot yet emit strict character \
         variables for CfRadial sweep_mode/prt_mode, so BowEcho does not fake numeric substitutes.",
    );
    cite(ui, "Deeper reference: docs/wrf-simulated-radar.md.");
}

// ---------------------------------------------------------------------------
// 6. CM1

fn cm1(ui: &mut egui::Ui) {
    ui.heading("CM1");
    para(
        ui,
        "Windows \u{25be} \u{25b8} CM1 opens the native NCAR CM1 workspace. It inventories \
         complete local-Cartesian cm1out files directly; CM1 data never passes through the \
         WRF reader. Modern r20.3+, legacy r18/r19 and legacy COARDS topologies are recognized, \
         while one-file MPI tiles are identified as needing assembly.",
    );

    subhead(ui, "NORMAL WORKFLOW");
    action(
        ui,
        "1. Open cm1out file…",
        "— inspect native axes, output records, variables, units, staggering and complete-domain \
         status. A multi-record file opens on its final, latest evolved record; record index 0 remains \
         selectable for initialization-state inspection. Unsupported shapes stay listed with a reason.",
    );
    action(
        ui,
        "2. Choose a native plane",
        "— select one 2-D field or one native level of a 3-D field. Official staggered u/v/w \
         are averaged from adjacent Arakawa-C faces onto the scalar grid, with the transform \
         retained in provenance.",
    );
    action(
        ui,
        "3. Place the domain",
        "— enter an explicit domain-center latitude/longitude, then choose Follow domain or an \
         available Fixed world placement. BowEcho never treats CM1 ctrlat/ctrlon as a map \
         projection or silently invents cell locations.",
    );
    action(
        ui,
        "4. Store in Models",
        "— store the selected plane, or choose All records (loop) for a strictly ordered \
         multi-record file. Exact elapsed seconds plus the official simulation start become \
         the model time axis; every loop frame must share the same placed grid.",
    );

    subhead(ui, "MOVING DOMAINS");
    para(
        ui,
        "Follow domain pins a storm-following computational grid to the chosen anchor. Fixed \
         world preserves exact source displacement. If a moving run has matching official \
         cm1out_diag_XXXXXX.nc files beside it, Attach exact diagnostic positions matches them \
         by elapsed time. BowEcho does not integrate umove/vmove into purported geolocation. \
         Moving Fixed-world records cannot share one stored loop grid.",
    );

    subhead(ui, "NATIVE AND METEOROLOGICAL COLUMNS");
    action(
        ui,
        "Read native column",
        "— shows any compatible 3-D field at one scalar x/y cell in native model-level order. \
         It preserves nominal height and an available zhval column without inventing pressure \
         levels or calling the undeclared vertical datum MSL.",
    );
    para(
        ui,
        "Meteorological profile readiness is stricter: exact unit-bearing th, prs, qv, zhval, \
         horizontal winds and a defensible wind-frame correction must all be present. Derivation \
         of pressure, temperature, dewpoint and earth-relative wind also requires an explicit \
         opt-in to CM1's default Rd/Cp/Rv constants because cm1out does not record testcase. \
         The result remains a native table; the MSL-labelled Sounding viewer is not enabled.",
    );

    subhead(ui, "SCALAR NATIVE REF/VEL");
    para(
        ui,
        "Build native REF/VEL in Radar requires an assembled complete domain with native 3-D \
         dbz, physical zhval, horizontal and vertical winds, exact time and explicit placement. \
         Native zs supplies terrain; when it is absent, only an explicit flat-idealized-domain \
         choice may use model-z = 0. The antenna is fixed at the placed CM1 domain center, so a \
         saved WRF/NEXRAD site cannot land outside the idealized grid. Compatible scan, range, \
         gate, blockage, noise and presentation controls come from the WRF simulated-radar panel. \
         Choose Selected record index for one radar frame, or All records (ordered loop) for one \
         exact-time radar loop. The selected record is processed first and its first completed \
         tilt opens while the remaining tilts and records process.",
    );
    para(
        ui,
        "Each radar frame samples one frozen CM1 record on CPU with standard 4/3-Earth geometry \
         and the file's native scalar dbz. Multi-record loops read and release one full native \
         scene at a time, then install completed radar volumes in exact CM1 time order. It does \
         not interpolate the atmosphere between records, extrude 2-D cref, assemble MPI tiles, \
         synthesize dual-pol, run T-matrix or WRF refractivity, recompute reflectivity, or use \
         adjacent-WRF interpolation.",
    );
    cite(
        ui,
        "Schema reference: NCAR CM1 writeout_nc.F at commit \
         a33cd28c206adb010995f3ffb65aada150d9b1b9. Practical walkthrough: docs/cm1-guide.md.",
    );
}

// ---------------------------------------------------------------------------
// 7. Formula Lab

fn formula_lab(ui: &mut egui::Ui) {
    ui.heading("Formula Lab");
    para(
        ui,
        "Windows \u{25be} \u{25b8} Formula Lab opens a dockable, first-class workspace for \
         deterministic custom model diagnostics. It evaluates a bounded, unit-aware expression \
         language through the pinned wrf-formula engine; it is not arbitrary Rust, Python, shell \
         code, or a plugin runner.",
    );

    subhead(ui, "NORMAL WORKFLOW");
    action(
        ui,
        "1. Choose the source",
        "— Stored model follows the current Models run/time. Raw WRF uses a chosen readable \
         wrfout file and time index.",
    );
    action(
        ui,
        "2. Insert exact fields",
        "— use an enabled Quick start or click a searchable field-browser row. Do not assume \
         that two models use the same token or that a display-only synthesized layer exists in \
         the underlying store.",
    );
    action(
        ui,
        "3. Compile the equation",
        "— Syntax valid proves that the bounded language parsed and planned the expression. It \
         does not prove that this dataset contains compatible fields.",
    );
    action(
        ui,
        "4. Read source readiness",
        "— Ready for selected source additionally checks known fields, dimensions, units, \
         pressure axes, required geometry, cadence and time availability. Raw WRF performs final \
         file-specific resolution when evaluation starts.",
    );
    action(
        ui,
        "5. Evaluate and display",
        "— work runs in the background. The result enters Models and can be added to the radar \
         map, opened in the native plot/PNG workflow, or styled with a model color table.",
    );

    subhead(ui, "LANGUAGE");
    para(
        ui,
        "A program contains zero or more newline- or semicolon-delimited assignments and one \
         final expression. There is no implicit multiplication. ^ binds more tightly than unary \
         minus, so -2^2 means -(2^2). Chained comparisons are rejected; combine comparisons with \
         and/or instead. Scalars broadcast to fields, but two fields must have compatible labeled \
         shapes and grid locations.",
    );
    para(
        ui,
        "Arithmetic and comparisons use +, -, *, /, ^, ==, !=, <, <=, > and >=. Boolean \
         operators are and, or and not. Constants include pi, e, true and false. The function \
         families include where/min/max/clamp; common math and trigonometry; quantity/convert; \
         explicit dBZ conversion; vector construction; horizontal and vertical calculus; and dt.",
    );

    subhead(ui, "WORKED EXAMPLES");
    formula_example(ui, "Stored HRRR/GFS/WRF wind:", GUIDE_FORMULA_EXAMPLES[0]);
    formula_example(ui, "Stored dewpoint depression:", GUIDE_FORMULA_EXAMPLES[1]);
    formula_example(ui, "Stored 3-km temperature:", GUIDE_FORMULA_EXAMPLES[2]);
    formula_example(ui, "Raw WRF divergence:", GUIDE_FORMULA_EXAMPLES[3]);
    formula_example(ui, "Raw WRF vertical vorticity:", GUIDE_FORMULA_EXAMPLES[4]);
    formula_example(ui, "Raw WRF 3-km temperature:", GUIDE_FORMULA_EXAMPLES[5]);
    formula_example(
        ui,
        "Raw WRF temperature tendency:",
        GUIDE_FORMULA_EXAMPLES[6],
    );
    formula_example(
        ui,
        "Linear-Z reflectivity experiment:",
        GUIDE_FORMULA_EXAMPLES[7],
    );
    para(
        ui,
        "Examples are capability demonstrations, not promises that every run contains those \
         tokens. HRRR commonly supplies composite reflectivity; GFS normally does not. The \
         stored quick starts adapt to the exact selected inventory. A stored height_iso field \
         labeled gpm may require accepting the offered conservative gpm \u{2192} m interpretation \
         before the explicit-height example becomes Ready.",
    );

    subhead(ui, "UNITS ARE PART OF THE EQUATION");
    para(
        ui,
        "Inputs are normalized to coherent SI. Addition, comparison and where branches require \
         compatible dimensions. Absolute temperature and temperature differences have different \
         arithmetic rules. quantity(value, \"unit\") creates a physical scalar and convert(value, \
         \"unit\") requests explicit output units.",
    );
    para(
        ui,
        "dBZ is logarithmic: multiplication, division, powers and derivatives require an \
         explicit dbz_to_z conversion to linear reflectivity and z_to_dbz to return. Expected \
         output units in recipe metadata are validated rather than used as an unchecked label.",
    );
    para(
        ui,
        "A raw-field unit override is a scientific assertion, not a converter. BowEcho offers \
         only conservative equivalent-label suggestions such as gpm \u{2192} m or fraction \
         \u{2192} 1. A scale-changing case such as kPa versus Pa receives no automatic relabeling; \
         fix the data or write an explicit conversion. Remove a stale override once the store \
         itself carries recognized units.",
    );

    subhead(ui, "STORED MODEL VS RAW WRF");
    action(
        ui,
        "Stored model",
        "— model-slug-neutral pointwise 2-D algebra over the fields actually present in any \
         compatible stored run, including HRRR, GFS/GEFS, AI-GFS/AI-GEFS, HGEFS, ECMWF Open \
         Data, RAP, NAM, RRFS, NBM and imported WRF. Pressure-volume fields can use \
         mean_z/integrate_z/interpolate_z when an explicit compatible height field is supplied. \
         dt works only on a complete, distinct, increasing, host-verified time axis.",
    );
    action(
        ui,
        "Raw WRF",
        "— supplies DX/DY, MAPFAC_M, physical height, projected vector basis and native time \
         information. This unlocks ddx, ddy, grad, div, curl, laplacian, default-height ddz and \
         full explicit-height vertical operators.",
    );
    para(
        ui,
        "rw-store does not persist horizontal spacing/map factors, a default 3-D physical-height \
         coordinate, or vector-basis metadata. Formula Lab therefore blocks horizontal calculus \
         for stored runs instead of inventing geometry. Its exact stored inventory is authoritative; \
         the Raw WRF browser is a useful common-field list, while the file resolver remains the \
         authority for dimensions and availability.",
    );

    subhead(ui, "DIMENSIONS, VERTICALS & TIME");
    para(
        ui,
        "A display result must resolve to a 2-D [Y, X] field. A bare pressure/native 3-D input \
         is blocked until mean_z, integrate_z or interpolate_z reduces it. Stored pressure inputs \
         must share an identical pressure axis. Vertical bounds are never silently clipped.",
    );
    para(
        ui,
        "WRF ddx/ddy are mass-point differences scaled by MAPFAC_M / DX or DY; on 3-D fields \
         they follow terrain model levels, not constant geometric height. Two-dimensional \
         divergence and curl use conformal metric forms, and laplacian is the conformal 2-D \
         surface Laplace-Beltrami operator. Three-dimensional grad/div/curl/laplacian remain \
         rejected because the full terrain-coordinate metric terms are not implemented.",
    );
    para(
        ui,
        "dt uses the actual nonuniform time coordinate and a three-time Lagrange stencil where \
         available. Endpoints depend on the chosen boundary policy. Nested dt is rejected. Raw \
         WRF dt requires adjacent times inside the one selected multi-time file; selecting \
         separate single-time wrfout files does not assemble a Formula Lab time axis. Moving \
         nests or changing grids require explicit remapping rather than fixed-index time differencing.",
    );

    subhead(ui, "POLICIES, RECIPES & RESOURCE LIMITS");
    para(
        ui,
        "Evaluation options choose boundary behavior (one-sided second order, missing, or error), \
         missing-value behavior, and non-finite behavior. Ignore in reductions applies only to \
         reductions; it is not a general license to erase bad data. The standard desktop profile \
         is bounded; the explicit Large research profile raises the documented memory/work meter \
         but does not remove immutable host ceilings.",
    );
    para(
        ui,
        "A selected raw WRF file at least 1 GiB also shows a separate memory-cost consent box. \
         Evaluation stays disabled until the \"I understand the memory cost; allow evaluation\" \
         box is checked for that exact file revision.",
    );
    para(
        ui,
        "Open recipe\u{2026} and Save recipe\u{2026} use the portable wrf-formula/v1 JSON schema. \
         Recipes can carry parameters, expected units, authors, references, tags, required fields, \
         cadence/spacing/vertical-level requirements and stricter resource ceilings. Untrusted \
         recipe input is size-bounded and compile-validated before it becomes active.",
    );

    subhead(ui, "SAFETY, PROVENANCE & OUTPUT");
    para(
        ui,
        "The language has no filesystem, network, shell, imports, loops, recursion, arbitrary \
         code execution or unreviewed global solvers. Source, tokens, AST, dependencies, output \
         elements, memory and operations are metered.",
    );
    para(
        ui,
        "Last result provenance records the engine version, source fingerprint, valid time, \
         input identity, recipe/version, and every requested \u{2192} resolved field with shape \
         and effective units. Warnings remain attached to the result card. If the selected source, \
         file revision, equation, options, parameters or output changes during background work, \
         the stale result is discarded instead of being displayed.",
    );
    para(
        ui,
        "A new result receives a finite-range color scale unless an exact saved output-name \
         binding supplies a user color table. Formula output can then use the same Models viewer, \
         map layer, native plot, Save PNG and color-table tools as an ingested field.",
    );
    cite(
        ui,
        "Detailed reference: docs/formula-lab.md; engine contract: wrf-formula/v1.",
    );
}

// ---------------------------------------------------------------------------
// 8. Satellite

fn satellite(ui: &mut egui::Ui) {
    ui.heading("Satellite");
    para(
        ui,
        "Windows \u{25be} \u{25b8} Satellite opens the satellite window: GOES live follow on \
         top, other satellite sources below, and a frame player for anything written to the \
         local satellite store.",
    );

    subhead(ui, "LIVE FOLLOW");
    para(
        ui,
        "Pick the satellite (GOES-19 East, GOES-18 West, GOES-16), sector (CONUS, Full disk, \
         Meso 1, Meso 2), and any of the 16 ABI bands. Start polls the NOAA open-data bucket \
         at the sector's native cadence and keeps a rolling local store. BowEcho uses its own \
         sat store, so it can run alongside other tools without corrupting their caches.",
    );

    subhead(ui, "OTHER SATELLITE SOURCES");
    para(
        ui,
        "Himawari-9 IR/WV bands can be loaded from NOAA public buckets for Asia/Pacific \
         context. The True color button composes AHI true color (real 0.51 µm green, not a \
         synthesized one) at the scope you pick: the west-Pacific tropics region, or the WHOLE \
         disk at ~4 km or ~2 km effective. GOES adds its own RGB composites (GeoColor, \
         NaturalColor, …) over the current sector. The source chips change the controls above \
         the one shared player; stored loops from every provider remain available below.",
    );

    subhead(ui, "METEOSAT-12 / EUMETVIEW");
    para(
        ui,
        "Choose Meteosat-12 for public, live MTG-I1 imagery rendered by EUMETSAT EUMETView — \
         no account or API key is required. Load latest fetches the newest image; Load loop \
         discovers recent service timestamps and fetches the selected number of frames. Normal \
         imagery is available every 10 minutes and the LI Accumulated Flash Area product every \
         5 minutes. The product picker includes Geo Colour and True Colour RGB, HRFI IR10.5 and \
         VIS0.6, Cloud Phase, Cloud Type, Dust, Fire Temperature, Fog / Low Clouds, Snow, and \
         LI Accumulated Flash Area.",
    );
    para(
        ui,
        "Lightning AFA is a five-minute gridded flash-footprint accumulation comparable to \
         GLM flash-extent density, not a count of individually plotted flashes. Select it to \
         expose the official MTG LI color-legend link.",
    );
    para(
        ui,
        "Each returned Meteosat frame enters the same local satellite store and frame player as \
         GOES and Himawari. Equal product, geographic grid, and UTC day values join one loop. \
         Map follows player and Show on radar map work normally, and Native plot opens the \
         provider-rendered RGB frame on BowEcho's plotting surface for Save PNG. These are \
         rendered EUMETView images rather than raw FCI radiances, so the local IR enhancement \
         picker does not recolor them.",
    );
    para(
        ui,
        "BowEcho keeps the required source notice visible on Meteosat map layers and plot/PNG \
         output: ‘Contains modified EUMETSAT Meteosat data YEAR.’ The year follows the frame's \
         UTC date. Product metadata also records the EUMETView layer and EUMETSAT provider.",
    );

    if crate::eumetsat_credentials::DATA_STORE_ACCOUNT_UI_ENABLED {
        subhead(ui, "OPTIONAL EUMETSAT DATA STORE ACCOUNT");
        para(
            ui,
            "The public EUMETView imagery above never needs an account. The separate Data Store \
             account controls let you check a consumer key and consumer secret, save them in the \
             operating-system credential vault on this device, or forget them. Account checks mint \
             a short-lived access token on the satellite worker and do not retain that token. The \
             key and secret never enter BowEcho's settings file. This optional account connection is \
             not a raw FCI download path.",
        );
        action(
            ui,
            "Which values to enter",
            "— open EUMETSAT API Key Management, then copy Consumer Key into BowEcho's consumer-key \
             field and Consumer Secret into the consumer-secret field. Do not paste the temporary \
             Access Token: BowEcho requests and refreshes short-lived tokens from the saved pair.",
        );
        ui.hyperlink_to(
            "Open EUMETSAT API Key Management",
            "https://api.eumetsat.int/api-key/",
        );
        para(
            ui,
            "If the page has no key pair yet, sign in with an EUMETSAT User Portal account and \
             create one there. Save securely writes the pair to Windows Credential Manager, macOS \
             Keychain, or Linux Secret Service; Test account verifies it by requesting a token.",
        );
    }

    subhead(ui, "IR ENHANCEMENTS");
    para(
        ui,
        "The IR enhancement picker recolors brightness-temperature bands (GOES ABI and \
         Himawari AHI bands 7\u{2013}16) through an absolute-temperature color curve: CIMSS \
         (the default), the NESDIS BD Dvorak curve, AVN, Funktop, rainbow, or plain grayscale. \
         Himawari IR is calibrated to true Kelvin, so a 190 K overshooting top reads 190 K and \
         the Dvorak steps mean what they say. BowEcho colors each frame at ingest, so the \
         enhancement applies to newly fetched frames; a frame stored before the true-Kelvin \
         calibration keeps its legacy auto-stretch and the panel says so — load a fresh frame \
         to use the new enhancements.",
    );

    subhead(ui, "FOCUSED WINDOWS");
    para(
        ui,
        "Tick Focused window and set a center lat/lon and size to keep high-resolution loads \
         around one storm or region. Himawari and GOES true-color loads fetch/decode the \
         instrument-native pixels covering that box; Meteosat asks EUMETView for a bounded \
         2048-pixel geographic crop. Repeated loads of the same window and product stack into \
         one loopable run. Use map center copies the current radar-map center into the box.",
    );

    subhead(ui, "STORM SATELLITE (ONE PRESS)");
    para(
        ui,
        "Each tropical cyclone card carries \u{1f6f0} Vis and \u{1f6f0} IR buttons beside \
         \u{1f4cd} Focus. One press picks the covering geostationary satellite for that storm's \
         basin (Himawari-9 / GOES-East / GOES-West), pulls up to 10 recent native-resolution \
         frames centered on the storm, and opens the loop in the Satellite window while \
         following the newest frame onto the radar map — true color for Vis (daylight side \
         only), Band-13 brightness temperature with your chosen IR enhancement for IR (day and \
         night). One storm satellite load runs at a time.",
    );

    subhead(ui, "HURRICANE HUNTERS (LIVE HDOB)");
    para(
        ui,
        "Enable Hurricane Hunters (live HDOB) beside Tropical cyclones in Settings to show \
         active USAF and NOAA reconnaissance. Cyan is Air Force; orange is NOAA. The line is \
         the track accumulated during this BowEcho session, spaced meteorological barbs are \
         30-second flight-level winds, and the aircraft glyph marks the newest report. The \
         compact status also shows pressure and T/Td when those fields pass the bulletin's \
         quality flags. BowEcho polls the four official Atlantic and East/Central Pacific NHC \
         feeds every ~45 seconds only while enabled, deduplicates successive bulletins, and \
         removes missions whose newest valid report is more than 12 hours old.",
    );
    ui.horizontal_wrapped(|ui| {
        ui.hyperlink_to(
            "NHC aircraft reconnaissance",
            "https://www.nhc.noaa.gov/recon.php",
        );
        ui.label("·");
        ui.hyperlink_to(
            "Official HDOB specification",
            "https://www.nhc.noaa.gov/pdf/HDOB-specification.pdf",
        );
    });

    subhead(ui, "FRAME PLAYER");
    para(
        ui,
        "Browse the fetched runs and step or play through frames below the follow panel; the \
         refresh control re-scans the store for frames written since.",
    );

    subhead(ui, "NATIVE PLOTS & EXPORT");
    para(
        ui,
        "Native plot sends the current real or simulated satellite frame through the same \
         projected plotting surface used by the Model window. IR and water-vapor plots retain \
         raw Kelvin values and a physical colorbar; derived products retain raw mm/K/optical-depth \
         values with fixed cross-frame palettes; true-color composites and provider-rendered \
         EUMETView products stay RGB and omit a false scalar legend. Save PNG in that plot \
         window exports the plotted product with its title, valid time, and map context.",
    );

    subhead(ui, "SHOW ON RADAR MAP");
    action(
        ui,
        "Show on radar map",
        "— puts the current frame under the radar as a map layer. Opacity and removal live in \
         the Map tab's GOES row.",
    );

    subhead(ui, "LIGHTNING");
    para(
        ui,
        "The Lightning layer is GOES GLM: BowEcho chooses East or West from the map longitude, \
         fetches the newest granules first, age-fades flashes, and reads the rolling store for \
         loaded radar loops. Live means the newest flash is inside the live-age gate; stale \
         status means the layer is waiting for newer GLM files, not that radar data is broken. \
         EUMETView's LI Accumulated Flash Area is available as a rendered raster in the \
         Satellite window; the dedicated point-flash Lightning overlay remains GOES GLM.",
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
// 9. SimSat

fn simsat(ui: &mut egui::Ui) {
    ui.heading("SimSat");
    para(
        ui,
        "Windows \u{25be} \u{25b8} SimSat opens the embedded SimSat 0.2.1 renderer. It turns \
         WRF or HRRR native-level model atmospheres into physically based visible, thermal, \
         water-vapor and derived satellite products. Durable CPU renders enter BowEcho's normal \
         Satellite player; a separate one-frame GPU preview is available for visual iteration.",
    );

    subhead(ui, "QUICK WORKFLOW");
    action(
        ui,
        "1. Source",
        "— choose Local WRF / GRIB, Downloaded HRRR, or Download HRRR. Local accepts one file, \
         a multi-time file, a SimSat run.json, or a folder sequence.",
    );
    action(
        ui,
        "2. Product and view",
        "— choose the product, geostationary or top-down geometry, satellite viewpoint, and \
         Model native / ABI 1 km / ABI 2 km resolution.",
    );
    action(
        ui,
        "3. Render controls",
        "— start with Recommended, High Quality or Sensor QA when compatible, then choose Final \
         (384 steps) or Preview (192 steps), earth margin, and the visible-family \
         atmosphere/cloud/lighting controls when applicable.",
    );
    action(
        ui,
        "4. Render to Satellite",
        "— runs the full CPU path, writes every successful frame to the satellite store, opens \
         the shared player, and groups a sequence by source run instead of making one run per \
         forecast time.",
    );
    action(
        ui,
        "5. Inspect or export",
        "— use the Satellite player/map layer, or Open native plot for the projected plotting \
         surface and Save PNG.",
    );

    subhead(ui, "QUICK MODES & INTENT");
    action(
        ui,
        "Recommended Display",
        "— restores the reviewed visible baseline without changing source or product: CPU \
         offline quality, Model native, CompactU8, exposure 1.5, AOD 0.05, cloud OD 0.15, \
         deterministic two-subcolumn clouds, fixed particle optics, the bounded 4.0 low-sun \
         land-normalization gain, ground lift 1.10, the tightly gated twilight recovery \
         (0.30 / 0.50 / 4.0), exposed-edge feathering and top-down shadow anti-aliasing on. \
         Experimental footprints and the legacy broad post-light toe remain off.",
    );
    action(
        ui,
        "High Quality Visible",
        "— Recommended Display plus the deterministic four-subcolumn reference and 0.45 \
         highlight knee. It is slower and remains explicit rather than silently enabling other \
         experimental physics.",
    );
    action(
        ui,
        "Sensor QA",
        "— accepts Visible or GOES-East IR Band 13 only. It selects exact GOES-R navigation \
         and neutral visible transforms or the official FM4/GOES-19 Band 13 response; invalid \
         product/platform combinations are refused rather than relabeled.",
    );
    para(
        ui,
        "Display intent preserves the reviewed SimSat look. Sensor Fast Gray applies the strict \
         simsat-fast-gray-v1 operator on a temporary request, reports each neutralized display \
         transform, and requires CPU. It is not a complete ABI/AHI channel simulator. Manual \
         edits remain available and change the Quick mode label to Custom.",
    );
    para(
        ui,
        "Quick modes preserve the source, product, earth margin, forced/automatic ground month \
         and what-if sun override. Recommended and High Quality also preserve view, satellite \
         and navigation; Sensor QA selects its required geometry. The Current label describes \
         only preset-owned controls. For an actual-time baseline, separately choose Auto ground \
         and turn off Override sun.",
    );

    subhead(ui, "INPUTS");
    action(
        ui,
        "Local WRF / GRIB",
        "— ordinary extensionless wrfout files from any domain are accepted; no filename \
         pattern or extension is required. Folders are probed and sorted by valid time.",
    );
    action(
        ui,
        "Downloaded HRRR",
        "— reuses a previously retained HRRR native-level file from SimSat's input directory \
         or BowEcho's model cache.",
    );
    action(
        ui,
        "Download HRRR",
        "— selects date, cycle and forecast hour, downloads the NOAA wrfnat product with \
         resumable cache reuse, then renders it.",
    );
    para(
        ui,
        "HRRR must be the full native-level wrfnat product because SimSat needs its vertical \
         cloud, moisture and thermodynamic structure. BowEcho's smaller pressure + surface \
         model download cannot reconstruct that volume. Retained wrfnat files remain reusable \
         without another download.",
    );

    subhead(ui, "PRODUCTS");
    action(
        ui,
        "Visible true color",
        "— physically lit RGB with atmosphere, volumetric clouds, seasonal ground, terrain \
         shadows and water glint.",
    );
    action(
        ui,
        "SimSat day / night color (GeoColor style)",
        "— broad RGB by day, band-13 IR at night, blended across the modeled terminator. It is \
         not yet sensor-derived ABI GeoColor.",
    );
    action(
        ui,
        "Sandwich",
        "— visible texture plus enhanced cold cloud tops for daytime convection.",
    );
    action(
        ui,
        "IR 10.3 \u{00b5}m / WV 6.2, 6.9, 7.3 \u{00b5}m",
        "— true-Kelvin brightness-temperature fields for clean-window cloud tops and \
         upper/mid/lower-tropospheric water vapor.",
    );
    action(
        ui,
        "Precipitable water / Cloud-top temperature / Cloud optical depth",
        "— raw map-registered scalar fields in mm, K and dimensionless optical depth with \
         fixed physical palettes across frames.",
    );

    subhead(ui, "VIEW & RESOLUTION");
    para(
        ui,
        "Geostationary uses the GOES-East, GOES-West or Himawari fixed-grid viewpoint and keeps \
         an optional earth margin around the finite model domain. Top-down is north/map-registered \
         to the source projection and ignores satellite choice. Model native keeps one output \
         pixel per source cell; ABI 1 km / 2 km use physical spacing in top-down view and scan \
         pitch in geostationary view while preserving aspect ratio at the output cap.",
    );
    para(
        ui,
        "Top-down visible output now directly marches the camera-to-cloud atmospheric column. \
         Its composite includes transmitted surface, transmitted cloud radiance and the \
         modeled airlight in front of the cloud on both CPU and GPU paths. This improves the \
         earlier no-front-airlight simplification; it remains SimSat's approximate atmosphere, \
         not a measured or line-by-line sensor retrieval.",
    );
    para(
        ui,
        "Navigation can retain the shipped WRF/model sphere or use opt-in exact GOES-R \
         ellipsoid/sweep-x geometry. Exact GOES-R navigation is CPU-only and unavailable for \
         Himawari. It improves registration geometry but does not imply sensor-exact radiometry.",
    );

    subhead(ui, "CPU OUTPUT VS GPU PREVIEW");
    action(
        ui,
        "Render to Satellite",
        "— the tested CPU quality path for every product and every stored frame/loop. A first \
         use ingests a reusable volume brick; a full HRRR native file can briefly require more \
         than 2 GiB of memory.",
    );
    action(
        ui,
        "GPU preview",
        "— a temporary visible-true-color first frame opened only in Native plot. It reports \
         every compatibility substitution, never changes saved controls, never enters the \
         satellite store, and never silently falls back to a stored CPU frame. Sensor Fast Gray, \
         ScienceCloudF16, exact GOES-R geostationary navigation and instrument footprints are \
         CPU-only. v0.2.1 keeps both post-light terrain controls visually matched on CPU/GPU.",
    );
    para(
        ui,
        "Cancel after current frame stops a sequence at the next safe boundary; an active render \
         finishes its current frame, and a resumable HRRR download may stop between chunks. \
         Successful frames already written remain available.",
    );

    subhead(ui, "ATMOSPHERE & CLOUD CONTROLS");
    action(
        ui,
        "Aerosol optical depth",
        "— visible AOD at 550 nm. Zero removes aerosol extinction while retaining molecular \
         Rayleigh scattering; RH aerosol swelling applies the documented humid-growth factor.",
    );
    action(
        ui,
        "Aerial veil / terrain atmosphere",
        "— controls finished daytime path airlight and shortens view/sunlight columns to model \
         terrain height. Terrain-height atmosphere is the physical shipped path.",
    );
    action(
        ui,
        "Use model cloud fraction",
        "— consumes WRF CLDFRA or HRRR's native 50-level cloud-fraction field when trustworthy; \
         missing coverage falls back conservatively. Turning it off restores horizontally full \
         cloudy cells wherever condensate is nonzero.",
    );
    action(
        ui,
        "Cloud optical-depth scale",
        "— visible-cloud sensitivity. The shipped cross-file visual calibration is 0.15; 1.00 \
         is unscaled model extinction. It does not modify thermal output or the quantitative \
         cloud-optical-depth product.",
    );
    action(
        ui,
        "Cloud transport",
        "— Legacy octaves is the shipped bright-anvil path; Single scatter is a dim diagnostic; \
         delta-flux v1/v2/v3 are explicitly experimental, CPU-only research closures.",
    );
    action(
        ui,
        "Fractional-cloud closure",
        "— Deterministic 2 is the reviewed finished-display default: two fixed-stratified \
         shared-u maximum-overlap cloud marches averaged in linear radiance before one tonemap. \
         Effective OD remains the fast explicit sensor-compatible closure; Deterministic 4/8/16 \
         remain higher-cost reference/convergence operators, not full stochastic McICA.",
    );
    action(
        ui,
        "Particle optics",
        "— Fixed radii is the production path. NSSL MP18 and HRRR Thompson native-moment \
         experiments use per-cell fallback and separate caches; they are visible-only because \
         thermal mass recovery remains tied to fixed radii.",
    );
    action(
        ui,
        "Edge feather / granulation",
        "— exposed-edge feathering fades only finished visible clouds where the camera reveals \
         the finite model boundary. Granulation is subtract-only experimental appearance detail \
         and cannot create scientifically resolved structure.",
    );
    action(
        ui,
        "Top-down stratiform reconstruction",
        "— optional, experimental and off by default. It can reduce source-grid rings in broad \
         low liquid decks while conserving selected-area optical depth; geostationary, raw-band, \
         thermal and derived products ignore it.",
    );
    action(
        ui,
        "Top-down cloud footprint",
        "— optional seven-tap pre-tonemap cloud-radiance footprint that leaves terrain sharp. \
         It is experimental, CPU-only and ignored by geostationary, thermal and derived output.",
    );
    action(
        ui,
        "Top-down shadow anti-aliasing",
        "— Recommended Display applies a normalized 5 by 5 binomial filter to the ground \
         cloud-shadow field in transmittance space. The sun-OD march also permits up to 4096 \
         samples, reducing coherent dash/ring artifacts on fine vertical grids. It does not \
         alter cloud radiance, add sub-grid weather or claim an instrument footprint.",
    );
    action(
        ui,
        "Override sun (what-if)",
        "— replaces the valid-time sun with a selected elevation/azimuth. The UI labels this \
         non-physical visualization override so it cannot be mistaken for model time.",
    );

    subhead(ui, "GROUND & DISPLAY");
    para(
        ui,
        "Seasonal NASA Blue Marble Next Generation imagery supplies the ground, with missing \
         2 km months downloaded lazily and hash-verified. Auto blends by valid date; forcing a \
         month is a what-if surface. Exposure, ground lift, highlight compression and land \
         visibility controls affect finished visible-family RGB only, not raw visible bands, \
         IR, water vapor or derived scalar fields.",
    );
    para(
        ui,
        "Restore shipped display calibration resets the visible display-tuning controls without \
         changing the source or product. SimSat 0.2.1 removes an unintended second \
         limb-darkening factor from surface illumination, uses a restrained surface-only \
         ground lift of 1.10, and enables tightly gated twilight terrain recovery: it fades in \
         from -6 to 0 degrees, is full through +4, and returns to identity by +12. The legacy \
         broad post-light toe remains a default-off experiment. Both post-view controls use the \
         same formula on CPU/GPU and leave water, glint, cloud radiance, raw fields and Sensor \
         Fast Gray unchanged.",
    );
    para(
        ui,
        "CIMSS Style is SimSat 0.2.1's recommended Band-13 false-color isotherm display. \
         Explicitly entering an IR or WV product selects CIMSS; startup in the same persisted \
         product preserves the saved palette. Natural (NOAA heritage) remains available as \
         NOAA's continuous bi-linear longwave grayscale. Both recolor the displayed Kelvin \
         plane only; neither changes the thermal operator.",
    );

    subhead(ui, "SENSOR & PRECISION CONTROLS");
    action(
        ui,
        "ScienceCloudF16",
        "— CPU-only log2-f16 hydrometeor-extinction storage in an isolated v7 cache. CompactU8 \
         v6 remains the production default; switching profiles re-ingests the retained source.",
    );
    action(
        ui,
        "FM4 / GOES-19 Band 13 response",
        "— integrates Planck emission through NOAA's official FM4 spectral response and uses \
         it for BT inversion. Cloud/gas absorption remains SimSat's gray approximation and is \
         labeled as a science limitation.",
    );
    action(
        ui,
        "ABI Band 13 MTF footprint",
        "— experimental GOES-16 east-west MTF-informed response applied to complete FM4 \
         radiance. It selects exact GOES-R + ABI 2 km + CPU; transfer to GOES-19 is unvalidated \
         and north-south/temporal/detector effects remain unmodeled.",
    );

    subhead(ui, "CACHE, LOOPS & OUTPUT");
    para(
        ui,
        "SimSat 0.2.1 keeps compact production SSB cache format v6; ScienceCloudF16 uses \
         a disjoint v7 cache. This release does not force a compact-cache format bump. Older \
         bricks must be ingested once again from their original WRF/HRRR source; a \
         cached-only brick cannot be upgraded without that source. A retained wrfnat source \
         re-ingests without downloading again.",
    );
    para(
        ui,
        "Product, view, quick/science, atmosphere, cloud, lighting and display choices persist \
         in BowEcho settings. A v0.2.0 pane payload migrates without changing its image: the \
         saved ground lift is kept and twilight recovery remains off until Recommended is \
         deliberately applied. Source paths, active jobs, progress, errors and rendered output \
         are session-only.",
    );
    para(
        ui,
        "CPU frames share the real-satellite store and player. Equal source run, product, view \
         and UTC-day values join one loop. Resolution is not a separate run key, so rerendering \
         the same source/product/view at another resolution can replace that valid-time frame. \
         Map follows player and Show on radar map work normally. Native plots keep Kelvin or \
         derived scalar values and physical colorbars; RGB products omit a false scalar legend.",
    );
    para(
        ui,
        "After a successful render the SimSat pane reports operator, storage, intent adjustment, \
         sensor, footprint and science-limit notices. The native-plot title identifies SimSat, \
         but those full notices and NASA ground credit are not currently embedded into Satellite \
         frames or PNG metadata; the Sources chapter and docs/simsat-guide.md are the attribution \
         record.",
    );

    subhead(ui, "HONEST SCIENCE BOUNDARIES");
    para(
        ui,
        "Clouds and weather exist only inside the model domain; margin shows real ground under \
         clear sky, not extrapolated weather. Model-sphere geometry follows WRF; exact GOES-R \
         navigation is available separately but does not imply exact sensor radiometry. A coarse \
         model cannot provide cloud-edge structure below its grid scale.",
    );
    para(
        ui,
        "Visible rendering uses a physically based clear-sky/cloud approximation, not a full \
         atmospheric chemistry model. IR and water-vapor products use documented gray, \
         band-averaged absorption rather than line-by-line radiative transfer. SimSat day/night \
         color is GeoColor-style broad RGB, not sensor-derived ABI GeoColor; its night side is \
         IR with no city-lights layer. CIMSS/Natural are display palettes, and shadow \
         anti-aliasing plus the 4096-step cap are sampling improvements rather than new \
         meteorology. Twilight recovery is a bounded finished-visible display control, not new \
         terrain science. Precision, native-optics, footprint, \
         granulation, delta-flux, reconstruction and sun experiments remain opt-in and labeled.",
    );
    cite(
        ui,
        "Implementation references include Hillaire sky atmosphere, Frostbite/Nubis cloud \
         rendering, Wrenninge multi-scatter, Cox-Munk water glint, and NASA Blue Marble. \
         Detailed workflow: docs/simsat-guide.md.",
    );
}

// ---------------------------------------------------------------------------
// 10. Archive & events

fn archive(ui: &mut egui::Ui) {
    ui.heading("Archive & events");
    para(
        ui,
        "The Data tab's Archive section follows the primary radar. For a US site it replays \
         any day in the US NEXRAD Level II record — the loop transport sits at the top of the \
         tab so you never switch tabs to play what you just loaded. For an international site \
         whose provider exposes an archive (EUMETNET ORD, SMHI Sweden, Australia NCI), the \
         same browser lists \
         that provider's holdings; providers without an archive say so with a reason instead \
         of silently listing a US site. Data also holds Live feeds (GR2A-style dir.list \
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
         the current frame limit); Single loads just that scan. +5 earlier extends a loaded loop further \
         back in time.",
    );

    subhead(ui, "UNIFIED PLAYER WINDOWS");
    para(
        ui,
        "For long loops, use Windows > Player. It can load latest frames, a recent loop, an \
         archive window, or an archive window ending at a selected time. Frame limits go up \
         to 2000, playback speed goes up to 64x, and warning/model/satellite/lightning sync \
         options live with the timeline instead of being scattered through the sidebar.",
    );

    subhead(ui, "RADAR COVERAGE EXPLORER");
    para(
        ui,
        "Data > Radar coverage is the provider-aware picker for international and research \
         sources. It shows Live / Loop / Archive / Dual-pol / Experimental badges, lets you \
         probe a site/time without loading data, and its Browse archive button points the \
         Archive browser above at any archive-capable provider site. Treat archive badges as \
         capability evidence, not a promise that every country has deep history through \
         BowEcho yet.",
    );

    subhead(ui, "DATA PACKS");
    para(
        ui,
        "Data packs are ready-made review scenes for important dual-pol and debug cases. Load \
         one to fetch the needed archive frames, apply the intended scene, then expand the \
         window if you want more context.",
    );

    subhead(ui, "SPC TORNADO EVENTS");
    action(
        ui,
        "Tornadoes (SPC) \u{25b8} Fetch",
        "— pulls the SPC filtered tornado reports for the date (SPC's 12Z\u{2013}12Z \
         convective day), pins the Event Day track layer, and shows EF-colored track lines \
         where surveyed geometry exists.",
    );
    action(
        ui,
        "Click a report",
        "— BowEcho picks the radar with the lowest beam over the report location, centers the \
         map there, loads the loop at the report time, and can auto-load the matching model \
         hour for soundings.",
    );

    subhead(ui, "EVENT EXPLORER");
    para(
        ui,
        "Pick a day, see everything that happened. With the SPC reports layer on, the report \
         dots FOLLOW the displayed radar time's convective day (12Z\u{2013}12Z — a 03Z report \
         belongs to the previous day's file) whenever you browse the archive, exactly like \
         the outlooks do. Tornado TRACKS draw as red begin\u{2192}end lines with a direction \
         arrowhead — surveyed paths from the SPC WCM tornado database; days the database \
         hasn't reached yet (the current year) show the torn report points instead.",
    );
    action(
        ui,
        "Event day \u{25b8} Load",
        "— pins the reports/outlook day from the DATA tab without moving the map, and shows \
         the day's count (\"N reports · M tornado segments\"). A day with no SPC file (quiet \
         or pre-2004) says so — no error spam. Unpin returns to following the displayed time.",
    );
    action(
        ui,
        "Click a tornado track",
        "— loads the lowest-beam WSR-88D at the track midpoint with a loop SPANNING the \
         track plus extra scans of context each side (the \u{b1} scans control on the Event \
         day row, default 5 before touchdown and 5 after the estimated lift), auto-playing \
         at the lowest tilt — and when the track's END is closer to a different radar, that \
         site loads as a second radar overlay at the event time.",
    );
    subhead(ui, "DEBUG CASES");
    action(
        ui,
        "\u{2699} Settings \u{25b8} Debug cases",
        GUIDE_DEBUG_CASES_TEXT,
    );
}

// ---------------------------------------------------------------------------
// 11. Unified Player

fn unified_player(ui: &mut egui::Ui) {
    ui.heading("Unified Player");
    para(
        ui,
        "The Unified Player is the full workspace behind the top bar's Timeline menu — \
         LIVE and Timeline cover go-live, archive loops, and sweep playback; choose Open \
         timeline, sync, and export (or Windows > Unified Player) for everything else. It owns long radar loops, archive \
         windows, low-sweep timelines, synced warnings/reports/lightning/models, multi-radar \
         mosaics, camera follow, and loop export. Every control that existed before the bar \
         still lives here.",
    );

    subhead(ui, "LOADING");
    action(
        ui,
        "Latest",
        "- loads the freshest frame for the active site without changing the rest of the \
         player setup.",
    );
    action(
        ui,
        "Loop",
        "- loads the requested number of recent frames. The menu offers common sizes up to \
         2000, and the numeric box accepts any value in that range.",
    );
    action(
        ui,
        "Archive window",
        "- loads a UTC start/end window. Ending-at is the fastest way to say 'give me N \
         frames ending at this time'.",
    );
    action(
        ui,
        "Add/sync",
        "- adds nearby radar sites as timeline-owned overlays. Mosaic 5 can coordinate up to \
         five sites and uses latest-at-or-before timing so sparse products hold their last \
         real sweep instead of showing future data.",
    );

    subhead(ui, "LOW SWEEPS");
    para(
        ui,
        "Low sweeps expands each volume into the real low-level cuts inside that scan. All \
         low shows every low sweep, same degree follows matching physical elevation, and base \
         only keeps the base tilt. This is useful for SAILS/MRLE-style rapid low-level cuts \
         without pretending every product updates at the same instant.",
    );

    subhead(ui, "TIME-SYNCED LAYERS");
    para(
        ui,
        "Warning sync, SPC reports, mPING, GLM lightning, satellite frames, and model fields \
         can follow the player time. Archive warning sync is explicit so live warning mode \
         stays live until you ask for historical warnings.",
    );

    subhead(ui, "CAMERA FOLLOW");
    para(
        ui,
        "Storm-track follow can lock the map to a detected track. Manual camera keyframes let \
         you place the center point on several frames and have BowEcho interpolate between \
         them. Hide guides keeps storm/tornado guide lines out of clean captures while follow \
         mode is active.",
    );

    subhead(ui, "EXPORT");
    para(
        ui,
        "Loop record/export uses the player timeline rather than wall-clock speed. Use full \
         resolution for release-quality video, and set export speed separately when you want \
         a 1500-step loop to play back faster than real time.",
    );
}

// ---------------------------------------------------------------------------
// 12. Tools & inspector

fn tools(ui: &mut egui::Ui) {
    ui.heading("Tools & inspector");

    subhead(ui, "INSPECTOR CARD");
    para(
        ui,
        "The floating card at the cursor reads the data under it: product value with units; \
         on velocity products an in/outbound arrow at the probed gate plus an automatic \
         couplet probe (Vrot, \u{394}V, separation) when one is nearby; raw VEL with the \
         Nyquist and a fold warning; range @ azimuth and tilt; beam height; and the model \
         value. Pick exactly which lines you want via the Inspector\u{2026} menu in TOOLS.",
    );
    action(
        ui,
        "Shift+click",
        "— pins the card to a geo point: it sticks through pan/zoom and re-reads every new \
         volume (watch one spot evolve through a loop). Shift+click near the pin releases it. \
         In grid layouts the pin works on the main pane.",
    );

    subhead(ui, "WHY THIS SYNTHETIC GATE?");
    action(
        ui,
        "Right-click a synthetic gate \u{2192} Why this synthetic gate?",
        "\u{2014} opens an exact selected-gate report. The fast embedded path shows the retained \
         cut/radial/gate geometry, ray acquisition time, range, Nyquist, MCOV/TUNB/MSIG support \
         fractions, available Ideal/Measured/Presented values and volume/operator provenance. \
         A field with different geometry stays unavailable; BowEcho does not substitute a \
         neighboring row or gate.",
    );
    para(
        ui,
        "Range-dependent sensitivity, estimator uncertainty/noise draws, hydrometeor \
         contributions and true/aliased/noise/measured Doppler spectra are not recoverable from \
         an ordinary RadarVolume. The inspector shows them only when a generation-matched real \
         retained-source GateExplanation exists. Otherwise it says why the source explanation is \
         unavailable; it never infers a species mixture or synthetic spectrum from REF, VEL or \
         dual-pol moments.",
    );
    para(
        ui,
        "The deep worker verifies the frame/config witness, reopens the exact retained WRF \
         source/time, and recomputes that radial from its first gate through the selected gate. \
         The prefix is required for cumulative PhiDP, attenuation and refracted blockage. Bulk \
         Rayleigh can then show individual hydrometeors and a selected-gate Doppler spectrum; \
         property T-matrix currently exposes only aggregate polar/instrument stages and marks \
         category decomposition and spectrum unavailable.",
    );

    subhead(ui, "BEST RADAR");
    action(
        ui,
        "Right-click the map",
        "— the \"Lowest beam here\" menu: the nearby radars with the lowest 0.5° beam over \
         that point, each with beam height and distance (units per Settings > Display); \
         click to switch and load. TDWRs, research feeds, custom feeds, and international \
         radars get their own rows, so the menu works over Europe or Japan the same as over \
         CONUS. Right-clicking also jumps to the nearest site directly.",
    );
    action(
        ui,
        "Ctrl+right-click",
        "— adds the nearest radar — US or international, whichever is closer — as an extra \
         overlay layer instead of switching, for multi-radar mosaics. Manage overlays \
         (opacity, refresh, promote, remove) in Map.",
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
// 13. 3D Volume

fn volume_3d(ui: &mut egui::Ui) {
    ui.heading("3D Volume");
    para(
        ui,
        "Windows > 3D Volume opens the GPU volume renderer for the active radar volume. It is \
         designed for storm-scale inspection, not as a replacement for the 2D warning map.",
    );

    subhead(ui, "CHOOSING A VOLUME");
    para(
        ui,
        "Use the 3D window controls or draw/select a map box over the storm you want. BowEcho \
         samples one complete same-site radar volume into a Cartesian 3D texture. If live data \
         is still partial, the 3D view waits for a complete same-site volume instead of \
         borrowing a different radar or resampling an incomplete scan.",
    );

    subhead(ui, "VIEW MODES");
    action(
        ui,
        "Orbit",
        "- default camera. Good for rotating around a storm cell and keeping spatial context.",
    );
    action(
        ui,
        "Fly",
        "- drag to look, WASD to move, Q/E vertical, mouse wheel to dolly, Shift for faster \
         movement. Useful for moving through or around tall exaggerated storm boxes.",
    );

    subhead(ui, "FLOOR PPI");
    para(
        ui,
        "Floor PPI paints the lowest usable reflectivity sweep onto the floor of the 3D box. \
         The floor uses the active reflectivity palette and has its own alpha slider, so you \
         can keep ground context without hiding the volume.",
    );

    subhead(ui, "LIMITS");
    para(
        ui,
        "The current 3D renderer is single-radar. True multi-radar 3D compositing needs a \
         real composite grid and quality rules first.",
    );
}

// ---------------------------------------------------------------------------
// 14. Capture & brand

fn capture_brand(ui: &mut egui::Ui) {
    ui.heading("Capture & brand");

    subhead(ui, "SCREENSHOTS");
    para(
        ui,
        "Screenshot writes a PNG to Pictures/BowEcho and places a paste-ready image on the \
         clipboard. Shift+F12 captures the map only; F12 captures the full app window. Map \
         Only hides chrome for clean screenshots and recordings, and the top hover affordance \
         brings the UI back if the tabs are hidden.",
    );

    subhead(ui, "VIDEO, GIF, WEBP");
    para(
        ui,
        "Loop export records the current timeline deterministically, waiting for the intended \
         radar texture and synced layers before writing each frame. MP4 is best for social \
         video, WebP is the better compact animated image, and GIF remains useful when a site \
         does not accept video.",
    );

    subhead(ui, "FREE RECORD");
    para(
        ui,
        "Free record captures what BowEcho is drawing while you pan, scrub, inspect, or use \
         soundings. It is separate from loop export: loop export records a data timeline, free \
         record records your interaction with the app.",
    );

    subhead(ui, "BRAND KIT");
    para(
        ui,
        "Settings > Brand Kit controls display name, links, palette, optional image assets, \
         screenshot/loop filename prefix, output folder, and optional watermark/share-card \
         overlays. The Generic preset is for non-BowEcho-branded builds; the default preset \
         remains BowEcho.",
    );
    para(
        ui,
        "Brand Kit is runtime presentation only. App icon metadata, code signing identity, \
         notarization, and installers are still release/build workflow concerns.",
    );
}

// ---------------------------------------------------------------------------
// 15. Keyboard shortcuts

fn shortcuts(ui: &mut egui::Ui) {
    ui.heading("Keyboard shortcuts");
    para(
        ui,
        "The complete list — there are deliberately few. Keys are ignored while a text box \
         has focus, and in grid layouts they act on the focused (last-clicked) pane.",
    );

    subhead(ui, "KEYS");
    key_row(ui, "Space", "play / pause the loaded loop");
    key_row(
        ui,
        "PgUp / PgDn",
        "previous / next frame in the loaded loop",
    );
    key_row(ui, "\u{2190} / \u{2192}", "previous / next product");
    key_row(ui, "\u{2191} / \u{2193}", "step up / down the tilt list");
    key_row(ui, "G", "show / hide the lat-lon grid");
    key_row(
        ui,
        "1 \u{2026} 9, 0",
        "product hotkeys — defaults: 1 REF · 2 VEL · 3 SRV · 4 RHO · 5 ZDR · 6 SW · \
         7 CREF · 8 ET · 9 VIL · 0 VILD",
    );
    para(
        ui,
        "Product hotkeys can also use letters A-Z. Settings > Hotkeys shows the current map \
         and config path; product buttons display their assigned key.",
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
        "context menu: Why this synthetic gate? when applicable, Lowest beam here (US + international), and nearest-site jump",
    );
    key_row(ui, "Ctrl+right-click", GUIDE_CUSTOM_OVERLAY_TEXT);
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
        "Ctrl+Shift+drag",
        "draw a model plot-domain box on the map (Model window open with a loaded field)",
    );
    key_row(
        ui,
        "Shift+right-drag",
        "select a 3D volume box on the map (opens the 3D Volume viewer)",
    );
    key_row(
        ui,
        "armed tools",
        "cross-section / Vrot / \u{1f4d0} Draw plot box own the clicks: left places or draws, \
         right clears",
    );
    key_row(
        ui,
        "Annotate mode",
        "click drops a crosshair; box/arrow/freehand are drags; Esc exits, \
         Clear wipes — annotations are geo-anchored and show up in \
         screenshots and recordings",
    );
    subhead(ui, "3D FLY MODE");
    key_row(ui, "drag", "look around in the 3D Volume Explorer");
    key_row(ui, "W / A / S / D", "move through the 3D volume");
    key_row(ui, "Q / E", "move down / up");
    key_row(ui, "Shift", "move faster while held");
}

// ---------------------------------------------------------------------------
// 16. Data sources & credits

fn sources(ui: &mut egui::Ui) {
    ui.heading("Data sources & credits");

    subhead(ui, "RADAR");
    para(
        ui,
        "NEXRAD Level II from Unidata's AWS Open Data buckets — unidata-nexrad-level2 \
         (archive) and unidata-nexrad-level2-chunks (real-time chunks). No keys, no \
         accounts. The site directory comes from api.weather.gov/radar/stations.",
    );

    para(
        ui,
        "European radar is provided by EUMETNET OPERA and the participating national \
         meteorological services via the OPERA Development Radar Data (ORD) service, \
         including Spain's AEMET radar network (opened through ORD in June 2026) — all 11 \
         sites, from the mainland to the Canary Islands — ORD data is licensed CC BY 4.0 with \
         attribution to OPERA and the originating national services. Additional national \
         open-data feeds: SMHI (Sweden), FMI \
         (Finland), DWD (Germany), DMI (Denmark), CHMI (Czechia), SHMU (Slovakia), \
         GeoSphere Austria, the Estonian Environment Agency (KAIA), Romania's ANM — its \
         native open feed carrying dual-pol (ZDR, KDP, RhoHV) beyond the shared European \
         moments (Data: Administrația Națională de Meteorologie (ANM) România), JMA/NICT \
         (Japan), \
         Italy's Protezione Civile with ARPA Piemonte and ARPA Lombardia, Taiwan CWA, \
         and Australia's Bureau of Meteorology via NCI. Coverage and archive depth vary \
         by country and provider.",
    );

    subhead(ui, "HAZARDS & REPORTS");
    para(
        ui,
        "Warnings: NWS active alerts (api.weather.gov) plus hot NWS text products and SPC \
         mesoscale discussions. Storm reports: SPC filtered storm-report CSVs, live and per \
         convective day (spc.noaa.gov/climo/reports). Tornado tracks: the SPC WCM \
         severe-weather database (spc.noaa.gov/wcm, \"onetor\" format; Schaefer & Edwards \
         1999, 11th Conf. Applied Climatology).",
    );

    para(
        ui,
        "Outlooks include official SPC GeoJSON plus raw SPC PTS fallback for faster issue \
         detection, and ESTOFEX XML polygons for Europe. mPING crowd reports (NOAA NSSL / \
         University of Oklahoma) and Spotter Network placefiles are optional map layers.",
    );

    subhead(ui, "MODEL & SATELLITE");
    para(
        ui,
        "HRRR (NOAA High-Resolution Rapid Refresh) and GFS (0.25° global) ingested into a \
         local store by the \
         rusty-weather stack (rw-ingest / rw-ui); the native skew-T is verified against \
         sharprs. RAP (13 km) 0-hour analysis wind profiles are fetched per site from the \
         NOAA open-data mirror to anchor the Analyst 3D dealiaser (CONUS sites). \
         GOES-16/18/19 ABI imagery from NOAA open-data buckets via rw-sat. Simulated satellite \
         imagery is rendered by FahrenheitResearch/simsat (MIT OR Apache-2.0) from WRF or \
         HRRR native-level fields; its seasonal ground layer uses NASA Blue Marble Next \
         Generation imagery.",
    );
    para(
        ui,
        "Raw WRF diagnostics and native-field resolution use FahrenheitResearch/wrf-rust \
         (wrf-core). Formula Lab's bounded language and raw-WRF evaluator are wrf-formula; \
         rusty-weather's rw-formula supplies the deliberately narrower, store-backed adapter. \
         BowEcho's simulated-radar operator records its own versioned scattering/configuration \
         provenance in generated volumes and CfRadial output.",
    );
    para(
        ui,
        "Native CM1 schema handling follows NCAR CM1's official writeout_nc.F at commit \
         a33cd28c206adb010995f3ffb65aada150d9b1b9. The real r19.1 compatibility file used by \
         the v0.33.3 reader check is from the Wang et al. idealized-tornado simulation \
         collection distributed by Penn State Data Commons; that file is not bundled.",
    );

    para(
        ui,
        "GOES GLM lightning and Himawari-9 IR/WV full-disk frames are also wired from NOAA \
         open-data buckets. Meteosat-12 / MTG-I1 rendered imagery is provided publicly by \
         EUMETSAT through EUMETView; public imagery needs no account and the current Satellite \
         interface does not request Data Store credentials. Modified map and plot output carries \
         ‘Contains modified EUMETSAT Meteosat data YEAR.’",
    );

    subhead(ui, "GDEX CLIMATE MODEL DATA");
    para(
        ui,
        "The WRF window's GDEX browser walks the NSF NCAR GDEX online catalog (a THREDDS \
         server) and imports whole files or NCSS subsets directly. A dataset picker offers \
         CONUS II — regional climate WRF downscaling for present and future periods — and \
         ERA-20C, ECMWF's 20th-century reanalysis (1900-2010, GRIB1). \
         Data: NSF NCAR GDEX, CONUS II (CC-BY 4.0), DOI 10.5065/49SN-8E08; ECMWF ERA-20C \
         via NSF NCAR GDEX (d626000). Files are \
         fetched at runtime — nothing is bundled.",
    );

    subhead(ui, "BASEMAPS");
    para(
        ui,
        "The default dark vector basemap is built in (offline). Tile styles: imagery © Esri, \
         Maxar, Earthstar Geographics; Streets/Topo map tiles © Esri and contributors.",
    );

    subhead(ui, "CONTRIBUTED WORK");
    para(
        ui,
        "Research-radar color tables (the \"research\" badge in the pickers) are ported \
         from GURT V3 — the Graphic Utility Radar Toolkit by ambient330 (MIT license). \
         The model and WRF field color tables — reflectivity, temperature (including the \
         per-level upper-air palettes), dewpoint, RH, wind, precip, CAPE, and more — are \
         ported from Solarpower07's WRF-Runner project. \
         The annotation tools' graphics vocabulary (front glyphs, hatch fills, \
         warning-polygon styling) reimplements the GBW Overlay renderer by grayskieswx \
         (YouTube), shared by the author for this purpose.",
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
        "Eilts & Smith 1990, JTECH 7, 118\u{2013}128 — environmental wind constraints; \
         James & Houze 2001, JTECH 18, 1674\u{2013}1683 — 4DD sounding initialization; \
         Louf et al. 2020, JTECH 37 — UNRAVEL reference checks (the Analyst 3D \
         model-anchored dealiaser).",
        "Johnson et al. 1998, Wea. Forecasting 13, 263\u{2013}276 — SCIT storm tracking.",
        "Stumpf et al. 1998, Wea. Forecasting 13, 304\u{2013}326 — mesocyclone detection; \
         Mitchell et al. 1998, Wea. Forecasting 13, 352\u{2013}366 — TVS detection.",
        "Doviak & Zrni\u{107} 1993: Doppler Radar and Weather Observations — 4/3-Earth beam \
         height (eq. 2.28b).",
        "Jung et al. 2008 and 2010 — bulk polarimetric radar forward-operator lineage; \
         Brandes et al. — equilibrium rain-drop axis-ratio relation used by the current \
         bulk S-band Rayleigh kernel.",
        "Hillaire 2020 sky atmosphere; Hillaire/Frostbite and Schneider/Nubis cloud \
         rendering; Wrenninge multi-scatter; Cox & Munk 1954 water-glint distribution — \
         physical-rendering lineage used by SimSat.",
        "Thyng et al. 2016 (cmocean) and Kovesi 2015 — the CVD-safe Balance VEL palette.",
    ] {
        cite(ui, citation);
        ui.add_space(2.0);
    }
    ui.add_space(4.0);
    para(
        ui,
        "Deeper write-ups live in the repo: docs/products-guide.md, docs/formula-lab.md, \
         docs/wrf-simulated-radar.md, docs/cm1-guide.md, docs/simsat-guide.md, and \
         docs/hail-wind-algo-spec.md.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_copy_mentions_current_navigation_and_repro_surfaces() {
        assert_eq!(GuideSection::ALL.len(), 16);
        assert_eq!(GuideSection::Layers.label(), "Map & layers");
        assert_eq!(GuideSection::ModelData.label(), "Models & soundings");
        assert_eq!(GuideSection::Wrf.label(), "WRF & simulated radar");
        assert_eq!(GuideSection::Cm1.label(), "CM1");
        assert_eq!(GuideSection::FormulaLab.label(), "Formula Lab");
        assert_eq!(GuideSection::SimSat.label(), "SimSat");
        assert_eq!(GuideSection::Player.label(), "Unified Player");
        assert_eq!(GuideSection::Volume3d.label(), "3D Volume");
        assert_eq!(GuideSection::CaptureBrand.label(), "Capture & brand");
        assert!(GUIDE_TOP_BAR_TEXT.contains("Map Only"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Workflows"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Radar overlays"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Formula Lab"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Algorithm Truth Lab"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("SimSat"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("CM1"));
        // The consolidated source/timeline front stays documented with its
        // sync default and full-player path.
        assert!(GUIDE_TOP_BAR_TEXT.contains("LIVE plus one Timeline"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("synced warnings"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("sweep playback"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Unified Player"));
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("security and updates"));
        assert_eq!(GUIDE_PANES_LABEL, "Panes 1 / 2 / 3 / 4");
        assert!(GUIDE_PANES_TEXT.contains("three-pane"));
        assert!(GUIDE_DEBUG_CASES_TEXT.contains("Tuscaloosa"));
        assert_eq!(GUIDE_CUSTOM_LAYER_ROW_LABEL, "Layer row in Map");
        assert!(GUIDE_CUSTOM_OVERLAY_TEXT.contains("in Map"));
        assert!(!GUIDE_CUSTOM_OVERLAY_TEXT.contains("in Custom"));
        // The Radar tab's link literally still says "Custom: N layers" —
        // that surface belongs to the concurrent Radar-tab visual round.
        assert!(guide_src.contains("Custom: N layers"));
        assert!(guide_src.contains("warning-polygon"));
        assert!(guide_src.contains("radar-age ring/marker arc/chip"));
        assert!(guide_src.contains("appearance profiles"));
        assert!(guide_src.contains("Mosaic 5"));
        assert!(guide_src.contains("GOES GLM"));
        assert!(guide_src.contains("Floor PPI"));
        assert!(guide_src.contains("Brand Kit"));
        let stale_color_tables = ["Settings", "\u{25b8}", "Color tables"].join(" ");
        assert!(!guide_src.contains(&stale_color_tables));
        let stale_link: String = ['"', 'L', 'a', 'y', 'e', 'r', 's', ':', ' ', 'N']
            .into_iter()
            .collect();
        assert!(!guide_src.contains(&stale_link));
    }

    #[test]
    fn guide_formula_examples_compile_against_the_pinned_engine() {
        for example in GUIDE_FORMULA_EXAMPLES {
            wrf_formula::compile(example).unwrap_or_else(|error| {
                panic!("guide formula did not compile: {example}: {error}")
            });
        }
    }

    #[test]
    fn guide_documents_the_science_workspaces_honestly() {
        let guide_src = include_str!("guide.rs");

        // Simulated radar includes the fast experimentation path, the full
        // recipe set, checked Build 24 base patterns, bounded adjacent-scene
        // sampling, and the research-only T-matrix boundary.
        assert!(guide_src.contains("Refresh current frame(s)"));
        for recipe in [
            "Storm view (fast)",
            "Clean model truth",
            "Clean dual-pol",
            "Real radar (balanced)",
            "Maximum fidelity (slow)",
            "P3/ISHMAEL T-matrix (research)",
        ] {
            assert!(guide_src.contains(recipe), "missing recipe {recipe}");
        }
        for vcp in [
            "VCP 12", "VCP 34", "VCP 35", "VCP 112", "VCP 212", "VCP 215",
        ] {
            assert!(guide_src.contains(vcp), "missing checked {vcp} guide copy");
        }
        assert!(guide_src.contains("All 94 Appendix C physical rows"));
        assert!(guide_src.contains("SAILS, MRLE, AVSET, Add-MPDA"));
        assert!(guide_src.contains("Linear adjacent is the fast path"));
        assert!(guide_src.contains("Raw-state pre-closure is the slower"));
        assert!(
            guide_src
                .contains("Raw-state pre-closure currently requires the BowEcho S research pack")
        );
        assert!(guide_src.contains("191,400,602 bytes"));
        assert!(guide_src.contains("PyTMatrix 0.3.3 is MIT-licensed"));
        assert!(guide_src.contains("retained incompatible combination"));
        assert!(guide_src.contains("Both adjacent modes never extrapolate"));
        assert!(guide_src.contains("hold, drop or error"));
        assert!(guide_src.contains("does not create extra loop frames"));
        assert!(guide_src.contains("the first N\\u{2212}1 frames"));
        assert!(guide_src.contains("Mixed d01/d02"));
        assert!(guide_src.contains("one CfRadial-1 file per frame"));
        assert!(guide_src.contains("per-ray PRT, unambiguous"));
        assert!(guide_src.contains("independent-sample arrays"));
        assert!(guide_src.contains("MP_PHYSICS alone is not sufficient"));
        assert!(guide_src.contains("Real radar, Maximum"));
        assert!(guide_src.contains("OPERATIONAL HRRR/RRFS FORECAST RADAR"));
        assert!(guide_src.contains("same SimSat input and model-cache directories"));
        assert!(guide_src.contains("bulk S-band dual-pol-like forecast guidance"));

        // Exact observed replay and the retained validation products must stay
        // discoverable without suggesting geometry or missing moments.
        assert!(guide_src.contains("Replay displayed observed scan"));
        assert!(guide_src.contains("Observed, Simulated and Difference"));
        assert!(guide_src.contains("synthetic minus observed"));
        assert!(guide_src.contains("missing sectors"));
        for product in ["MCOV", "TUNB", "MSIG", "IREF", "MREF"] {
            assert!(
                guide_src.contains(product),
                "missing validation product {product}"
            );
        }
        assert!(guide_src.contains("Minimum model coverage"));
        assert!(guide_src.contains("quality fields stay unmasked"));
        assert!(guide_src.contains("Physically coupled single-PRF moment estimator"));
        assert!(guide_src.contains("Emit Ideal + Measured diagnostic moments"));
        assert!(guide_src.contains("Algorithm Truth Lab"));
        assert!(guide_src.contains("wrong Nyquist branches"));
        assert!(guide_src.contains("Why this synthetic gate?"));
        assert!(guide_src.contains("generation-matched real retained-source GateExplanation"));
        assert!(guide_src.contains("recomputes that radial from its first gate"));
        assert!(guide_src.contains("property T-matrix currently exposes only aggregate"));
        assert!(guide_src.contains("never infers a species mixture"));

        assert!(guide_src.contains("property-aware T-matrix research contract"));
        assert!(guide_src.contains("P3 50–53 and ISHMAEL 55"));
        assert!(guide_src.contains("symmetric Bruggeman air/ice/water"));
        assert!(guide_src.contains("exactly 2.8 GHz"));
        assert!(guide_src.contains("2.8 GHz S, 5.6 GHz C or 9.4 GHz X"));
        assert!(guide_src.contains("No validated C- or X-band pack ships"));
        assert!(guide_src.contains("bowecho-simradar/tmatrix-packs"));
        assert!(guide_src.contains("generate_band_pack.py"));
        assert!(guide_src.contains("always marks them unvalidated_research"));
        assert!(guide_src.contains("Full property includes standalone/residual rain"));
        assert!(guide_src.contains("Frozen-only deliberately omits all rain"));
        assert!(guide_src.contains("deterministic 50-node (5 by 10) orientation integration"));
        assert!(guide_src.contains("signed KDP remains signed"));
        assert!(guide_src.contains("never silent Rayleigh fallback"));
        assert!(guide_src.contains("reconstruct their native gamma PSD"));
        assert!(guide_src.contains("Wet ISHMAEL PSD allocation is unavailable"));
        assert!(guide_src.contains("old single-characteristic-particle"));
        assert!(guide_src.contains("official WRF v5.4 two-moment table"));
        assert!(guide_src.contains("reconstructed scheme-native PSDs per particle"));
        assert!(guide_src.contains("projected-area-equivalent spheroid"));
        assert!(guide_src.contains("80 mm upper edge"));
        assert!(guide_src.contains("WRF refractivity (research)"));
        assert!(guide_src.contains("not independently validated"));
        assert!(guide_src.contains("make no operational claim"));
        let stale_p3_claim = ["P3 is", "characteristic-particle"].join(" ");
        let stale_psd_claim = ["not", "PSD-integrated"].join(" ");
        let stale_bowecho_version = ["BowEcho", "v0."].join(" ");
        let stale_simsat_version = ["SimSat", "v0."].join(" ");
        assert!(!guide_src.contains(&stale_p3_claim));
        assert!(!guide_src.contains(&stale_psd_claim));
        assert!(!guide_src.contains(&stale_bowecho_version));
        assert!(!guide_src.contains(&stale_simsat_version));
        assert!(guide_src.contains("embedded SimSat 0.2.1 renderer"));

        // Formula Lab must retain the explicit capability boundary rather
        // than implying every stored model has raw-WRF grid geometry.
        assert!(guide_src.contains("Syntax valid"));
        assert!(guide_src.contains("Ready for selected source"));
        assert!(guide_src.contains("rw-store does not persist horizontal spacing/map factors"));
        assert!(guide_src.contains("Three-dimensional grad/div/curl/laplacian remain"));
        assert!(guide_src.contains("source fingerprint"));
        assert!(guide_src.contains("any compatible stored run"));
        assert!(guide_src.contains("one selected multi-time file"));
        assert!(guide_src.contains("at least 1 GiB"));

        // SimSat's durable CPU path and preview-only GPU path are distinct,
        // and HRRR's native volume requirement cannot be watered down.
        assert!(guide_src.contains("full native-level wrfnat product"));
        assert!(guide_src.contains("GPU preview"));
        assert!(guide_src.contains("never enters the satellite store"));
        assert!(guide_src.contains("SSB cache format v6"));
        assert!(guide_src.contains("does not force a compact-cache format bump"));
        assert!(guide_src.contains("Deterministic 2 is the reviewed finished-display default"));
        assert!(guide_src.contains("ground lift 1.10"));
        assert!(guide_src.contains("tightly gated twilight recovery"));
        assert!(guide_src.contains("camera-to-cloud atmospheric column"));
        assert!(guide_src.contains("Top-down shadow anti-aliasing"));
        assert!(guide_src.contains("sun-OD march also permits up to 4096"));
        assert!(guide_src.contains("same formula on CPU/GPU"));
        assert!(guide_src.contains("Natural (NOAA heritage)"));
        assert!(guide_src.contains("CIMSS Style is SimSat 0.2.1's recommended"));
        assert!(guide_src.contains("band-averaged absorption rather than line-by-line"));
        assert!(guide_src.contains("The Current label describes"));
        assert!(guide_src.contains("fades in from -6 to 0 degrees"));
        assert!(guide_src.contains("Source paths, active jobs, progress, errors"));
        assert!(guide_src.contains("Resolution is not a separate run key"));
        assert!(guide_src.contains("not currently embedded into Satellite"));
    }

    #[test]
    fn guide_documents_cm1_and_partial_namelist_boundaries() {
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("Extract namelist…"));
        assert!(guide_src.contains("not the original namelist.input"));
        assert!(guide_src.contains("cannot reproduce the run"));
        assert!(guide_src.contains("complete local-Cartesian cm1out files"));
        assert!(guide_src.contains("does not integrate umove/vmove"));
        assert!(guide_src.contains("Meteorological profile readiness"));
        assert!(guide_src.contains("Build native REF/VEL in Radar"));
        assert!(guide_src.contains("native 3-D dbz"));
        assert!(guide_src.contains("final, latest evolved output"));
        assert!(guide_src.contains("fixed at the placed CM1 domain center"));
        assert!(guide_src.contains("does not extrude 2-D cref"));
        assert!(guide_src.contains("docs/cm1-guide.md"));
    }

    /// International radars are not second-class: the guide's gesture and
    /// archive copy must describe the US+international reality, and the
    /// retired US-only absolutes must stay gone (parity audit, dishonest-NA
    /// item 13).
    #[test]
    fn guide_copy_is_honest_about_international_radars() {
        // Ctrl+right-click overlay dispatch picks the closer of US/intl.
        assert!(GUIDE_CUSTOM_OVERLAY_TEXT.contains("US or international, whichever is closer"));
        let guide_src = include_str!("guide.rs");
        // The right-click beam menu ranks international sites too, and the
        // shortcuts table says so.
        assert!(guide_src.contains("US and international alike"));
        assert!(guide_src.contains("US + international radars"));
        // PICK A RADAR tells intl users how to find and live-poll sites.
        assert!(guide_src.contains("amber markers"));
        assert!(guide_src.contains("Radar coverage"));
        assert!(guide_src.contains("provider's cadence"));
        // Load Loop copy mirrors the derive-named provider hover in main.rs:
        // recent-catalog providers loop, single-frame providers grow live.
        assert!(guide_src.contains("single-frame providers start at the newest scan"));
        // The Data-tab day browser is the unified archive world (v0.29
        // Phase 2): US Level II AND archive-capable international
        // providers, with honest greyed reasons for the rest — the old
        // US-exclusivity claim (assembled below so this file cannot
        // match itself) must stay gone.
        assert!(guide_src.contains("US NEXRAD Level II"));
        assert!(guide_src.contains("EUMETNET ORD, SMHI Sweden, Australia NCI"));
        assert!(guide_src.contains("say so with a reason"));
        assert!(guide_src.contains("Browse archive"));
        let old_us_only_claim = ["US Level II", "only"].join(" ");
        assert!(!guide_src.contains(&old_us_only_claim));
        // Retired claims, assembled at runtime so this test file does not
        // match itself: the beam menu is no longer 88D-only (old copy said
        // exactly three of them), and the absolute chip promise was
        // falsified by intl archive loads.
        let old_beam_claim = ["three", "WSR-88Ds"].join(" ");
        assert!(!guide_src.contains(&old_beam_claim));
        let old_chip_claim = ["you can never", "mistake"].join(" ");
        assert!(!guide_src.contains(&old_chip_claim));
    }

    /// Trust-label tripwire (v0.29 Phase 5): the guide's archive-capable
    /// provider name-drop must track the DERIVED capability set — the
    /// same `supports_archive()` answer the capability cards, the archive
    /// browser, and the Event Loop Builder gate on. A new archive adapter
    /// flips its card automatically; this test makes the guide copy keep
    /// up instead of quietly understating coverage.
    #[test]
    fn guide_archive_provider_names_track_the_derived_capability_set() {
        let archive_ids: std::collections::BTreeSet<&str> =
            data_source::international::intl_providers()
                .iter()
                .filter(|provider| provider.supports_archive())
                .map(|provider| provider.id())
                .collect();
        assert_eq!(
            archive_ids,
            std::collections::BTreeSet::from(["australia-nci", "ord", "smhi"]),
            "a provider gained/lost an archive adapter — update the guide's \
             Archive section copy and this pin together"
        );
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("EUMETNET ORD, SMHI Sweden, Australia NCI"));
    }

    /// GDEX (Stage 1b) attribution: the in-app CONUS II browser must carry the
    /// NSF NCAR GDEX credit, its CC-BY 4.0 license, and the dataset DOI in the
    /// sources copy (no invented credit surface — this is the existing one).
    #[test]
    fn guide_credits_gdex_conus_ii_source() {
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("NSF NCAR GDEX"));
        assert!(guide_src.contains("CONUS II"));
        assert!(guide_src.contains("10.5065/49SN-8E08"));
        assert!(guide_src.contains("CC-BY 4.0"));
    }

    /// ERA-20C attribution rides the SAME GDEX sources paragraph (no invented
    /// credit surface): ECMWF as producer, GDEX as the distributor, and the
    /// dataset id the picker exposes.
    #[test]
    fn guide_credits_gdex_era20c_source() {
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("ERA-20C"));
        assert!(guide_src.contains("ECMWF"));
        assert!(guide_src.contains("d626000"));
    }

    /// Restored MTG imagery is the public rendered EUMETView path, not the
    /// retired whole-product FCI downloader. Keep the no-account imagery,
    /// common player/map/plot path and LI raster vs. point-overlay distinction
    /// explicit together. Data Store controls stay hidden until they back a
    /// real raw-product workflow.
    #[test]
    fn guide_documents_restored_public_meteosat_integration() {
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("Meteosat-12 / EUMETVIEW"));
        assert!(guide_src.contains("EUMETSAT EUMETView"));
        assert!(guide_src.contains("no account or API key is required"));
        assert!(guide_src.contains("Load latest"));
        assert!(guide_src.contains("Load loop"));
        assert!(guide_src.contains("Map follows player"));
        assert!(
            guide_src.contains("if crate::eumetsat_credentials::DATA_STORE_ACCOUNT_UI_ENABLED")
        );
        assert!(guide_src.contains("does not request Data Store credentials"));
        assert!(guide_src.contains("Contains modified EUMETSAT Meteosat data YEAR."));
        assert!(guide_src.contains("not a raw FCI download path"));
        assert!(guide_src.contains("dedicated point-flash Lightning overlay remains GOES GLM"));
        let retired_unavailable_claim = [
            "MTG/European satellite imagery",
            "is not available in this build",
        ]
        .join(" ");
        assert!(!guide_src.contains(&retired_unavailable_claim));
    }

    #[test]
    fn guide_puts_loop_loading_before_export_controls() {
        let guide_src = include_str!("guide.rs");
        assert!(guide_src.contains("set Frames to load before pressing Load Loop"));
        assert!(guide_src.contains("Raise it and press"));
        assert!(
            guide_src.contains("Video & GIF export settings only record already-loaded frames")
        );
        assert!(guide_src.contains("do not change the load count"));
    }
}
