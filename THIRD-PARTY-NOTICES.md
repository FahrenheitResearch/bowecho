# Third-party notices

BowEcho itself is dual-licensed under MIT or Apache-2.0 (see the README).
This file collects license and attribution notices for third-party work that
BowEcho derives from, embeds, or offers as a separately downloaded research
data pack. Data sources and contributed graphics work are credited in the
[README's Credits section](README.md#credits). Full license texts live in the
linked repositories and bundled asset files noted below.

## SHARPpy

- **License:** BSD-3-Clause
- **Copyright:** © SHARPpy contributors (Patrick T. Marsh & John Hart; MetPy
  Developers; Kelton Halbert, Greg Blumberg & Tim Supinie)
- **Source:** <https://github.com/sharppy/SHARPpy>

The SPC sounding window BowEcho renders (skew-T, hodograph, insets, index
board) and its `sharptab` analysis algorithms are a derivative port of
SHARPpy's rendering and numerics. Please cite the SHARPpy paper when citing
this functionality:

> Blumberg, W. G., K. T. Halbert, T. A. Supinie, P. T. Marsh, R. L. Thompson,
> and J. A. Hart, 2017: SHARPpy: An open-source sounding analysis toolkit for
> the atmospheric sciences. *Bull. Amer. Meteor. Soc.*, **98**, 1625–1636,
> <https://doi.org/10.1175/BAMS-D-15-00309.1>.

## SHARPpy-Reimagined-vRust

- **License:** BSD-3-Clause (retaining the upstream SHARPpy notices)
- **Copyright:** © SHARPpy Reimagined maintainers (FahrenheitResearch)
- **Source:** <https://github.com/FahrenheitResearch/SHARPpy-Reimagined-vRust>

The `sharppyrs` crate that draws BowEcho's sounding window is a Rust/egui
port of SHARPpy-Reimagined's modified SPC widget (layout, zoomed hodograph,
0–500 m trace band, locator inset, index board).

## sharprs / rusty-weather

- **License:** BSD-3-Clause / MIT
- **Copyright:** © FahrenheitResearch
- **Sources:** <https://github.com/FahrenheitResearch/sharprs>,
  <https://github.com/FahrenheitResearch/rusty-weather>

`sharprs` is the pure-Rust SHARPpy calculation engine behind the sounding
analysis (parcels, effective inflow, composite indices); the rusty-weather
crates (`rw-ui`, `rustwx-*`) provide the model-data dock, classic sounding
panel, and satellite/model stack. See each repository for its authoritative
license text.

## Space Grotesk font

- **License:** SIL Open Font License 1.1
- **Copyright:** © 2020 Florian Karsten (The Space Grotesk Project Authors,
  <https://github.com/floriankarsten/space-grotesk>)

Space Grotesk Regular and Bold are embedded in the `sharppyrs` crate (and thus
in BowEcho binaries) to match the original renderer's face. The full OFL text
ships alongside the fonts at `sharppyrs/assets/fonts/OFL.txt`.

## Natural Earth

- **License:** Public domain
- **Source:** <https://www.naturalearthdata.com/>

The sounding window's locator basemap lines (CONUS state boundaries and
coastline) are simplified from Natural Earth 1:50m vector data, as bundled by
SHARPpy-Reimagined. BowEcho's offline application basemap and the embedded
ArWen Studio map also contain generated geometry and labels from Natural Earth
1:50m country boundaries, 1:10m regional administrative boundaries, and 1:10m
populated places.

## U.S. Census Bureau cartographic boundary data

- **Source:** U.S. Census Bureau cartographic boundary files
- **Embedded inputs:** `cb_2024_us_state_500k`, `cb_2024_us_county_500k`, and
  `cb_2023_us_place_500k`

BowEcho's offline application basemap and the embedded ArWen Studio map contain
generated U.S. state/county geometry and town/place labels derived from these
files. The year and dataset identifiers above are retained from the checked-in
generators and generated-source provenance comments.

## Py-ART region-based velocity dealiasing

- **License:** BSD-3-Clause, with the U.S. DOE / Argonne government-rights
  notice
- **Copyright:** Copyright (c) 2013, UChicago Argonne, LLC. All rights reserved.
- **Sources:** <https://github.com/ARM-DOE/pyart>,
  <https://github.com/FahrenheitResearch/region-global-dealias>

BowEcho's Region Global velocity solver uses the pinned
`region-global-dealias` Rust port of Py-ART's `dealias_region_based` per-sweep
core. It is modified software, not the Py-ART distribution from Argonne: the
standalone solver works on flat `f32` arrays and replaces the original region
labeling and network-reduction data structures with fold-identical optimized
implementations. The complete upstream notice is reproduced in
[`PYART-LICENSE.txt`](PYART-LICENSE.txt).

The separately selectable RIFT engine uses the same pinned crate's v0.2.0
opt-in gate-resolution refinement. It starts from the unchanged Region Global
result and only accepts bounded local corrections when its independent trigger
and wrapped-vortex fit agree; selecting Region Global does not enable RIFT.

Please cite Helmus, J. J., and S. M. Collis, 2016: The Python ARM Radar Toolkit
(Py-ART), a Library for Working with Radar Data in the Python Programming
Language. *J. Open Res. Softw.*, **4**(1), e25,
<https://doi.org/10.5334/jors.119>.

## ArWen Studio control plane

- **License:** Apache-2.0
- **Snapshot:** `fdee27fe5bd1eae6601e1d3342bde1544c15db36` (2026-08-08)

BowEcho vendors the ArWen Studio Rust planner, `gpuwm run-plan` contract,
process supervisor, Lambert-domain geometry, and contract fixtures. The
ArWen-authored Rust sources retain their Apache-2.0 SPDX headers and are not
relicensed under the MIT alternative offered for BowEcho-owned code. The two
generated basemap tables instead retain their Census/Natural Earth provenance
comments and are covered by the data notices above. See
[`docs/arwen-studio/VENDOR.md`](docs/arwen-studio/VENDOR.md) for scope and
integration invariants.

## PyTMatrix-derived property T-matrix research pack

- **License:** MIT
- **Copyright:** Copyright (c) 2013-2023
- **Author:** Jussi Leinonen
- **Source:** <https://github.com/jleinonen/pytmatrix>

BowEcho can optionally download a separately versioned, PyTMatrix 0.3.3-derived
S-band lookup-table pack when the property T-matrix simulated-radar mode is
first used. The exact upstream MIT notice is included inside that downloaded
pack as `PYTMATRIX-LICENSE.txt`. These derived lookup tables are a research
product, have not been independently validated by BowEcho, and must not be
treated as an operational radar calibration.
