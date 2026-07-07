# SimSat Engine — Engineering Design (v1)

*Drafted 2026-07-07 for the v0.31 headline. Repo: BowEcho (`crates/app_ui`, eframe 0.34.3, wgpu 29.0.3 via `eframe::egui_wgpu`).*
*Canonical real-data fixture: the Enderlin 250 m run (`wrfout_d03_2025-06-21_*`, 800×800×79 ≈ 50.5 M cells, ~2 GB/file — see `docs/wrf-import-large-grids.md`).*

**Scope (owner intent, non-negotiable):** physically-based simulated visible satellite from WRF "with similar calculations as the actual GOES satellites": Blue Marble seasonal ground, sun glint, terrain shadows from `HGT`, finite solar disk (real penumbras), sky-scattering secondary illumination incl. below-horizon twilight, volumetric clouds with scattering/reflection/diffusion, shadows weighted by sky visibility and sky color distribution, plus a synthetic IR mode through the existing true-Kelvin IR enhancement pipeline. No product tiers; milestones below are build order only.

**Delivery shape (owner call 2026-07-07): a STANDALONE tool.** SimSat ships as its own desktop app (engine crate + its own eframe binary), not a BowEcho pane. The bridge to BowEcho is the output contract, not embedding: SimSat writes frames in the sat-store format (§7), so pointing BowEcho's sat store at a SimSat run directory makes rendered loops viewable in BowEcho with zero integration code. An in-BowEcho pane remains possible later precisely because of that contract, but it is not a v1 deliverable.

**Hardware floor (owner call 2026-07-07): modern desktop iGPUs.** The engine must run well on integrated graphics of current desktop CPUs — Intel Core Ultra (Arrow Lake Xe, e.g. 270K-class) and AMD Ryzen 9000 (RDNA2 iGPU, e.g. 9950X). These parts are 10–40× slower than the discrete-GPU class the first draft assumed, and they share system RAM (unified memory) instead of carrying VRAM. Design consequence: the product's primary mode on iGPU is **prerender-then-play** (frames render tiled over seconds each, loops play back instantly as stored textures); live interactive preview adapts resolution/step count to whatever the silicon can hold. Discrete GPUs remain the fast path where everything is interactive. See §8.

**Non-goals (honesty boundary):** not a line-by-line radiative transfer model (no CRTM/RRTMG); band-averaged gray optics per hydrometeor class; WRF's native spherical earth (R = 6 370 km), not the GRS80 ellipsoid real ABI navigation uses — pixel-for-pixel registration against real GOES imagery is explicitly not promised, physical plausibility is.

---

## 1. Reference frame & camera model

**v1 cameras (two):**

1. **Geostationary fixed-grid camera (primary, the product).** The output raster *is* an ABI-style scan-angle grid: each output pixel maps to scan angles (x, y) via the CGMS normalized geostationary mapping already implemented in `crates/app_ui/src/sat_window.rs` (`ahi_lat_lon_to_scan_angles`, GOES-R PUG §5.1.2.8.1 visibility condition at `sat_window.rs:201`), and each pixel's primary ray is the line from the satellite (sub-lon configurable; defaults `GOES_EAST_SUB_LON_DEG = -75.2` / `GOES_WEST_SUB_LON_DEG = -137.0`, `sat_window.rs:224-225`) through that scan direction. Raster extent = the scan-angle bounding rect of the WRF domain corners+edges, computed exactly like `window_scan_angle_rect` (`sat_window.rs:113`). Default pixel pitch 28 µrad (ABI 1 km class) for visible, 56 µrad (2 km class) for IR; both clamped so no output plane exceeds the domain crop.
   **Why this wins:** frames written on this grid drop straight into the existing sat store/player with a per-pixel lat/lon mesh from `rw_sat::geostationary::scan_angles_to_lat_lon` — and the **top-down map view then comes for free**, because the satellite map layer already resamples any stored frame onto the radar map (`SatRequest::LoadFrameForMap` → `SatMapSampleCtx` / `sample_sat_map_color` in `sat_paint.rs`). We do not build a separate map-view renderer.
2. **Free orbit/fly debug camera (dev tool).** Same volume, perspective camera reusing the `vol3d::Vol3dCameraMode::{Orbit, Fly}` conventions (`vol3d.rs:87-99`) for close-range inspection of cloud lighting. Not a v1 product surface; no store output.

*Rejected for v1:* a bespoke top-down orthographic camera (redundant with the map layer, above).

**Projection math (WRF grid → render frame).** WRF map projections are on a sphere, R = 6 370 000 m. Geometry frame = spherical ECEF.

- WRF column (i,j) → lat/lon: authoritative from `XLAT`/`XLONG` (already how `local_import.rs:310-316` builds `LatLonGrid`); the analytic Lambert/Mercator/polar-stereo/lat-lon forward+inverse (from `MAP_PROJ`, `TRUELAT1/2`, `STAND_LON` — exactly the attributes `local_import.rs::wrf_projection` (line 864) and `wrf_process.rs::wrf_projection` (line 1029) already read) is implemented in the new crate for the *inverse* direction used per ray step.
- Vertical: geometric height z = (PH+PHB)/g₀ (m MSL), resampled once at ingest onto uniform Δz levels (§2), so the brick's vertical axis is affine.
- **Ray march transform:** rays are straight lines in spherical ECEF (earth curvature handled exactly — across an 800 km domain the chord-vs-surface drop is ~12.5 km, larger than cloud depth, so a flat-tangent-plane shortcut is *not* acceptable). Per step: ECEF point → spherical lat/lon/h (closed form, no iteration on a sphere) → projection forward → fractional (i,j); (i,j,z) indexes the brick. Cost ≈ 25 ALU ops/step in WGSL — affordable at ≤ 256 steps (the ceiling `vol3d.rs::MAX_SHADER_STEPS` already ships).
- **Correctness ratchet:** a `cargo test`-able round trip asserting that projecting every Nth stored `XLAT/XLONG` through the analytic forward lands on its own (i,j) within 0.05 cell — same pattern as `sat_window::tests::ahi_forward_navigation_round_trips_the_app_inverse`.

## 2. Data model — streaming import → GPU volume bricks

**Ingest rules (hard, from `docs/wrf-import-large-grids.md`):**
- Never `getvar` 3-D fields: wrf-core's `WrfFile` memoizes every 3-D **f64** intermediate (~400 MB each at 50.5 M cells) until timestep change; that produced the measured 8.87 GB peak. SimSat ingest reads raw variables one at a time via `WrfFile::read_var` (the "fast reads" path that took light import 1241 s → 91 s, `local_import.rs` `PlaneSource`) and does its own cheap arithmetic (T = (θ′+300)(p/p₀)^κ with p = P+PB; z = (PH+PHB)/g₀ with destaggering).
- One 3-D field resident at a time; each is folded into the (much smaller, quantized) brick accumulation buffers and dropped before the next read. Peak RSS target < 2.5 GB on the Enderlin file (vs ~200–400 MB per raw field).
- Ingest worker thread calls `wrf_process::lower_import_thread_priority()` (`wrf_process.rs:262`, `THREAD_PRIORITY_BELOW_NORMAL`) — the owner's machine has already hard-crashed under all-core memory-bandwidth load; that lesson is inherited wholesale, including the confirm-first size gate UX from `local_import.rs`.

**Fields read per timestep:** 3-D `QCLOUD, QRAIN, QICE, QSNOW, QGRAUP, QVAPOR, T, P, PB, PH, PHB`; 2-D `HGT, TSK, LANDMASK, SNOWH (if present), U10, V10, XLAT, XLONG` (+ `IVGTYP` best-effort — note the known wrf-core "no layout message" fallback for `IVGTYP/ISLTYP` documented in `wrf-import-large-grids.md`; fall back to netcrust exactly as `local_import.rs` does).

**Brick format (GPU-resident, per timestep):**
- Horizontal axes = WRF (i,j) (possibly decimated, below); vertical = uniform Δz (default 250 m, surface→20 km ⇒ 80 slices), log-pressure-free because we resample by geometric height at ingest.
- Texture A `Rgba8Unorm` 3D: (ext_liquid, ext_ice, ext_precip, τ_up) — per-channel log quantization with per-volume scale in the header. ext_ice merges QICE+QSNOW (shared optics, §4); ext_precip merges QRAIN+QGRAUP. τ_up = cumulative vertical optical depth to space, precomputed at ingest — feeds cloud ambient, the IR fast path, and the shadow-map pass.
- Texture B `R16Float` 3D: temperature (needed at ~0.1 K fidelity for IR; 8-bit would alias the 1 K enhancement steps in `sat_worker.rs::ir_enhancement_anchors`).
- 2-D domain textures: HGT (+derived normals), horizon map (§6), LANDMASK/albedo modifiers, TSK, SNOWH, 10 m wind.
- This follows the `vol3d.rs` precedent (R8Unorm 3-D volume + 2-D floor textures, uploads drained in `prepare` via a `PendingUploads`-style `Arc<Mutex<…>>`) but with its own resources struct in `egui_wgpu::CallbackResources`.

**Windowing/LOD (no-full-residency, GPU side):**
- Voxel budget knob (default 64 M voxels ⇒ Texture A ≈ 256 MB + Texture B ≈ 128 MB). Enderlin 800×800×80 = 51.2 M voxels fits natively. A CONUS 3 km grid (1799×1059×50 ≈ 95 M) auto-decimates 2× horizontally, OR the user draws a **native window** — same concept and UI vocabulary as `sat_window::SatNativeWindow`.
- Exactly one timestep resident + one upload in flight (double buffer). Animation streams bricks from the disk cache; it never re-reads wrfout.

**On-disk cache:** `{settings data dir}/simsat/{run_id}/t{HHMM}.ssb` + `run.json` manifest (JSON: grid/projection attrs, quantization scales, channel list, format version). Payload = the quantized brick slices, flate2-compressed (workspace already ships `flate2` with zlib-rs). Enderlin timestep ≈ 384 MB raw → ~120–200 MB on disk; 20-frame run ≈ 3–4 GB, under the user-visible data-dir override (`settings::set_data_dir_override`, main.rs:7532). *Rejected:* reusing `rw_store::HourWriter` `.rws` frames for volume data — it is a 2-D-plane store; 80 slices × 6 channels per hour would fight its contract and its reader costs.

**Animation:** each rendered timestep is written as one frame into the app's sat store (§7 mechanics); the existing `rw_ui::SatPlayerPanel` (main.rs:2709) provides play/scrub/loop with zero new player code.

## 3. Atmosphere (clear-sky)

**Chosen family: Hillaire 2020, "A Scalable and Production Ready Sky and Atmosphere Rendering Technique" (EGSR 2020; the UE sky-atmosphere)** — transmittance LUT (256×64), multiple-scattering LUT (32×32), per-frame sky-view LUT (192×108), aerial-perspective froxel volume (32×32×32). All four recompute in well under 1 ms, sun direction is free to move per frame, and — critical for this brief — the sky-view LUT natively produces correct **twilight** (sun below horizon: only high-altitude scattering survives the earth-shadow test embedded in the transmittance/raymarch, giving the blue-hour gradient and the dark segment). *Runner-up rejected:* Bruneton & Neyret 2008 / Bruneton 2017 "new implementation" — the 4-D scattering LUT is heavier to precompute and port to WGSL, and its advantages (view-from-space multiple scattering fidelity) don't pay off for a regional-domain geostationary crop; Hillaire is the industry descendant of it anyway.

- **Sun below horizon:** direct term uses per-pixel transmittance-to-sun including earth intersection with the *finite disk* (§6) — the sun sets over ~2 minutes of real time instead of a step; ambient continues from the sky-view LUT (twilight illumination requirement).
- **Aerosol/Mie:** single exponential Mie layer (H≈1.2 km), Cornette-Shanks phase, configurable AOD (default 0.10).
- **WRF moisture modulation (honest approximation, named):** clear-sky Rayleigh/ozone stay standard-atmosphere; water-vapor broadband absorption in the transmittance is scaled by WRF precipitable water (vertically integrated QVAPOR) relative to the US-standard 14.2 kg/m² column, and optional RH-driven aerosol swelling scales the Mie coefficient. We do **not** rebuild the LUTs from full WRF profiles in v1 (documented limitation; the LUT parameterization hook makes it a later drop-in).

## 4. Clouds — raymarch design

**Chosen family: Frostbite/Nubis-style single-pass raymarch — dual-lobe Henyey-Greenstein + Wrenninge multi-octave multi-scatter approximation + beer-powder + LUT-fed ambient** (Hillaire, "Physically Based Sky, Atmosphere and Cloud Rendering in Frostbite", SIGGRAPH 2016 course; Schneider, Nubis SIGGRAPH 2015/2017/"Nubis3" 2022; Wrenninge et al., Oz multi-scatter octaves). *Runner-up rejected:* precomputed multiple-scattering LUT tables per (τ, g, sun angle) — better energy accuracy for deep clouds but a large offline bake per optics config, and WRF-driven per-voxel mixed-phase optics break the LUT dimensionality; octaves degrade gracefully instead.

- **Steps:** adaptive — coarse 2× voxel-pitch steps through empty space using a low-res occupancy mip of Texture A, refined to ~0.5× pitch inside cloud; hard cap 192 primary steps (interactive) / 384 (offline frame). Sun visibility per step comes from the **sun OD map** (§6) — one 2-D fetch — plus 3 short-range detail taps toward the sun for local self-shadowing (the Nubis pattern), instead of a full secondary march.
- **Phase:** dual-lobe HG, liquid g₁=0.85/g₂=−0.15 (w=0.9), ice g₁=0.75/g₂=−0.1; single-scatter albedo 1.0 visible (conservative).
- **Multi-scatter:** 3 Wrenninge octaves (σ×0.5ᵏ, g×0.5ᵏ, summed) — states clearly in-shader that this is the energy-gain approximation of record.
- **Beer-powder:** Schneider's e^(−τ)(1−e^(−2τ)) sugar-powder term, applied only to the sun term — named in code as a stylization with a physical rationale (missing forward-scatter buildup), toggleable.
- **Ambient:** per-voxel top/bottom sky irradiance from the sky SH-2 (§6) attenuated by e^(−τ_up) (channel 4) and an analogous cheap downward estimate — this is exactly "shadowing that accounts for how much sky is occluded and its color distribution" applied to clouds.
- **Density → optics (hydrometeor class mixing):** β_ext = (3/2)·ρ_air·q/(ρ_w·r_e) per class; r_e: cloud liquid 10 µm, ice/snow 40 µm, rain/graupel 1 mm (so precip reads as a translucent gray veil, not cauliflower). Class extinctions carried separately in Texture A so liquid vs ice phase/albedo mix per sample. Constants live in one documented table in `optics.rs` with the derivation.
- **Resolution honesty:** a 3 km grid cannot represent cloud-edge texture below ~6 km wavelength — renders will look "airbrushed" and that is the honest output; the 250 m Enderlin run is where the engine shows real structure. **Sub-grid noise: OFF by default.** An optional "stylized detail" toggle modulates extinction with monotone-preserving Perlin-Worley noise that (a) conserves column optical depth in expectation, (b) never creates cloud where q=0, and (c) is labeled non-physical in the UI. It stays honest by construction, not by tuning.

## 5. Surface

- **Blue Marble NGB:** NASA Blue Marble Next Generation monthly composites (Visible Earth collection 1484) — 12 months, global at 500 m (8×21600² tiles, ~1.4 GB/month), 2 km (21600×10800), 8 km (5400×2700); public domain. **Plan:** lazy per-month download of the global 2 km JPEG (~30–50 MB/month; a render needs at most 2 adjacent months for the day-of-year lerp, so first-use cost ≈ 60–100 MB; full-year cache ≈ 400–600 MB), re-hosted as GitHub release assets with a sha256 manifest — reusing the **`self_update.rs` streaming-download + SHA-256 gate machinery** (the real downloadable-asset mechanism in this app; note that `data_packs.rs` today is radar *scene* metadata (`BuiltInDataPack`), so SimSat adds a sibling `AssetPack` concept surfaced in the same Data Packs UI). Optional later: 500 m regional crop packs. Domain crop is resampled to a ≤4096² texture at load (two months bound for the seasonal lerp; `image` crate already decodes JPEG).
- **Soil/vegetation adjustment (stretch, honest):** optional albedo nudge from `IVGTYP` classes; v1 default off.
- **Water & glint:** `LANDMASK` gates water; specular = **Cox-Munk (1954) wind-ruffled slope distribution** driven by WRF `U10/V10` (the remote-sensing-standard glint model — the runner-up, GGX-with-wind-mapped-roughness, is equivalent-but-less-citable). Glint integrates the finite solar disk (§6) so the glitter pattern has correct angular extent; sky reflection from the sky-view LUT via Fresnel elsewhere.
- **Terrain:** `HGT` (already imported as `orography`, `local_import.rs:365`) → normal map + slope/aspect shading; cast shadows via **horizon maps** (§6); `SNOWH` blends a snow albedo with a soft ramp when present.

## 6. Direct + indirect light

- **Finite solar disk:** angular diameter 0.533° (half-angle 4.65 mrad); Hestroffer-Magnan limb darkening for the glint image of the disk.
  - *Terrain penumbra:* per-texel **horizon map** over `HGT` (16 azimuths × max horizon elevation, R16F, precomputed once per domain — 800×800×16 ≈ 20 MB, seconds on CPU/rayon). Shadow = smoothstep of (sun elevation − horizon angle) across ±0.267°, i.e., the physically correct fraction of the disk above the local horizon. *Runner-up rejected:* per-pixel heightfield ray-trace per frame (exact but per-frame cost with no reuse; horizon maps amortize).
  - *Cloud penumbra:* the sun OD map (next bullet) is sampled with a blur radius = occluder distance × tan 0.533° (mip-selected), giving distance-widening soft shadows — named approximation (pre-blur instead of disk-sampling the volume).
- **Sun OD map:** per timestep, a sun-aligned orthographic optical-depth map (1024², R16F) accumulated through Texture A by compute pass; provides (a) cloud shadows on terrain, (b) the raymarcher's long-range sun transmittance, (c) attenuation of glint.
- **Sky-visibility ambient (the "how much sky and what color" requirement):** each frame, project the sky-view LUT into **SH-2 (9 RGB coefficients)** in a tiny compute pass. Receivers: terrain uses an ambient-aperture cone from its horizon map (mean visible elevation + gap fraction; Oat & Sander 2007 ambient-aperture family) dotted against the SH; cloud voxels use the τ_up/τ_down attenuated SH evaluation (§4). This gives directional, colored ambient — orange-side vs blue-side fill at sunset — for both terrain and cloud shadows. *Runner-up rejected:* per-texel SH visibility bake (correct but a heavy per-domain bake for marginal v1 gain over the cone).
- **Energy & tonemap:** internal HDR radiance, band-averaged RGB, physical scale (sun band irradiance ≈ TSI 1361 W/m² split by CIE-ish weights). Two output transforms: (a) **"ABI-like" reflectance factor** ρ = πL/E_band with the standard sqrt-ish satellite stretch — the product default, because it is what "looks like GOES"; (b) filmic/ACES-style display mode for the debug camera. All approximations (gray bands, SH-2 ambient, octave multi-scatter, pre-blurred penumbra) are named in code comments and in this doc — that is the honesty standard.

## 7. IR mode

Shares the volume bricks. A dedicated small pass marches each **IR output pixel's actual slant ray** (geostationary geometry, so limb slant is honest) top-down through Texture A/B accumulating gray-body emission: B(T_voxel) with mass-absorption constants per class (ice ≈ 0.07 m²/g, liquid ≈ 0.15 m²/g at 10.3 µm — documented, tuned so optically thick anvil BT ≡ cloud-top T), surface term = ε·B(TSK) through the remaining transmittance, weak WV continuum from the QVAPOR column. Output = **true Kelvin BT plane**, written to the sat store as a single-band `surface2d` variable with a `{"satellite": {... "band": 13 ...}}` selector (the shape `write_himawari_grid_frame`/`himawari_selector` already writes, `sat_worker.rs:1715-1742`), so `load_colored_frame` → `render_sat_pixels` → `ir_enhancement_anchors(13, enhancement)` (`sat_worker.rs:1389`) applies the existing CIMSS/BD/AVN/Funktop/Rainbow/Grayscale enhancements live, with the picker and re-color plumbing (`SatRequest::SetIrEnhancement`, `sat_paint.rs:443-500`) unchanged. Synthetic BT medians ~280 K, so the legacy-stretch heuristic (`sat_worker.rs:1196-1200`) classifies it as true Kelvin.

Visible frames are written as the three baked `rgb_r/g/b` planes (`COMPOSITE_R_VAR` et al., `sat_worker.rs:1085-1087`) — the composite path renders them untouched. Run naming follows the store convention `{token}_c{band:02}_{day}` under a `simsat` model dir in `settings::sat_store_dir()` (`settings/src/lib.rs:1210`), one `grid.rwg` per run with the bit-identical-grid reuse rule `write_himawari_grid_frame` implements.

## 8. Performance budget

- **Baseline hardware (the floor): modern desktop iGPUs** — Intel Arrow Lake Xe (Core Ultra 270K-class) and AMD RDNA2 iGPU (Ryzen 9950X-class, ~2 CU — genuinely entry silicon). Discrete GPUs (RTX 3070+) are the fast path, not the requirement. Consequences baked into every pass:
  - Tiling discipline stays (**no single dispatch/draw > ~4 ms**, TDR safety) but tile counts scale up ~10–40× on iGPU; all passes are progressive and cancelable.
  - Unified memory: "VRAM" budgets are shared-system-RAM budgets — allocation is cheap, bandwidth is the scarce resource. Bandwidth-lean choices are mandatory: quantized bricks (already RGBA8), occupancy-mip empty-space skipping, half-resolution cloud march + bilateral upsample as the iGPU default, LUT reuse across frames.
  - Quality is NEVER cut on stored frames — iGPU changes *how long* an offline frame takes, not what it looks like.
- **Interactive preview budget:** dGPU: 1024–1440p at 30 fps (sky LUTs ≤ 0.6 ms · sun OD ≤ 2 ms · surface ≤ 2 ms · cloud march ≤ 20 ms · compose ≤ 1 ms). iGPU: adaptive — start 720p half-res march and scale until frame time holds ~30 fps (Arrow Lake Xe) / ~15 fps floor (RDNA2 2 CU); preview exists to frame the shot, prerender is the product on these parts.
- **Offline "player frame" budget:** full quality always. dGPU: ≤ 2 s per 2048²-class frame (20-frame Enderlin loop ≤ 1 min). iGPU targets: ≤ 20 s/frame Arrow Lake Xe, ≤ 60 s/frame RDNA2 2 CU (20-frame loop = grab a coffee, then it plays back instantly as stored textures). Player cap 4096² (`sat_paint.rs:1429` test) unchanged.
- **Memory ceilings:** GPU default 2 GB total (owner call: liberal VRAM for an advanced product; may expand further while the radar app is idle) — brick A/B sized by the voxel-budget knob, Blue Marble 2×4096² ≈ 128 MB, LUTs/OD/horizon ≈ 30 MB, render targets, generous double-buffer headroom. Exposed as a settings knob mirroring `radar_history_budget_gib` (main.rs:7547). CPU ingest peak < 2.5 GB (§2) — that constraint is about system stability, not stinginess, and stays.
- **LUT precompute:** transmittance+multiscatter ≈ 0.3 ms once per optics config; sky-view+froxels per frame ≤ 0.5 ms; horizon map once per domain (seconds, CPU, below-normal thread).

## 9. Crate/module layout

- **Two new crates: `crates/simsat` (engine lib) + `crates/simsat_studio` (the standalone app binary).** SimSat Studio is its own eframe/wgpu desktop app: open wrfout files (or a cached run), pick satellite/timestep/mode, render, play/scrub loops, export PNG frames, and write runs in sat-store format for BowEcho. It reuses workspace UI conventions but owns its window — no BowEcho pane in v1 (the sat-store output contract is the bridge; an embedded pane can come later without engine changes).
- **`crates/simsat`** (renderer + ingest; keeps app_ui/main.rs untouched — nothing here lands in BowEcho's binary at all in v1):
  - `ingest.rs` (wrfout → bricks; deps: `wrf-core` for `WrfFile::read_var`, netcrust only for metadata fallback — mirroring `local_import.rs`'s split), `bricks.rs` (.ssb format, quantization, cache), `frame.rs` (projections, geostationary camera, ECEF math), `optics.rs` (hydrometeor constants + CPU reference kernels), `atmosphere.rs` (LUT generation, CPU reference), `ir.rs`, `gpu/` (pipelines/resources/callbacks à la `Vol3dResources`/`Vol3dCallback`), `gpu/shaders/*.wgsl` via `include_str!` (unlike vol3d's inline `const SHADER` — separate files are testable and diffable).
  - Deps: `eframe = { workspace = true, features = ["wgpu"] }` so `eframe::egui_wgpu::wgpu` stays the single workspace wgpu (29.0.3 per Cargo.lock); `rayon`, `flate2`, `image`, `serde/serde_json`, `chrono`. Dev-dep: `naga` for shader validation.
- **BowEcho integration (deferred, by contract not code):** SimSat Studio writes runs under a `simsat` model dir in sat-store format (§7); BowEcho's existing sat store/player reads them when pointed at the directory. The previously-sketched in-app glue module (`simsat_ui.rs` dock pane) is explicitly NOT v1 — if ever wanted, it slots in later against the same engine crate with the patterns already identified (vol3d-style `init_gpu`, glow-fallback notice at main.rs:1260-1264).
- **Test strategy (nodes are headless Linux — no GPU, no golden images):**
  1. Pure-math unit tests: projection round trips (XLAT/XLONG self-consistency), scan-angle round trips vs `rw_sat::geostationary`, quantization round trips, horizon-map on synthetic ridges, penumbra fraction at known geometries.
  2. **CPU-reference sampling tests:** every WGSL kernel has a Rust twin in `optics.rs`/`atmosphere.rs`; deterministic 32×32 CPU raymarches of tiny analytic scenes (uniform slab, single box cloud) asserted against pinned arrays with tolerance — real regression power, runs in `cargo test`.
  3. `naga` parse+validate every `.wgsl` in a unit test (catches shader breakage headlessly).
  4. Env-gated real-fixture tests (`SIMSAT_WRF_FIXTURE=...`, release-profile only) following the `RW_LOCAL_IMPORT_FIXTURE`/`RW_WRF_CRASH_FIXTURE` precedent for ingest wall/RSS assertions.
  5. GPU-vs-CPU parity harness as an example binary, run manually on the dev box (documented, not CI-gated).

## 10. Milestone build order

Dependency-ordered; every gate = fmt/clippy/test on nodes + a real-data proof render from the Enderlin run on the dev box; no milestone is a product stopping point.

| # | Deliverable | Proof standard |
|---|---|---|
| M0 | `simsat` crate scaffold; .ssb brick format; streaming ingest of one wrfout timestep | Enderlin 02:15 file → brick cache, logged peak RSS < 2.5 GB, wall < 60 s release; quantization/projection tests green |
| M1 | SimSat Studio window (open run → render → view) + geostationary camera + surface pass (Blue Marble single-month dev texture, HGT normals, LANDMASK, point sun) + **sat-store-format frame writer** | Studio renders and displays a frame; the written run ALSO plays in BowEcho's Satellite window when the store dir is pointed at it |
| M2 | Hillaire atmosphere LUTs + aerial perspective + finite-disk sun + twilight | Enderlin 02:15 UTC (≈21:15 CDT, sun on the horizon) renders a credible sunset limb-lit frame; 05:15 renders night |
| M3 | Horizon maps: penumbral terrain shadows; Cox-Munk glint; SNOWH blend | Low-sun frame with measured shadow lengths consistent with HGT; glint streak on water bodies |
| M4 | Cloud raymarch v1: adaptive steps, sun OD map, dual-HG, beer-powder, SH ambient | The Enderlin supercell at a daylight timestep, side-lit, with cloud shadows on ground |
| M5 | Multi-scatter octaves; penumbral cloud shadows; full sky-visibility ambient on terrain+cloud | Anvil-shadow scene: shadow core vs blue-ambient fill visibly correct at sunset |
| M6 | IR mode → store as Kelvin band-13 frame → existing enhancements | Same timestep through Rainbow/BD in the player; anvil BT ≈ cloud-top T from the sounding |
| M7 | Multi-timestep prerender pipeline; Blue Marble asset pack (download+sha256, 12-month lerp); budgets/knobs; native-window LOD | Full Enderlin loop plays in `SatPlayerPanel`; pack downloads cold on a clean profile |
| M8 | Hardening: tiling/TDR discipline on iGPU, docs, parity harness | Full prerender+playback session on an Arrow Lake Xe iGPU box within the §8 iGPU budgets; BowEcho running alongside without either app hitching |

## 11. Risks & open questions

**Top risks + mitigations:**
1. **Memory/IO pressure on ingest** (the machine-crash class of bug): mitigated by raw `read_var` streaming, one-field residency, below-normal priority, size gates — all existing, measured patterns (`docs/wrf-import-large-grids.md`). Tripwire: fixture test asserts peak RSS.
2. **GPU contention with the live radar app / egui queue stalls / Windows TDR:** every pass tiled ≤ 4 ms; offline frames time-sliced across egui frames; kill-switch pauses prerender while a radar loop is animating.
3. **wgpu/naga feature limits** (no shader-f16 without feature; 3-D texture size limits at 2048): f32 ALU only, f16/rgba8 as storage formats only; brick capped ≤ 2048 per axis; naga-validate in CI.
4. **Honesty drift** (sub-grid noise or tuned constants quietly becoming "the look"): noise off by default and labeled; all optics constants in one reviewed table; IR constants validated against a real GOES scene of comparable convection.
5. **Blue Marble distribution friction** (hosting, size, offline use): NASA imagery is public domain; release-asset hosting + sha256 manifest reuses the shipped `self_update.rs` pattern; per-month lazy fetch keeps first-use ≤ ~100 MB; bundled 8 km emergency fallback (~10 MB) so the renderer never hard-fails offline.

**Owner decisions (2026-07-07 — "advanced product, best possible within reason, no supercomputer shit"):**
1. **Memory: liberal, now unified.** Default budget 2 GB (knob exposed). Since the hardware floor moved to iGPUs (decision 7), this is shared system RAM — allocation is cheap there; the §8 bandwidth-lean rules are what keep iGPU fast.
2. **Polish: everything.** No either/or — offline frames render at full quality (384+ steps, full-res), interactive preview holds 30 fps by adapting resolution/steps, both paths get finish work. When a milestone forces sequencing, offline/loop quality lands first within that milestone, preview parity immediately after.
3. **Satellites: East/West/Himawari selectable at v1** (shared camera math; sub-lon + AHI/ABI pixel-pitch presets).
4. **Blue Marble: full-year 2 km pack (~500 MB) is the default download**; lazy per-month remains as the low-bandwidth option; 500 m regional crop packs greenlit as follow-on. GitHub release-asset hosting accepted.
5. **Spherical earth accepted** — the standard is physical plausibility, not pixel registration with real ABI imagery.
6. **QVAPOR gets a full brick channel now** so a 6.2 µm water-vapor IR band is a shader-only addition later.
7. **Standalone tool (2026-07-07 addendum):** SimSat ships as its own app (`simsat_studio`), not a BowEcho pane; the sat-store output format is the only bridge to BowEcho in v1.
8. **iGPU hardware floor (2026-07-07 addendum):** must run well on Intel Arrow Lake Xe (Core Ultra 270K-class) and AMD RDNA2 iGPU (Ryzen 9950X-class): prerender-then-play is the primary iGPU mode, stored-frame quality is never reduced, preview adapts. Discrete GPUs are the fast path.

---

## 12. Implementation handoff — read this first if you are the implementing agent

This section exists so the plan is executable without the context of the session that wrote it.

**Process rules (owner mandates, non-negotiable):**
- NEVER run cargo/rustc on the owner's Windows machine (a PreToolUse hook blocks it). The one sanctioned local build is a release smoke (`env -u CARGO_BUILD_JOBS cargo build --release -p <bin>`) when producing an exe for the owner to run.
- All verification runs on the build nodes over ssh: push your branch to the node's bare repo first (`git push node3|node4 HEAD:refs/heads/<branch>` — node3 = drew@192.168.68.56, node4 = drew@192.168.68.57; the verify script fetches from the node's LOCAL repo, pushing to origin alone fails with exit 3), then run each gate FOREGROUND: `ssh -o BatchMode=yes drew@<ip> "flock -w 3600 /tmp/bowecho-verify.lock env BOWECHO_BRANCH='<branch>' bash ~/bowecho-verify.sh <cargo args>"` for `fmt --all --check`, `clippy --workspace --all-targets -- -D warnings`, `test --workspace`. Never leave a gate unverified; on a client timeout, re-run (node caches make retries fast).
- Work in a git worktree branched from the integration tip; commit early and often (uncommitted worktrees have been lost); checkpoint findings to a notes file under `C:\Users\drew\radar-work\` as you go.
- Nodes are headless Linux: no GPU, no golden-image tests — the §9 test strategy (CPU reference kernels, naga validation, pure-math round trips) is the CI story; visual proofs happen on the owner's box via a locally built exe the owner runs.
- Never touch `~/weather` on the nodes (owner data). Do not add lines to `crates/app_ui/src/main.rs` (a ratchet test enforces a shrinking ceiling).

**Owner-side facts:**
- The Enderlin canonical fixture lives on the owner's machine under his `wrf_demo` folder (2.4 GB, 800×800×79); ask the owner to confirm the path before M0 ingest testing, and NEVER import it with the heavy full-diagnostics path.
- Proof loop per milestone: build `simsat_studio` release exe locally (sanctioned smoke), copy to the owner's Desktop with a clear name, owner runs it and reports; treat owner reports as the acceptance test.
- Blue Marble source: NASA Visible Earth collection 1484 (public domain), 12 monthly global composites; 2 km JPEGs ≈ 30–50 MB/month; re-host as GitHub release assets with a sha256 manifest (streaming-download pattern precedent: `crates/app_ui/src/self_update.rs`).

**Order of work = §10 milestones, one at a time, each fully gated before the next.** Scaffold M0 by hand-writing the two crates' `Cargo.toml` + adding them to the workspace `members` (you cannot run `cargo new` locally); first gate run on a node will validate the scaffold. Every §1–§7 subsystem names its repo precedent file — read the precedent before writing the subsystem. When a detail here conflicts with what you find in the code, the code wins and this doc gets a correcting commit.

---

## Appendix — critical files for implementation

- `crates/app_ui/src/vol3d.rs` — the wgpu volume-raymarch + egui_wgpu callback pattern SimSat generalizes
- `crates/app_ui/src/sat_worker.rs` — sat store contract, `rgb_r/g/b` composites, Kelvin BT + `ir_enhancement_anchors` — the output surface
- `crates/app_ui/src/local_import.rs` — fast `read_var` plane reads, `wrf_projection`, import worker/priority pattern the ingest copies
- `crates/app_ui/src/sat_window.rs` — geostationary navigation math the camera reuses
- `docs/wrf-import-large-grids.md` — binding memory constraints + Enderlin fixture facts
