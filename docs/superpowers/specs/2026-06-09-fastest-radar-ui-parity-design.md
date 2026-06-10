# Fastest Radar — UI Revamp & Warning-Desk Parity (Design / Plan of Record)

Date: 2026-06-09 · Status: approved-by-owner-directive (autonomous execution) · Branch: `fix/region-based-velocity-dealias` → feature branches per sub-project

## North Star

**World's fastest base-radar app.** Speed is the product identity; parallelized
downloading is the moat. Every feature must preserve the fast path: instant
product/tilt switching, buttery loop scrubbing, prefetch-everything, low-latency
render, never block the UI thread. A feature that slows the fast path is wrong
even if it adds parity.

## Success criterion (honest scope)

Not literal GR2Analyst/RadarScope parity (that includes 3D/UDP, MRMS mosaic,
SCIT/MDA/TVS detection, raster/satellite basemaps, mPING, VAD, spotters, etc.).
The target is **warning-desk parity, fastest-in-class**: the
derecho/tornado/severe interrogation workflow done faster and cleaner than the
incumbents. GR2-style *ergonomics* (dense, keyboard-driven, multi-pane,
on-canvas everything) executed at RadarScope-grade *fluidity*.

## What's already strong (keep, don't rebuild)

Base + dual-pol moments (REF/VEL/SW/ZDR/CC/PHI); SRV/DSRV with user storm
motion; a real region-based velocity dealiaser (Jing & Wiener 1993; Feldmann
et al. 2020 R2D2; Helmus & Collis 2016 Py-ART); a complete `.pal` color-table
engine with per-product presets + new perceptual defaults (Kovesi 2015; Thyng
et al. 2016); live NWS/SPC hazard polygons with parsed attributes; up to 10
multi-radar overlay layers; vector basemap + site catalog; AWS live-chunk +
archive ingest with rayon-parallel decode; honest perf telemetry.

## Locked decisions (owner delegated; not asking)

- **Feel:** GR2 warning-desk ergonomics, RadarScope-grade speed. Dark theme,
  dense layout, keyboard-first, multi-pane.
- **GPU backend (SP16):** deferred but reframed as the *strategic speed endgame*,
  not just a 3D enabler. First pass stays CPU+rayon; the render pipeline is
  designed so a `wgpu` path can slot behind the same `RenderRequest` interface.
  3D volume rendering / isosurfaces / GLSL UDP remain out of first-pass scope.
- **Placefiles (SP7):** yes, Phase 2 — one parser unlocks the whole community
  overlay catalog.
- **Derived products (SP10/11/13):** yes, Phase 3, on a shared full-volume
  column-walk + beam-height infra.
- **Environmental ingest (SP12):** yes, Phase 3 (RAP/RUC for 0 °C/−20 °C heights).
- **Persistence:** JSON `AppConfig` under the platform config dir (SP1, first).
- **Multi-pane sync:** location+zoom+time linked by default; product+tilt
  per-pane (the canonical Z+SRV+ZDR+CC quad). Opt-in unlink.
- **Basemap:** keep vector; add "None"/solid toggle now; tiled raster deferred.
- **Overlay vs pane:** panes are the primary multi-view metaphor; the existing
  10-radar overlay stack becomes "additional layers *within* a pane." One
  `RenderView` template serves both.

## Corrections forced by adversarial review (shared infra, Phase 0/1)

These were mis-attributed as "done" or omitted; they are real, shared, and
sequenced early:

1. **Beam-height geometry (NEW, Phase 0 shared infra).** No height calc exists
   today. Implement standard 4/3-Earth-radius beam propagation
   `h(r,θ) = sqrt(r² + (k·aₑ)² + 2·r·k·aₑ·sinθ) − k·aₑ`, `k=4/3`,
   aₑ = 6371 km (Doviak & Zrnić 1993, *Doppler Radar and Weather Observations*,
   §2.2; Bean & Dutton 1968). Add `height_agl` to the inspector. Prerequisite
   for SP6 (cross-section), SP10 (echo tops/VIL), SP13 (MESH).
2. **Projection correctness (Phase 1, before SP3 sync).** Replace naive
   equirectangular with a per-radar **azimuthal-equidistant** transform centered
   on the site so range rings stay circular and AK/PR/Guam are correct; this is
   the standard single-radar display projection and makes cross-pane co-located
   readout gate-accurate.
3. **Shared render budget (SP2 requirement, not a footnote).** Multi-pane must
   use a single bounded render scheduler / thread budget — NOT one rayon-flooding
   worker per pane. Frame budget tracked; this is the core "fastest under load"
   guarantee.
4. **Smoothing sampler (Phase 0/1, CPU-scope).** Add optional bilinear/area
   sampling of the polar field (vs nearest-gate) — a quality + "looks like GR2"
   lever with no backend change.
5. **Dual-pol QC (Phase 1).** CC/SNR masking of low-confidence gates + a user
   ZDR-bias offset (RDA ZDR routinely ±0.3–0.5 dB off) so TDS CC-drops and ZDR
   columns are trustworthy, not buried in speckle.
6. **Special-value correctness (cross-cutting).** RF (range-folded), below-
   threshold, and no-data must be honored consistently across dealias, SRV,
   VROT, and the new column-walks — never treated as real velocity (prevents
   fabricated couplets).
7. **Intra-volume time semantics (SP3/SP5).** Surface per-cut valid time per
   pane/frame; define time-match tolerance (SAILS/MRLE re-scans of 0.5° differ
   within one volume). VCP scan-mode awareness (Clear-Air vs Precip, SAILS/
   AVSET/MRLE) as a correctness input to tilt nav + derived-product validity.
8. **Degraded-state UX (Phase 0 reliability).** Surface partial/incomplete
   volume, missing tilts, and stale-source as explicit UI state.

## Sub-project decomposition

SP1 settings/persistence + cache retention · SP2 pane grid (shared render
budget) · SP3 pane sync + projection fix · SP4 inspector + on-canvas HUD/colorbar
· SP5 timeline & loop ergonomics · SP6 vertical cross-section/RHI · SP7 placefile
engine · SP8 native hazard/outlook/LSR overlays · SP9 shear interrogation +
markers · SP10 volumetric derived (echo tops, VIL/VILD, composite, LLSD az-shear)
· SP11 KDP + HCA · SP12 environmental ingest · SP13 hail/precip · SP14
data-source abstraction/multi-source · SP15 keyboard/command-palette/export
polish · SP16 (big rock) GPU backend · SP17 (big rock) tiled raster basemap.
Plus shared-infra: **SP0a beam-height geometry**, **SP0b smoothing sampler**,
**SP0c special-value plumbing**.

## Phased roadmap

- **Phase 0 — Foundations & free wins (speed + feel):** SP1 config/retention;
  dark low-chroma theme + tightened density; SP0a beam-height + inspector field;
  on-canvas colorbar; pinned/floating inspector + velocity radial arrow + SHIFT-
  pin; Live/Archive/Paused mode chip; display threshold clamp (also a tiny speed
  win); drag-drop `.pal`; SP9 compute slice (full Vin/Vout/diameter/azimuthal
  shear/EF prob on the VROT probe); surface full warning text; favorites +
  startup site + auto-poll; degraded-state messaging; SP0b smoothing toggle.
- **Phase 1 — Multi-pane & timeline (warning-desk core):** SP2 pane grid on a
  shared render budget; **projection fix**; SP3 link location/zoom/time +
  cross-pane co-located readout; per-pane colorbar/header; SP5 timeline bar
  (decoupled speed/time/dwell, `request_repaint_after`, Space/step/End); dual-pol
  QC (CC/SNR mask + ZDR offset).
- **Phase 2 — Cross-section + community overlays:** SP6 RHI; SP7 placefiles;
  SP8 SPC outlooks + LSR icons; SP14 data-source abstraction + failover.
- **Phase 3 — Derived products & environment:** SP10 column-walk cluster
  (echo tops; VIL/VILD per Greene & Clark 1972; composite; LLSD azimuthal shear
  per Smith & Elmore 2004, Mahalik et al. 2019); SP12 RAP/RUC ingest; SP11 KDP
  (Wang & Chandrasekar 2009) + fuzzy-logic HCA (Park et al. 2009); SP13
  MESH/POSH/MEHS (Witt et al. 1998); SP9 markers + storm-motion-from-frames.
- **Phase 4 — Big rocks (gated):** SP16 GPU/wgpu backend → 3D volume render /
  isosurfaces / GLSL UDP; SP17 tiled raster/satellite + GOES; MRMS mosaic;
  detection suite; historical replay.

## Speed invariants (the fast-path contract)

- Downloads always parallel; decode rayon-parallel with first-cut preview.
- Switching product/tilt/frame never re-downloads or re-decodes; served from
  cache; render off the UI thread via the bounded scheduler.
- Loop frames prefetched + pre-decoded; scrub is texture-swap only.
- Multi-pane shares one render budget; never N× core oversubscription.
- Threshold/mask are render-time sampler clamps (cheaper, not extra passes).
- Telemetry stays honest (per-stage timings already exist; extend per pane).

## Delivered (autonomous session 2026-06-09)

Shipped + committed to `main`, ~189 tests green, clippy clean, exe on Desktop:

- **Shared infra:** 4/3-Earth beam height/ground-range (`radar_core`); inspector
  now shows beam height (m/kft).
- **Region-based velocity dealiaser** + HD perceptual velocity/reflectivity
  defaults + dedicated **CC / ZDR / Echo-Tops / VIL / Az-Shear** color families.
- **5 new products, GUI-wired** (`DisplayProduct::Derived` → `new_derived`, cache
  key carries a `derived` discriminator): Composite Reflectivity, Echo Tops,
  VIL (volume column-walk); Azimuthal Shear (LLSD) + Radial Divergence (per-cut,
  on dealiased velocity). All unit-tested + PNG-verified on the real derecho.
- **Vertical cross-section (RHI) engine** — `reflectivity_cross_section`,
  verified; interactive GUI gesture/panel deferred (see below).
- **UX:** dark dense GR2 theme; on-canvas colorbar; LIVE/ARCHIVE/STALE mode
  chip; SP1 settings crate (JSON persistence, startup-site memory + favorites).

**Verification policy this session:** engines/algorithms are unit-tested and
rendered to PNG for visual confirmation; additive-paint GUI (colorbar, chip,
theme) is build+clippy-verified low-risk; **interactive-input GUI (cross-section
draw gesture + panel, multi-pane grid) is deferred** to a user-present session
because it cannot be verified headlessly and could regress canvas input.

**Top remaining:** cross-section GUI; multi-pane + sync (the big P0);
projection fix; smoothing sampler; dual-pol CC/SNR mask + ZDR offset; placefiles.

## SOTA references

Doviak & Zrnić 1993 (beam geometry, radar fundamentals) · Bean & Dutton 1968
(4/3-Earth refraction) · Jing & Wiener 1993 / Feldmann et al. 2020 R2D2 /
Helmus & Collis 2016 (dealiasing, shipped) · Kovesi 2015 / Thyng et al. 2016 /
Crameri et al. 2020 / Borland & Taylor 2007 (perceptual & CVD-safe colormaps) ·
Greene & Clark 1972 (VIL) · Witt et al. 1998 (MESH/POSH/MEHS) · Smith & Elmore
2004 + Mahalik et al. 2019 (LLSD azimuthal shear) · Park et al. 2009 (HCA) ·
Wang & Chandrasekar 2009 (KDP) · Ryzhkov et al. 2005 (dual-pol QC / ZDR
calibration). Cite the specific paper in code comments + commit messages per
project convention.
