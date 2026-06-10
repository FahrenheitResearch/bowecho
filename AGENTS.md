# AGENTS.md — BowEcho

Orientation for AI coding agents working in this repository. The
human-facing overview is [README.md](README.md); product/algorithm docs
live under [docs/](docs/).

## What this repo is

BowEcho is a fast, native NEXRAD Level II radar viewer in Rust (egui/eframe
GUI). It decodes raw Level II data from the public AWS archive and live
chunk feed and renders it with a CPU rasterizer.

## Workspace map

| Crate | Purpose |
|---|---|
| `crates/app_ui` | The application (binary `bowecho`): egui UI, render worker, panes — one large `main.rs` |
| `crates/radar_core` | Core data model: `RadarVolume`, `ElevationCut`, `MomentGrid`, beam geometry |
| `crates/nexrad_io` | NEXRAD Archive II / Level II decoder (Message Type 31) |
| `crates/render2d` | CPU rasterizer + product algorithms (moments, derived products, detection, cross-sections) |
| `crates/dealias` | **Standalone, zero-dependency velocity dealiasing** (`bowecho-dealias` on crates.io); `render2d` consumes it through thin grid adapters. See [crates/dealias/AGENTS.md](crates/dealias/AGENTS.md) |
| `crates/data_source` | Radar data-source helpers (archive/live feeds) |
| `crates/cache` | Cache/retention policy for downloaded files and decoded volumes |
| `crates/color_tables` | Color table parsing and sampling (incl. `.pal` import) |
| `crates/product_engine` | Base/derived product registry scaffolding |
| `crates/settings` | Persisted app settings (JSON in platform config dir) |
| `crates/timeline` | Timeline/animation state for live & archive playback |

## Build, test, gates

CI (`.github/workflows/ci.yml`, Windows runner, stable toolchain) enforces
exactly these — run all three before declaring work done:

```sh
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Run the app: `cargo run --release -p app_ui --bin bowecho`.

## Conventions

- Rust edition 2024, stable toolchain. `panic = "abort"` is intentionally
  NOT set (worker panics are isolated with `catch_unwind`) — don't add it.
- Algorithms with a research basis carry citations (author, year, venue/DOI)
  in code comments and module docs — keep that up when adding or modifying
  methods (see `render2d` module headers and `crates/dealias` for the
  pattern).
- Changes to radar science (dealiasing, detection, derived products) should
  be validated on real captured volumes, not only unit tests.
  `docs/dealias-fold-branch-analysis.md` documents the dealiasing
  validation set and a worked failure analysis; `render2d/examples/`
  contains the probe/diagnostic binaries (`velocity_diag`,
  `velocity_point_probe`, `dealias_blob_probe`, `product_gallery`, …) used
  for that purpose.
- Examples in `render2d/examples/` are part of `--all-targets`; keep them
  compiling and clippy-clean.

## Safety rails

- Don't publish to crates.io or create GitHub releases unless explicitly
  asked; both are effectively irreversible.
- The renderer and dealiaser are deterministic by design and covered by
  determinism tests — avoid introducing iteration-order or
  time/randomness dependence into product algorithms.
