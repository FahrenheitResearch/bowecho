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
SHARPpy-Reimagined.

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
