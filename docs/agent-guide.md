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

1. **NEVER run cargo/rustc on this Windows machine.** A PreToolUse hook blocks it. The ONE sanctioned local build is the RC/release smoke: `env -u CARGO_BUILD_JOBS cargo build --release -p app_ui --bin bowecho` (run it in background; copy exe to Desktop as `BowEcho vX.Y RCn.exe` for the owner to test).
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
3. Implement. New feature code goes in modules/crates, never appended to `app_ui/src/main.rs` — a ratchet test (`crates/app_ui/tests/main_rs_line_ratchet.rs`) enforces a shrinking line ceiling. Refactor moves follow the move-only protocol in docs/main-decomposition-plan.md (byte-identical bodies; only use/mod/pub(crate) edits; verify with `git diff --color-moved=dimmed-zebra`).
4. Gate the branch tip (trio above). Report to the integrator with: branch, final sha, files changed, what moved/changed and why, visibility promotions, tests added, gate results with exact counts, anything deliberately left out.
5. Integration (if you are the integrator): squash-merge onto integration. If the branch was cut from the current tip, verify `git rev-parse HEAD^{tree} <branch-tip>^{tree}` are equal → gated-tree identity, no re-gate. Stale base or conflicts → resolve (understand WHY each conflict exists — a "clean" auto-merge once nearly duplicated a relocated test) and RE-GATE the merged tree on a node before pushing `claude`.

## Repo map — where everything lives

`crates/app_ui/src/` (the app; main.rs ≈ 67.7k lines and shrinking):
- `main.rs` — ViewerApp struct (~330 fields), radar loading/loop engine, eframe update. ~63.6k lines after phase-1 decomposition. The map-paint block is now extracted (see `map_paint.rs`); `single_pane_canvas`/`grid_canvas` (input + paint dispatch) and the `handle_*_click` cluster stay here by design. Palette gate for model layers: `model_layer_solar_table` — precedence: user 🎨 override → Solar table → production style → viridis fallback. Ratchet ceiling ~63,821 — new feature code goes in modules, not here.
- Extracted modules (v0.30): `self_update.rs` (update checks; also the streaming-download + sha256 pattern), `hazard_geom.rs` (pure hazard geometry/parse/fill), `hazard_ui.rs` (hazard panels/paint), `product_select.rs` (product/cut selection), `settings_ui.rs` (settings panels), `sat_paint.rs` (sat window/worker pump/map layer/LUT cache), `geo_helpers.rs`, `map_paint.rs` (projection AEQD + GeoBounds, map chrome/labels/colorbar/mode-chip/cursor-inspector+loupe, site/intl/community markers, basemap+radar-raster+tiles layers).
- Satellite: `sat_window.rs` (geostationary navigation math — CGMS scan angles, GOES/Himawari sub-lons, native windows), `sat_worker.rs` (ingest workers, true-color composites, true-Kelvin IR + `ir_enhancement_anchors` BD/CIMSS/etc., sat store write contract `{token}_c{band:02}_{day}`).
- Tropical: `tropical.rs` (storm cards incl. 🛰 Vis/IR one-press sat views, JTWC vitals).
- Model/WRF: `model_data.rs` (model dock boundary; display-time label translation for raw `wrf_*` vars; `attach_solar_fallback_style` — the seam where style-less wrf fields get Solar compiled into `field.style`, WITH the friendly label as title because rw-ui prints `style.title` as plot title + viewer heading), `model_data/iso_fields.rs` (upper-air: synthesizes per-level picker entries 925–250 mb from `*_iso` volumes, background slicer), `model_layer.rs` (map layer), `local_import.rs` (LIGHT import: fast `read_var` 2-D planes + iso volumes; 4-dim vars skipped), `wrf_process.rs` (heavy full-diagnostics import), `wrf_volumes.rs` (native→isobaric interpolation, 37 levels, feeds skew-T), `wrf_radar.rs` (synthetic radar forward operator: virtual NEXRAD, 14-tilt ladder, 4/3-earth beam height/ground range per Doviak & Zrnić eq 2.28b/c, Stoelinga/Thompson reflectivity, Sun & Crook radial velocity; samples beam CENTERLINE — no beam broadening/attenuation/terrain blockage yet), `vol3d.rs` (wgpu volume raymarch + egui_wgpu callback pattern — the GPU precedent).
- Other: `layers_rail.rs`, `overlays.rs`, `dock.rs`, `guide.rs` (in-app manual + credits/sources), `brand.rs` (branding + canonical repo URL fallbacks), `italy_dpc.rs` (native Italy radar), `data_packs.rs` (radar scene packs).
- `crates/app_ui/tests/main_rs_line_ratchet.rs` — the ratchet. Lower the ceiling when you shrink main.rs.

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
- Open follow-ups usually live in the session task list; standing ideas: WRF description-attr fallback labels; radar forward-operator "next level" (Gaussian beam-broadening integration, attenuation, terrain blockage — wrf_radar.rs is the home, radar_core has the beam math); France radar (needs OAuth + BUFR — own project); KNMI 7-year archive (needs KNMI-native HDF5 decoder).

## OPEN HANDOFF (2026-07-09): WRF severe/thermo processing — UNFINISHED

Read this before touching WRF import. A prior Claude session (integration tip `6c00937`)
diagnosed and half-solved this but did NOT deliver what the owner actually needs, and did NOT
measure timing. Detailed notes: `C:\Users\drew\radar-work\wrf-severe-notes.md`.

**What the owner wants:** process 1-to-hundreds of raw wrfouts and get CAPE / STP / EHI etc. via
**wrf-rust (`wrf-core` getvar)**, as fast as wrf-rust allows. He explicitly does NOT want a
SHARPpy/sharprs re-implementation for raw wrfouts — he wants the wrf-rust values.

**The owner's actual files** (`C:\Users\drew\Downloads`, ~30+): mostly raw `wrfout_d03_*` at
**1.4–2.5 GB each** (high-res inner domains), plus `wrf3d_d01_2087-04-27` (2.37 GB, POST-PROCESSED
future: derived TK/Z/P, no raw T/PB) and a broken `.part`. These d03 grids are the SAME class the
hard rules call "minutes per file / ~8.85 GB RAM peak" (Enderlin 800×800×79). This is the crux the
prior session missed: "hundreds, fast" collides with the reality that `getvar` CAPE3D on a 2 GB
grid is inherently minutes and many GB of RAM.

**Two import paths** (know which the owner clicked):
- LIGHT — quick "📄 WRF/NetCDF file…" / "📥 folder" buttons → `local_import.rs::spawn_import_paths`.
  2-D surface fields + isobaric sounding volumes ONLY. No getvar, so NO CAPE/STP. This is what the
  owner used and why he saw ~120 vars and no severe.
- HEAVY wrf-rust — "🛠 …" buttons → `wrf_process.rs::spawn_process_paths`. `getvar` → full severe/
  thermo suite. `wrf_core::variables::VARS` includes `stp` (default set), sbcape/mlcape/mucape,
  srh1/srh3, bulk_shear, etc. `read_wrf_products` gates each getvar behind `should_process`
  (wrf_process.rs ~L649), so a narrowed selection genuinely skips getvars.

**What shipped at `6c00937` (integration, pushed `claude`):** a multi-select FILE picker for the
wrf-rust heavy import (it was folder-only) + a "🛠 …whole folder" button + relabeled quick buttons
("soundings only, no CAPE/STP"). `model_data.rs::import_pickers` + `gate_or_launch_heavy_import`
helper. Gated node3: fmt clean / clippy 0 / test 1938/0/56 (baseline — the UI is desktop-cfg'd, so
the change is NOT compiled on the Linux nodes; the Windows release build is its only compile check;
it built). Exe: `Desktop\BowEcho v0.30.5 dev (wrf-rust severe).exe`.

**What is NOT done / the real open problem:**
1. **Timing was never measured** on the owner's files. Get a real number: run the heavy wrf-rust
   import on ONE representative `wrfout_d03_*` (2 GB) and report per-file wall-clock + peak RAM,
   then extrapolate to the batch. Can't run the app on Windows (hook blocks cargo except the
   release build); options are a build-node run of the `RW_WRF_PROCESS_FIXTURE` ignored test with a
   real file copied over (mind the ~2 GB transfer and node RAM), or instrument the import.
2. **Fast-batch strategy for hundreds of 2 GB grids is unresolved.** wrf-rust getvar is minutes/file
   sequential (sequential is mandatory — hard rule 4, all-core memory load has crashed the owner's
   machine). Hundreds → hours. The fast alternative is a profile-based calc (`sharprs`, already used
   by `oa_derived.rs` for the SPC mesoanalysis; seconds/file, strided, memory-safe) — but the owner
   rejected sharprs for raw wrfouts. This is an OWNER DECISION: accept wrf-rust hours (add per-file
   progress + batch ETA + a durable queue), use a fast profile calc, or a hybrid. Do not just pick.
3. **Per-file elapsed + batch ETA readout** in the import UI is wanted for the queue-hundreds case.
4. **The post-processed `wrf3d_d01_2087` file** routes to `try_postprocessed_wrf_shared` → currently
   soundings only, NO severe (wrf-rust can't open it — no raw T/PB). A parked branch implements
   sharprs severe for exactly this case: `feat/wrf-postproc-severe` @ `3521674` (UNVERIFIED, not
   gated). Either finish/gate it or tell the owner that file is soundings-only.

**Other unmerged branches:** `feat/gdex-ui` @ `6ec07f5` (GDEX Stage 1b in-app CONUS-II catalog
browser, gated 1948/0/56 on node4 — ready to integrate); `feat/wrf-postproc-severe` @ `3521674`
(parked sharprs, see above).

## Report style (what the owner expects)

NO decorative emojis — not in chat, release notes, docs, or commit messages (strict house rule). Referencing literal in-app button labels that contain icons is fine; decorating prose is not.
Lead with the outcome. Exact numbers (line counts, test counts, sha). Say what you deliberately did NOT do and why. Never claim visual behavior you couldn't verify — describe what the code does and let the owner's RC test confirm. When you find something mid-task that contradicts the brief, say so and adapt — the code is the truth, docs get corrected.
