# Working with BowEcho — the agent guide

BowEcho: Rust/egui/wgpu weather radar + satellite desktop app (Windows/mac/Linux).
Owner: drew. You are expected to work like the sessions that built v0.29–v0.30:
recon by reading code, implement in a worktree, gate on the build nodes, report
like an engineer. This file tells you where everything lives and the rules that
are not negotiable.

## Repos, branches, remotes

- Dev repo: `C:\Users\drew\radar-work\wt-experiments` — branch `integration/loopfix-plus-rusty` is the integration line. Remote `claude` = github FahrenheitResearch/radar-rs-analyst-claude (push integration here).
- Public repo: github FahrenheitResearch/bowecho (remote `bowecho`). Its `main` is ONLY ever fast-forwarded to a vetted integration commit at release time. Never commit dev churn to it.
- Build nodes have their own bare repos (see gates below).

## HARD RULES (violating these is how agents get fired)

1. **Don't SWARM this Windows machine with concurrent/heavy compilation — that is the ONLY restriction, NOT a ban on using the machine.** The owner's actual rule is "don't have 19 agents all heavy-compiling at once, use the Linux nodes for that" — it was NEVER "never run anything on Windows." A PreToolUse hook blocks ad-hoc `cargo`/`rustc` purely to enforce the no-swarm rule; the sanctioned single build is allowed: `env -u CARGO_BUILD_JOBS cargo build --release -p app_ui --bin bowecho` (background it; copy exe to Desktop as `BowEcho vX.Y RCn.exe`). **You CAN and SHOULD use this machine to RUN things** — launch the built `bowecho.exe`, drive/time it, and test against the owner's real files (e.g. `C:\Users\drew\Downloads`). Running the compiled app is NOT `cargo` and is exactly how you reproduce, verify, and MEASURE (e.g. time WRF processing). NEVER conclude "I can't run anything on Windows" — that is false and has burned a whole night. Heavy/parallel verification (workspace `fmt`/`clippy`/`test`) goes to node3/node4 (rule 2).
2. **All verification on the build nodes** — node3 = `drew@192.168.68.56`, node4 = `drew@192.168.68.57`:
   - First: `git push node3|node4 HEAD:refs/heads/<branch>` — the verify script fetches from the NODE's local bare repo (`/home/drew/bowecho.git`); pushing only to origin exits 3.
   - Then each gate FOREGROUND (Bash timeout 600000; NEVER background-and-walk-away; on client timeout just re-run — node caches make retries fast):
     `ssh -o BatchMode=yes drew@<ip> "flock -w 3600 /tmp/bowecho-verify.lock env BOWECHO_BRANCH='<branch>' bash ~/bowecho-verify.sh <cargo args>"`
   - The gate trio for EVERY commit: `fmt --all --check` · `clippy --workspace --all-targets -- -D warnings` · `test --workspace`. Expect exact test arithmetic: baseline + exactly your new tests, 0 failed. Establish the baseline by running the base tip if unsure.
3. **Never touch `~/weather` on the nodes** (owner's data). Node4 builds live at /home/drew/weather/data/bowecho-build — the script handles it.
4. **WRF discipline**: small fixtures in tests; the Enderlin run (owner's `wrf_demo` folder, 2.4 GB, 800×800×79) at most 1–2 passes and only on nodes/owner request; NEVER the heavy full-diagnostics import on it; never `getvar` 3-D fields in bulk (wrf-core memoizes f64 intermediates → measured 8.87 GB; use `WrfFile::read_var` streaming — see docs/wrf-import-large-grids.md). Import workers run at below-normal thread priority (`wrf_process::lower_import_thread_priority`) — the owner's machine has hard-crashed under all-core memory-bandwidth load.
5. **Never claim fixed/working without real-data proof through OUR code path.** h5py/python decoding a file proves nothing about our decoder (this exact gap shipped a broken Spain integration for one RC). Precedent: ignored live tests run once on a node (`ord.rs`/`meteoromania.rs` live roundtrips), real-file fixtures vendored for regressions.
6. **Machine feeds only** — REST/S3/API endpoints; never scrape HTML for data (parsing an autoindex directory listing is fine).
7. **Credit policy**: Solarpower07 and grayskieswx by handle ONLY, never real names. Follow existing credit surfaces (README, `guide.rs` sources) — don't invent new ones. Color-table internal names are NOT user-facing text.
8. **Parallelism**: a few concurrent agents fine; never a swarm of build-heavy ones. Don't run heavy verifies while the owner is live-testing an RC.
9. **Checkpoint or die**: commit early and often on your branch (uncommitted worktrees have been lost to crashes); append progress/findings to `C:\Users\drew\radar-work\<task>-notes.md` as you go. On relaunch, resume from notes/branch.

## The workflow (per task)

1. Worktree off the integration tip: `git -C /c/Users/drew/radar-work/wt-experiments worktree add ../wt-<name> -b <branch> <tip>`. Work ONLY there.
2. Recon first, by CONTENT — line numbers in docs drift; find code by names. Write recon findings to your notes file before coding.
3. Implement. Prefer modules/crates when they provide coherent ownership, but the owner discontinued the `app_ui/src/main.rs` line-count ratchet on 2026-07-09; correctness fixes may land where they naturally belong. Refactor moves follow the move-only protocol in docs/main-decomposition-plan.md (byte-identical bodies; only use/mod/pub(crate) edits; verify with `git diff --color-moved=dimmed-zebra`).
4. Gate the branch tip (trio above). Report to the integrator with: branch, final sha, files changed, what moved/changed and why, visibility promotions, tests added, gate results with exact counts, anything deliberately left out.
5. Integration (if you are the integrator): squash-merge onto integration. If the branch was cut from the current tip, verify `git rev-parse HEAD^{tree} <branch-tip>^{tree}` are equal → gated-tree identity, no re-gate. Stale base or conflicts → resolve (understand WHY each conflict exists — a "clean" auto-merge once nearly duplicated a relocated test) and RE-GATE the merged tree on a node before pushing `claude`.

## Repo map — where everything lives

`crates/app_ui/src/` (the app; main.rs remains the largest coordination unit):
- `main.rs` — ViewerApp struct (~330 fields), radar loading/loop engine, eframe update. ~64k lines after phase-1 decomposition and subsequent fixes. The map-paint block is now extracted (see `map_paint.rs`); `single_pane_canvas`/`grid_canvas` (input + paint dispatch) and the `handle_*_click` cluster stay here by design. Palette gate for model layers: `model_layer_solar_table` — precedence: user 🎨 override → Solar table → production style → viridis fallback. Prefer cohesive module ownership, but there is currently no line-count ceiling.
- Extracted modules (v0.30): `self_update.rs` (update checks; also the streaming-download + sha256 pattern), `hazard_geom.rs` (pure hazard geometry/parse/fill), `hazard_ui.rs` (hazard panels/paint), `product_select.rs` (product/cut selection), `settings_ui.rs` (settings panels), `sat_paint.rs` (sat window/worker pump/map layer/LUT cache), `geo_helpers.rs`, `map_paint.rs` (projection AEQD + GeoBounds, map chrome/labels/colorbar/mode-chip/cursor-inspector+loupe, site/intl/community markers, basemap+radar-raster+tiles layers).
- Satellite: `sat_window.rs` (geostationary navigation math — CGMS scan angles, GOES/Himawari sub-lons, native windows), `sat_worker.rs` (ingest workers, true-color composites, true-Kelvin IR + `ir_enhancement_anchors` BD/CIMSS/etc., sat store write contract `{token}_c{band:02}_{day}`).
- Tropical: `tropical.rs` (storm cards incl. 🛰 Vis/IR one-press sat views, JTWC vitals).
- Model/WRF/Formula Lab: `model_data.rs` owns one shared worker/state with three first-class UI surfaces: **Models** for the run library/field viewer/plotter/soundings, **WRF** for raw import/diagnostics/GDEX/simulated radar, and the dockable **Formula Lab** workspace for bounded custom diagnostics. Never create a second model dock for Formula Lab: it shares the selected store run/time and installs results into the same Models/map/native-plot pipeline. `formula_lab.rs` owns editor persistence, adaptive starters, exact-token field discovery, source-readiness checks, background evaluation, and provenance. Store-backed pointwise formulas are model-slug agnostic but depend on real manifest fields/units; store horizontal calculus is deliberately blocked because rw-store lacks grid metrics, while raw WRF supplies the full grid-aware resolver. `attach_solar_fallback_style` is the seam where style-less `wrf_*` fields receive Solar palettes and friendly titles. `model_data/iso_fields.rs` synthesizes 925–250 mb display fields; those synthetic slugs are not Formula Lab variables unless they really exist in the store. `model_layer.rs` owns the map layer; `local_import.rs` is the light 2-D + sounding path; `wrf_process.rs` is full diagnostics; `wrf_volumes.rs` performs native→isobaric interpolation. `wrf_radar.rs` + `wrf_radar_physics.rs` implement the synthetic operator: named user recipes, linear-Z sampling, center/9/27-point pulse-volume integration, scatterer-weighted Doppler/fall speed, spectrum width, terrain blockage, timed rays, sensitivity/folding, and scheme-aware bulk S-band dual-pol/propagation. It is not a T-matrix solver; P3/ISHMAEL fall back explicitly. `vol3d.rs` is the wgpu volume-raymarch precedent.
- Other: `layers_rail.rs`, `overlays.rs`, `dock.rs`, `guide.rs` (in-app manual + credits/sources), `brand.rs` (branding + canonical repo URL fallbacks), `italy_dpc.rs` (native Italy radar), `data_packs.rs` (radar scene packs).
- The former `main_rs_line_ratchet.rs` ceiling test was removed on 2026-07-09 at the owner's direction. Do not reintroduce a line-count gate unless the owner explicitly restores it.

`crates/color_tables/` — palettes: `solar.rs` (Solarpower07 WRF-Runner ports: reflectivity/temp/dewpoint/RH/wind/CAPE + per-level 250/500/700/850 mb + °C-native surface; level-aware resolver `solar_model_field_table(var, units)` — slugs like `temperature_850` hit level tables; unit-aware K/°C/°F), `wrf_fields.rs` (174-entry raw-wrf-var catalog: labels/descriptions/family hints), `iso_levels.rs` (iso slug↔label naming contract).

`crates/data_source/` — feeds: `international/ord.rs` (EUMETNET ORD/CloudFerro EU single-site radar: `ORD_LIVE_COUNTRIES`/`ORD_SITES` incl. Spain's 11 AEMET sites, split-PVOL merge grammar), `international/meteoromania.rs` (native ANM Romania dual-pol; in-place-rewrite feed quirk → 9-min settle window), `international/fixtures/` (listing fixtures). Site pickers consume `data_source::international::intl_static_sites()`.

`crates/nexrad_io/` — decoders: `hdf5lite.rs` (minimal HDF5 reader; v1 AND v2 'OHDR/OCHK' object headers with Jenkins lookup3 checksums — AEMET writes the v2 dialect), `odim.rs` (ODIM decode; `rstart > 20 km ⇒ metres` sanity rule for AEMET), real-file fixtures in `tests/data/`.

PINNED, UNMODIFIABLE deps (fix at app seams, never in them): rusty-weather crates (`rw_ui` — FieldViewerPanel/plot viewer/SatPlayerPanel/skew-T; `rw_store` — the store; `rw_sat` — geostationary math), `wrf-core` (getvar/read_var), `netcrust` (NetCDF). 12 pinned git deps total.

## Testing facts

- Nodes are headless Linux: no GPU, no golden images. Test with pure math, CPU reference kernels, real-file fixtures, `#[ignore]` live tests run once on a node for proof.
- Test counts are exact currency: every landing states `baseline + N new, 0 failed`. If your count drifts, something is wrong — find it.
- Env-gated heavy fixtures precedent: `RW_LOCAL_IMPORT_FIXTURE` / `RW_WRF_CRASH_FIXTURE`.

## RC + release process

- RC loop: build the sanctioned local release exe → `Desktop\BowEcho vX.Y RCn.exe` → owner live-tests → every complaint becomes a same-day fix agent → next RC. Owner reports are the acceptance test.
- Release: bump workspace version (root Cargo.toml + Cargo.lock, sed is fine, gates verify) → notes at `docs/releases/vX.Y.Z.md` → FF `bowecho/main` to the vetted tip + annotated tag `vX.Y.Z` → release.yml builds 7 signed assets (win x64/x64-v3/arm64, mac intel/AS, linux x64/arm64) → publish-release fires when ALL 7 succeed.
- Known trap: Apple notarization HTTP 403 "required agreement is missing" = Apple published new legal terms; owner clicks Agree at developer.apple.com, then `gh run rerun <id> -R FahrenheitResearch/bowecho --failed`. Certs/secrets are fine — do not debug them. Azure signing key is correct in secrets — never resurface "rotate secret".

## Plans & state (read the one for your task)

- `docs/main-decomposition-plan.md` — main.rs decomposition. **Phase 1 COMPLETE (2026-07-08): all 8 queue items landed incl. `map_paint.rs` (projection/chrome/markers/layers); main.rs 79,177 → 63,621, −19.6%.** REMAINING: phase 2 (LoopEngine/PaneView state absorption — touches real logic, feature-level review, separate approval) and phase 3 (crate split, unscheduled).
- `docs/simsat-engine-plan.md` — v0.31 headline: standalone physically-based simulated-satellite renderer (SimSat Studio), iGPU hardware floor. §12 is the implementer handoff — start there.
- `docs/wrf-import-large-grids.md` — binding memory constraints, Enderlin facts, upstream reader gaps.
- `docs/releases/` — shipped notes. `C:\Users\drew\radar-work\FABLE_BACKLOG.md` — owner's backlog/doc of record.
- Open follow-ups usually live in the session task list; standing ideas: WRF description-attr fallback labels; versioned T-matrix scattering LUTs plus true temporal interpolation/VCP behavior for simulated radar; France radar (needs OAuth + BUFR — own project); KNMI 7-year archive (needs KNMI-native HDF5 decoder).

## RESOLVED HANDOFF (2026-07-10): WRF severe/thermo DONE + v0.30.5 feature wave

The 2026-07-09 "UNFINISHED" WRF severe handoff is fully resolved. Integration `65432df`,
merged gate 2036 passed / 0 failed on node4.

- Raw wrfouts: wrench multi-file/folder import computes the full severe suite via wrf-core
  getvar. wrf-core pinned at `213068c` (wrf-rust branch codex/perf-science-integration):
  3.26x faster on the 79-diagnostic set (EIL scans cached, top_m CAPE + EL parallelized,
  composites reuse cached CAPE/DCAPE, parallel HDF5 chunk decompress) plus science fixes
  (satlift >=1000 hPa clamp, CIN layer selection, strict wrf-python CAPE/SRH parity, AVO/PVO
  map metrics). Timing WAS measured (node1, real 800x800x79 d03): 185.3 s / 24 threads /
  7.47 GB peak before the fixes; ~40-55 s class after. App-side: raw files probe
  WrfFile::open BEFORE the ~57 s netcrust attempt (order inversion in wrf_process.rs);
  `ncape` is classified with the Heavy/eCAPE group (needs the eCAPE toggle now).
- Post-processed WRF (GDEX wrf3d/wrf2d, derived TK/Z/P, no raw T/PB): `postproc_severe.rs`
  computes the 16-field severe suite through the PUBLIC slice-based `wrf_core::met` kernels.
  sharprs is NOT used anywhere in the WRF path; parked branch feat/wrf-postproc-severe is
  obsolete.
- GDEX Stage 1b browser merged (`gdex_ui.rs`). ERA-20C GRIB1 imports via `grib_import.rs`
  (grib-core at the pinned rusty-weather rev; vendored first-message fixture in tests/data).
- v0.30.5 feature wave (all merged at `65432df`): batch auto-plot (`batch_plots.rs` -
  headless rustwx-render PNGs of every field incl. 925-250 mb iso levels, auto after
  wrench/light/GDEX imports + manual button, index.json manifest, progress/cancel); synthetic
  radar CfRadial export (`radar_export.rs` - round-trips bit-identical through our cfradial
  reader; sweep_mode char var omitted, strict py-ART wants it); update chip opens the in-app
  Settings updater instead of the GitHub page; adaptive per-monitor startup window size.
- Audits on record in the radar-work root (next to this repo): `wrf-perf-notes.md`
  (measurements + ranked fixes), `wrf-science-audit-notes.md` + `wrf-science-audit-raw.txt`
  (3 confirmed majors all fixed in 213068c; ~25 unverified leads remain - extend the parity
  harness to the SHARPpy-lineage composites SHIP/VTP/TEHI/critical-angle first).
- Nodes: node3 build workspace relocated to the big volume (symlink); hourly disk guard +
  daily 3-day model-file retention sweep installed on node3. LIVE wxstore services run on
  node3 ports 8899 (cafire) and 8897 (rustfront) - do not delete their <3-day data window.
  node1 = 192.168.68.54, node2 = 192.168.68.50 (24c/123GB, rustup installed, good for
  wrf-rust benchmarks).

Open follow-ups: ERA-20C pressure-level (an.pl) profiles into the iso/sounding path; py-ART
sweep_mode nicety for CfRadial export; iso batch-plot read-once-slice-all optimization;
wrf-rust codex branch not merged to its master (the app pins the rev directly).

## Report style (what the owner expects)

NO decorative emojis — not in chat, release notes, docs, or commit messages (strict house rule). Referencing literal in-app button labels that contain icons is fine; decorating prose is not.
Lead with the outcome. Exact numbers (line counts, test counts, sha). Say what you deliberately did NOT do and why. Never claim visual behavior you couldn't verify — describe what the code does and let the owner's RC test confirm. When you find something mid-task that contradicts the brief, say so and adapt — the code is the truth, docs get corrected.
