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

## OpenJPEG / openjp2 Rust port

- **License:** BSD-2-Clause
- **Source:** <https://github.com/Neopallium/openjp2>

Rusty Weather uses a patched copy of the `openjp2` Rust port to decode
JPEG2000-packed GRIB2 fields. The patch adds memory-safety guards without
changing the following upstream notice:

Copyright (c) 2002-2014, Universite catholique de Louvain (UCL), Belgium
Copyright (c) 2002-2014, Professor Benoit Macq
Copyright (c) 2003-2014, Antonin Descampe
Copyright (c) 2003-2009, Francois-Olivier Devaux
Copyright (c) 2005, Herve Drolon, FreeImage Team
Copyright (c) 2002-2003, Yannick Verschueren
Copyright (c) 2001-2003, David Janssens
Copyright (c) 2011-2012, Centre National d'Etudes Spatiales (CNES), France
Copyright (c) 2012, CS Systemes d'Information, France

All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS “AS IS”
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
(INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

## Space Grotesk font

- **License:** SIL Open Font License 1.1
- **Copyright:** © 2020 Florian Karsten (The Space Grotesk Project Authors,
  <https://github.com/floriankarsten/space-grotesk>)

Space Grotesk Regular and Bold are embedded in the `sharppyrs` crate (and thus
in BowEcho binaries) to match the original renderer's face. The full OFL text
ships alongside the fonts at `sharppyrs/assets/fonts/OFL.txt`.

## Source Sans 3 font

- **License:** SIL Open Font License 1.1
- **Copyright:** Copyright 2010-2024 Adobe, with Reserved Font Name "Source"
- **Source:** <https://github.com/adobe-fonts/source-sans>

Source Sans 3 Regular is embedded in the vendored `sharprs` classic sounding
renderer. The copy below is included here because this top-level notice ships
with BowEcho's binary release packages.

SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007

PREAMBLE

The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS

"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS

Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION

This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER

THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.

## Natural Earth

- **License:** Public domain
- **Source:** <https://www.naturalearthdata.com/>

The sounding window's locator basemap lines (CONUS state boundaries and
coastline) are simplified from Natural Earth 1:50m vector data, as bundled by
SHARPpy-Reimagined. BowEcho's offline application basemap also contains
generated geometry and labels from Natural Earth 1:50m country boundaries,
1:10m regional administrative boundaries, and 1:10m populated places.

## U.S. Census Bureau cartographic boundary data

- **Source:** U.S. Census Bureau cartographic boundary files
- **Embedded inputs:** `cb_2024_us_state_500k`, `cb_2024_us_county_500k`, and
  `cb_2023_us_place_500k`

BowEcho's offline application basemap contains generated U.S. state/county
geometry and town/place labels derived from these files. The year and dataset
identifiers above are retained from the checked-in
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

## weather-contours native engine

- **License:** MIT
- **Copyright:** Copyright (c) 2026 Andrew Snyder / Fahrenheit Research
- **Vendored source:** `vendor/weather-contours`
- **Pristine source archive:**
  `vendor/weather-contours/upstream/autumnplot-weather-contours-sota.zip`
- **Source archive SHA-256:**
  `E9B7F22646BCB2C4938A10B1CE98D33705C41BFB75A134DF28C4E8CE9730E453`

BowEcho includes a modified, native-only copy of the dependency-free OIRT
connected-isoline and COBRM indexed-isoband engine. BowEcho's copy adds stable
degenerate-saddle behavior, exact zero-length path filtering, checked resource
budgets, fallible large allocations, and additional validation and tests.

MIT License

Copyright (c) 2026 Andrew Snyder / Fahrenheit Research

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
