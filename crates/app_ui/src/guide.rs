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
use crate::ui_theme::{accent_color, subhead_color};

const GUIDE_TOP_BAR_TEXT: &str = "The top bar leads with the LIVE | ARCHIVE control: \
    LIVE follows the current radar's newest data and backfills a loop in one click; \
    ARCHIVE loads a loop ending at any UTC date/time. Both arm synced warnings by \
    default. Sweeps picks the low-level sweep loop mode one click deep, and Advanced \
    opens the full Unified Player. One-shot actions follow \
    (Reset View, Reload, Map Only, Screenshot, Annotate) plus Workflows presets. \
    On the right, the Windows menu opens every data window (Model, Radar \
    overlays, Satellite, WoFS, FARM, 3D Volume, Sounding) beside this Guide. Status chips appear \
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum GuideSection {
    #[default]
    GettingStarted,
    Products,
    Layers,
    ModelData,
    Satellite,
    Archive,
    Player,
    Tools,
    Volume3d,
    CaptureBrand,
    Shortcuts,
    Sources,
}

impl GuideSection {
    const ALL: [GuideSection; 12] = [
        Self::GettingStarted,
        Self::Products,
        Self::Layers,
        Self::ModelData,
        Self::Satellite,
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
            Self::ModelData => "Model data & soundings",
            Self::Satellite => "Satellite",
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
        "— after Load Loop: play/pause, step buttons, a scrub slider, and the frame cap \
         (compact controls; Unified Player supports up to 2000 frames, 64x playback, and exports).",
    );

    subhead(ui, "PANES & THE MAP");
    action(ui, GUIDE_PANES_LABEL, GUIDE_PANES_TEXT);
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
        "+ Add layer \u{25be}",
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
         Radar tab keeps a one-line \"Custom: N layers \u{2192}\" link; Poll-URL feeds moved to the \
         Data tab (they replace the volume source — acquisition, not a layer); SPC \
         day/kind config moved to the Alerts tab.",
    );
}

// ---------------------------------------------------------------------------
// 3. Model data & soundings

fn model_data(ui: &mut egui::Ui) {
    ui.heading("Model data & soundings");
    para(
        ui,
        "Model fields and skew-T soundings (HRRR/RAP/RRFS-style CONUS workflows plus GFS \
         worldwide, plus local WRF and NetCDF files you import yourself), layered straight \
         onto the radar map. Enable the \
         master switch first: \u{2699} Settings \u{25b8} Model \u{25b8} Model data (off = \
         pure radar app). Windows \u{25be} \u{25b8} Model data opens the Model window.",
    );

    subhead(ui, "GETTING DATA");
    action(
        ui,
        "Fetch latest",
        "(Custom row) — ingests the freshest init of the picked model (GFS automatically \
         when the radar sits outside HRRR coverage), sounding-grade hours bracketing now, \
         then prunes the store to the newest runs. About a minute, throttled below \
         the UI so frames never stutter.",
    );
    action(
        ui,
        "Download…",
        "— the full window: any init date/cycle, an hours spec (\"0-3\" or \"2,4,6\"), profile \
         choice, with a live size estimate before you commit. Model support depends on the \
         local rusty-weather ingest/catalog build.",
    );
    action(
        ui,
        "Keep runs",
        "— store retention; the newest N runs survive each fetch and startup (default 2, \
         \u{2248}1.5 GB on disk).",
    );

    subhead(ui, "LOCAL WRF & NETCDF");
    action(
        ui,
        "\u{1f4c4} WRF/NetCDF file\u{2026} / \u{1f4e5} folder\u{2026}",
        "— import local model files into the store: 2-D surface fields plus skew-T sounding \
         volumes, one forecast hour per file. Handles raw wrfout, post-processed climate \
         wrfout, and plain NetCDF. Click a point in the field viewer afterwards to sound it.",
    );
    para(
        ui,
        "Imported WRF fields now list under readable names — the raw wrfout registry names \
         (wrf_swupt\u{2026}) resolve to a catalog of friendly labels with hover descriptions, \
         and most get a Solarpower07 model palette compiled into their style so the map draws \
         in a tuned color ramp rather than the viridis fallback. This applies to runs already \
         on disk — no re-import.",
    );
    action(
        ui,
        "\u{1f6e0} Full model import (heavy)",
        "— the full-diagnostics ingest (~117 fields: CAPE, severe, precip, soil, …) via \
         wrf-core, minutes per file on large grids. A field selector narrows it to a \
         core-only set when you don't need everything. This is NOT the simulated-radar button.",
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

    subhead(ui, "SIMULATED RADAR FROM WRF");
    para(
        ui,
        "\u{1f329} Simulated radar from WRF (fast) forward-models a raw wrfout file's \
         hydrometeors and winds into a simulated NEXRAD-style volume — reflectivity and \
         radial velocity on a real 14-tilt polar ladder (720 radials, 250 m gates to 230 km, \
         4/3-Earth beam) — that renders and LOOPS in the radar view through the same pipeline \
         as Level II: colormaps, tilts, cross-sections, and velocity dealiasing. Pick one or \
         more files (each forecast time becomes a loop frame); it writes nothing to the model \
         store and is labelled \u{201c}simulated\u{201d} so you always know. Runs in seconds \
         per file, not the minutes the heavy import takes.",
    );
    para(
        ui,
        "The \u{1f4e1} Virtual radar site & range panel shapes the scan; every change rebuilds \
         the volume on the next run.",
    );
    action(
        ui,
        "Antenna placement",
        "— Domain center (default), an explicit Lat/Lon, or a real NEXRAD site id resolved \
         through the app's site catalog, so you can compare the model directly against what \
         that radar would see. Max range defaults to the classic 230 km and reaches 1000 km.",
    );
    action(
        ui,
        "Reflectivity operator",
        "— Model native (REFL_10CM) renders the model's own Thompson 10-cm reflectivity \
         (hotter, fatter cores in graupel and the melting layer); Classic Stoelinga (community \
         look) always computes dBZ with fixed Marshall-Palmer intercepts, matching the \
         wrf-python / GR2Analyst pipeline — roughly 10\u{2013}20 dB cooler in those regions, so \
         hooks stand out of moderate echo.",
    );
    action(
        ui,
        "Gate texture",
        "— adds deterministic, radially-correlated speckle to the simulated reflectivity so it \
         reads like real Level-II gates instead of a smooth model field (on by default); a \
         separate opt-in adds a gentle \u{00b1}0.5 m/s wobble to velocity, kept off so the clean \
         Vr keeps feeding the dealias / GBVTD tools.",
    );
    action(
        ui,
        "Ground clutter",
        "— a 0\u{2013}100% slider that paints fabricated near-radar ground return (concentrated \
         within ~40 km on the low tilts, fading with range and beam height, with a few \
         hotspots) modelled on the community WRF\u{2192}GR2 export. Our operator is pure physics \
         at 0%; it only fills gates weaker than itself, so storms are never overwritten, and \
         cluttered gates read near-zero velocity like real ground. Deterministic per frame, so \
         a loop doesn't shimmer.",
    );
    action(
        ui,
        "Realistic Nyquist (velocity folds)",
        "— folds the simulated radial velocity into a chosen Nyquist co-interval (default \
         25 m/s) so fast winds alias the way a real pulse-pair radar measures them; the plain \
         VEL product then folds while DVEL / DSRV (or the velocity dealiaser) reconstruct the \
         true field — a dealiasing practice ground with known ground truth. Off stamps a wide \
         Nyquist and shows the exact unfolded wind.",
    );
    action(
        ui,
        "Include 0.1\u{00b0} low tilt",
        "— prepends a 0.1\u{00b0} sweep below the standard 0.5\u{00b0} lowest tilt (the \
         community exports' lowest cut); the lower beam samples roughly half the height at \
         range, so a low-level hook is better defined. Adds one sweep to every volume.",
    );
    action(
        ui,
        "Match gate size to grid resolution",
        "— sets the gate spacing to the WRF file's own grid resolution (its DX, clamped \
         100 m\u{2013}10 km) instead of a fixed 250 m, so a coarse model isn't oversampled into \
         a dozen identical gates per cell. Off by default; a file with no grid-resolution \
         attribute falls back to the configured spacing.",
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
// 4. Satellite

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
         NaturalColor, …) over the current sector. MTG/European satellite imagery is not \
         available in this build — it needs EUMETSAT entitlement the app cannot assume. \
         Switching source, sector, or layer clears stale frames so late downloads from the old \
         selection cannot flash onto the map.",
    );

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

    subhead(ui, "NATIVE-RESOLUTION WINDOWS");
    para(
        ui,
        "Tick Native window and set a center lat/lon and size to compose the true-color loads \
         at the sensor's full 0.5 km resolution over just that box — only the segments and \
         pixels covering it are fetched and decoded, so a typhoon eye stays crisp and the \
         frames stay small enough to loop. Repeated loads of the same window stack into one \
         loopable run. It works for Himawari (segment-level fetch) and GOES (windowed decode), \
         and overrides the full-sector/full-disk scope while it is on.",
    );

    subhead(ui, "STORM SATELLITE (ONE PRESS)");
    para(
        ui,
        "Each tropical cyclone card carries \u{1f6f0} Vis and \u{1f6f0} IR buttons beside \
         \u{1f4cd} Focus. One press picks the covering geostationary satellite for that storm's \
         basin (Himawari-9 / GOES-East / GOES-West), pulls a native-resolution window centered \
         on the storm, and opens it in the Satellite window while following the frame onto the \
         radar map — true color for Vis (daylight side only), Band-13 brightness temperature \
         with your chosen IR enhancement for IR (day and night). One storm satellite load runs \
         at a time.",
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
         the Map tab's GOES row.",
    );

    subhead(ui, "LIGHTNING");
    para(
        ui,
        "The Lightning layer is GOES GLM: BowEcho chooses East or West from the map longitude, \
         fetches the newest granules first, age-fades flashes, and reads the rolling store for \
         loaded radar loops. Live means the newest flash is inside the live-age gate; stale \
         status means the layer is waiting for newer GLM files, not that radar data is broken.",
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
// 6. Unified Player

fn unified_player(ui: &mut egui::Ui) {
    ui.heading("Unified Player");
    para(
        ui,
        "The Unified Player is the Advanced disclosure of the top bar's LIVE | ARCHIVE \
         control — the bar covers go-live and loop-ending-at in one click; open Advanced \
         (or Windows > Unified Player) for everything else. It owns long radar loops, archive \
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
// 7. Tools & inspector

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
// 8. 3D Volume

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
// 9. Capture & brand

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
// 10. Keyboard shortcuts

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
        "\"Lowest beam here\" menu (US + international radars) + jump to the nearest site",
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
// 11. Data sources & credits

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
         GOES-16/18/19 ABI imagery from NOAA open-data buckets via rw-sat.",
    );

    para(
        ui,
        "GOES GLM lightning and Himawari-9 IR/WV full-disk frames are also wired from NOAA \
         open-data buckets. MTG/European satellite imagery is not available in this build \
         (EUMETSAT entitlement).",
    );

    subhead(ui, "GDEX CLIMATE MODEL DATA");
    para(
        ui,
        "The model dock's GDEX browser walks the NSF NCAR GDEX online catalog (a THREDDS \
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_copy_mentions_current_navigation_and_repro_surfaces() {
        assert_eq!(GuideSection::ALL.len(), 12);
        assert_eq!(GuideSection::Layers.label(), "Map & layers");
        assert_eq!(GuideSection::Player.label(), "Unified Player");
        assert_eq!(GuideSection::Volume3d.label(), "3D Volume");
        assert_eq!(GuideSection::CaptureBrand.label(), "Capture & brand");
        assert!(GUIDE_TOP_BAR_TEXT.contains("Map Only"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Workflows"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Radar overlays"));
        // The v0.29 two-button front: the bar leads the top-bar copy, the
        // sweep menu is documented one click deep, synced warnings are the
        // named default, and the Unified Player is its Advanced disclosure.
        assert!(GUIDE_TOP_BAR_TEXT.contains("LIVE | ARCHIVE"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("synced warnings"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Sweeps"));
        assert!(GUIDE_TOP_BAR_TEXT.contains("Advanced"));
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

    /// The retired MTG plumbing (v0.29 Phase 5 deletion sweep) must stay
    /// gone from the copy: the guide may say European imagery is NOT
    /// available, never that FCI discovery/decode "are wired" (stale
    /// claims assembled at runtime so this file cannot match itself).
    #[test]
    fn guide_copy_is_honest_about_retired_mtg_plumbing() {
        let guide_src = include_str!("guide.rs");
        let stale_wired_claim = ["FCI", "discovery/local decode"].join(" ");
        assert!(!guide_src.contains(&stale_wired_claim));
        let stale_decode_claim = ["decode are", "wired"].join(" ");
        assert!(!guide_src.contains(&stale_decode_claim));
        assert!(guide_src.contains("not available in this build"));
    }
}
