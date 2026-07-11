# BowEcho Products Guide

This guide explains what the radar product buttons mean, what units the
inspector should report, and which products depend on full-volume data.

## Base and dual-pol moments

| Product | Reads | Units | Notes |
|---|---|---|---|
| **REF** Reflectivity | precipitation intensity | dBZ | Default reflectivity view. Analyst, NWS, GR2-style, and imported `.pal` color tables are supported. |
| **VEL** Velocity | radial wind toward/away from the radar | m/s or kt, depending on the selected table | Use **Unfold VEL** for dealiased display when a fold is suspected. |
| **SRV / DSRV** Storm-relative velocity | radial wind with storm motion removed | m/s or kt | Uses the storm-motion controls and storm-track handoff where available. |
| **SW** Spectrum width | velocity variance/turbulence | m/s or kt | Useful around shear zones, boundaries, and noisy velocity returns. |
| **ZDR** Differential reflectivity | drop shape and hydrometeor signal | dB | Useful for ZDR arcs, columns, calibration checks, and hail/rain discrimination. |
| **CC / RHO** Correlation coefficient | uniformity of scatterers | unitless | Low values in strong reflectivity can indicate debris, hail, mixed phase, or non-meteorological returns. |
| **PHI** Differential phase | along-beam phase shift | degrees | Raw phase context for dual-pol interpretation. |
| **KDP** Specific differential phase | phase shift rate | deg/km | Heavy rain and liquid-water signal; less sensitive to hail contamination than reflectivity. |

## Derived products

Derived products appear when their required source moments are available. Some
are per-tilt products; others use the whole volume and render on the lowest
usable reflectivity tilt.

| Product | Method | Units | How to read it |
|---|---|---|---|
| **CREF** Composite reflectivity | Column maximum reflectivity through the volume | dBZ | Best quick view of cores, including elevated cores that the lowest tilt can miss. |
| **ET** Echo tops | Highest sampled height meeting the reflectivity threshold | kft display labels | Storm-top height. The cone of silence can understate tops close to the radar. |
| **VIL** Vertically Integrated Liquid | Reflectivity-integrated liquid estimate through the column | kg/m^2 | Core intensity and heavy-precipitation loading. |
| **VILD** VIL Density | VIL divided by echo-top depth | g/m^3 | Large-hail screening; more selective than raw VIL. |
| **MEHS** Maximum Estimated Hail Size | Hail-size estimate from reflectivity aloft and environmental melting levels | mm or in | Potential hail size, not a ground truth report. |
| **POSH** Probability of Severe Hail | Severe-hail probability from SHI and wet-bulb height threshold | % | Probability-style hail guidance; values are clamped to 0-100%. |
| **POH** Probability of Hail | Hail probability from 45 dBZ echo-top height above the melting level | % | Any-hail probability, separate from severe-hail probability. |
| **MARC** Mid-Altitude Radial Convergence | Mid-level inbound/outbound velocity difference | m/s or kt | Downburst/wind-damage precursor guidance. Viewing angle matters. |
| **Gust** Estimated gust | Low-level velocity and storm/outflow context | m/s, kt, or mph by table/unit settings | Experimental near-surface gust guidance. Treat as situational context. |
| **AzShr** Azimuthal shear | Local low-level shear on velocity/dealiased velocity | 1/s scale | Rotation signal; pair with velocity, SRV, and environmental context. |
| **Div** Divergence | Local radial divergence/convergence | 1/s scale | Outflow, convergence, and boundary structure. |

### Advanced on-demand sweep products

The **Derive advanced** button in the PRODUCTS row computes additional
per-tilt products for the current visible tilt. KDP remains automatic; these
fields are intentionally on-demand so live loops do not precompute every
diagnostic for every incoming scan.

| Product | Method | Units | How to read it |
|---|---|---|---|
| **PHIF** Filtered differential phase | Smoothed/quality-controlled PHI field | deg | Inspect phase continuity before trusting KDP and attenuation products. |
| **KDP_SD** KDP uncertainty | Robust retrieval spread/confidence proxy | deg/km | Higher values mean the KDP estimate is less stable. |
| **AH** Specific attenuation | PHI/KDP-scaled horizontal attenuation | dB/km | Heavy-rain attenuation context along the beam. |
| **PIA** Path-integrated attenuation | Integrated horizontal attenuation | dB | Beam-path attenuation load; high values warn that far-range REF may be depressed. |
| **REFC** Corrected reflectivity | REF plus path attenuation correction | dBZ | Use as an aid in heavy rain, not as a replacement for multi-radar checks. |
| **ADP** Specific differential attenuation | PHI/KDP-scaled differential attenuation | dB/km | ZDR attenuation context. |
| **PIDA** Path-integrated differential attenuation | Integrated differential attenuation | dB | Beam-path ZDR correction context. |
| **ZDRC** Corrected differential reflectivity | ZDR plus differential attenuation correction | dB | Helps preserve ZDR arcs/columns in heavy rain when phase quality is good. |
| **RATE_Z** Z-R rain rate | Reflectivity-only rain-rate estimate | mm/h | Simple rate estimate; hail and attenuation can bias it. |
| **RATE_KDP** KDP rain rate | KDP-based rain-rate estimate | mm/h | Better in heavy rain and hail-contaminated cores when KDP is stable. |
| **RATE** Hybrid rain rate | Blended polarimetric rain-rate estimate | mm/h | Operator-friendly rain-rate field using the best available source. |
| **LWC** Liquid water content proxy | Reflectivity-derived water-content proxy | g/m^3 | Heavy precipitation/loading context. |
| **HKE** Hail kinetic-energy flux | Reflectivity-derived hail-energy proxy | J/(m^2 s) | Hail-core context; depends on sampling height and beam filling. |
| **CDR** Circular depolarization ratio | Dual-pol depolarization diagnostic | dB | Hydrometeor/non-meteorological quality context. |
| **L_RHO** Log correlation ratio | Log-scaled CC diagnostic | 1 | Expands low-CC structure for debris/hail/noise inspection. |
| **REF_TEX** Reflectivity texture | Local REF variability | dB | Sharp cores, clutter/noise, and mixed hydrometeor texture. |
| **VEL_TEX** Velocity texture | Local VEL variability | m/s | Shear/noise/turbulence context around couplets and boundaries. |
| **SW_TEX** Spectrum-width texture | Local SW variability | m/s | Turbulence/noise structure. |
| **ZDR_TEX** ZDR texture | Local ZDR variability | dB | Hail/mixed phase/noise context. |
| **RHO_TEX** CC texture | Local CC variability | 1 | Debris, hail, biological, and mixed-phase texture context. |
| **PHI_TEX** Differential-phase texture | Local PHI variability | deg | Phase quality and non-meteorological gate context. |
| **KDP_TEX** KDP texture | Local KDP variability | deg/km | KDP quality/noise context. |
| **REF_GRAD_R** Reflectivity range gradient | Along-radial REF gradient | dBZ/km | Sharp cores, boundaries, and attenuation gradients. |
| **VEL_GRAD_R** Velocity range gradient | Along-radial velocity gradient | 10^-3/s | Radial shear/convergence context. |
| **MET_QI** Meteorological quality | Quality score for meteorological gates | 1 | Higher confidence that gates are useful meteorological echoes. |
| **MET_MASK** Meteorological gate mask | Thresholded meteorological quality | 1 | Quick quality-control mask. |
| **TDS_SCORE** Tornadic debris diagnostic | REF/CC/ZDR style debris confidence score | % | Triage only; verify with raw REF, VEL/SRV, CC, ZDR, and scan height. |
| **HAIL_SCORE** Dual-pol hail diagnostic | REF/ZDR/CC style hail confidence score | % | Hail triage; verify with raw products and environmental context. |
| **TURB** Doppler turbulence proxy | Velocity/SW texture-style turbulence proxy | m/s | Turbulence and shear context; pair with SW and VEL texture. |

## Velocity dealiasing

BowEcho includes velocity unfolding modes for folded velocity data. The
inspector warns when a sampled gate is near Nyquist and may still be folded.
Use dealiased velocity as analysis aid rather than as proof that every gate is
meteorologically correct.

## Vertical wind profile (experimental)

The Vertical Wind Profile (VWP) estimates the horizontal wind by fitting the
azimuthal pattern of **dealiased radial velocity** in the currently loaded PPI
volume. It is a derived analysis, not a directly measured wind at a point.
The panel shows the wind profile and hodograph together with RMS fit error,
sample count, azimuth coverage, selected range/elevation, and an explicit
quality result for each height. Missing or rejected levels are expected when
the scan lacks sufficient velocity, height coverage, or viewing angles.

Treat VWP as experimental guidance. Dealiasing errors, sparse azimuth coverage,
range folding, non-meteorological echoes, and an uneven precipitation field can
bias the fit. Compare the result with the velocity display, a nearby sounding,
and other observations before using it operationally.

On Windows, macOS, and Linux, the VWP panel can also import a local NEXRAD
Level III Product 48 file. Imported Product 48 profiles use that product's tabular data
when present and fall back to its symbology wind barbs; they remain distinct
from a profile computed from the current Level II volume. An imported profile
stays selected across dealias-engine or environmental-anchor updates; changing
the primary radar volume or choosing **Recompute** returns to the computed VWP.
CSV export retains available tabular divergence, slant range, elevation angle,
and adaptable threshold metadata. BowEcho does not automatically download
Product 48 files.

Computed VWP requires a loaded radar volume with radial velocity. A
two-dimensional composite or CMAX layer — including the IMGW POLRAD layers
below — has no radials or elevation scans and therefore cannot produce a VWP.

## Poland IMGW-PIB POLRAD CMAX layers

BowEcho can display the ODIM HDF5 dual-polarization CMAX grids published through
the IMGW-PIB national datastore. Supported quantities are **KDP** (deg/km),
**RHOHV** (unitless), **ZDR** (dB), and **PHIDP** (degrees). Product availability
varies by radar and cycle. The menu uses this static fallback catalog snapshot,
verified **2026-07-10**; it does not probe every site while the menu is open:

| POLRAD sites | KDP | RHOHV | ZDR | PHIDP |
| --- | --- | --- | --- | --- |
| All ten sites below | published | published | published | Ramża only |

Every selection still checks the live datastore before downloading and reports
an unavailable product honestly if IMGW changes a site's publication set.

Supported POLRAD sites are Brzuchania, Nowy Gdańsk, Góra Św. Anny, Legionowo,
Pastewnik, Poznań, Ramża, Rzeszów, Świdwin, and Użranki.

These products are site-centered, top-down **two-dimensional maximum
projections**. They are useful as regional dual-pol layers, but they are not
polar sweeps or full volumes: individual tilts, radial geometry, beam heights,
and three-dimensional storm structure cannot be recovered from them. Do not
interpret a CMAX pixel as a value from a specific elevation, and do not compare
it one-for-one with a selected base tilt.

Source: [IMGW-PIB public datastore](https://danepubliczne.imgw.pl/pl/datastore).
[IMGW-PIB reuse terms](https://danepubliczne.imgw.pl/pl/introduction) require
the source notice “Źródłem pochodzenia danych jest Instytut Meteorologii i
Gospodarki Wodnej – Państwowy Instytut Badawczy” and, for processed
IMGW-derived output, “Dane Instytutu Meteorologii i Gospodarki Wodnej –
Państwowego Instytutu Badawczego zostały przetworzone”.

## Formula Lab model diagnostics

Formula Lab is available from the Model window. It compiles custom diagnostic
expressions through Rusty Weather's bounded formula engine; expressions are
data formulas, not arbitrary Rust, Python, or shell code. Evaluation runs in a
background worker and keeps progressing if the Model window is closed.

The **Store** source uses the model/run/hour selected in BowEcho's model
browser. It is model-neutral: WRF, HRRR, and other stored models work whenever
the requested variables and dimensions exist. The **Raw WRF** source exposes
WRF grid metrics, height, vector, vertical, and horizontal-calculus operations.
Its file picker is intentionally unrestricted because ordinary `wrfout_*`
files are often extensionless; it does not assume `d01` or any other domain.

Temporal operators such as `dt(...)` are enabled only when BowEcho verifies a
complete exact-time axis for the selected run and every lead time is exactly
representable by the formula bridge. A stale, legacy, partial, or ambiguous
axis disables temporal evaluation while leaving pointwise formulas available.
This prevents ordinal storage slots from being mistaken for meteorological
time.

A completed formula becomes an in-memory field in the existing model field
viewer. BowEcho generates a color scale over the result's full finite range,
unless an exact saved output-name binding supplies a user color table. The
styled field can be sent to the radar map and native plot/PNG workflow just
like a stored model field. Changing source data while evaluation is running
causes the stale result to be discarded instead of displayed.

## EUMETNET ORD full-day archive loads

For an ORD radar, open **Data > Archive**, enter a UTC date, and choose
**Load full day**. BowEcho lists the day in 24 bounded one-hour catalog phases,
merges the ORD PVOL/SCAN lanes, sorts and identity-deduplicates the scans, then
downloads them with progress grouped into 20-scan phases. Decoded frames stream
into the loop immediately; the newest successful scan is selected at the end.

The MeteoGate API's commonly observed count of 20 refers to elevation
`Coverage` objects, not to a limit of 20 radar scans. A daily listing can
therefore contain hundreds of scans. BowEcho tags cached listings with their
exact UTC date so editing the date cannot accidentally load a previously
listed day.

Both listing and download expose cancellation. A failed volume gets one retry;
if any catalog phase or scan remains unavailable, BowEcho reports the day as
incomplete and keeps the successfully decoded frames instead of claiming a
complete load.

## Cross-sections and panes

Vertical cross-sections are interactive in the app. Draw a section across a
storm to inspect height-vs-distance structure for vaults, overhangs, descending
cores, rear-inflow jets, and velocity structure. Cross-sections use the same
4/3-Earth beam geometry as the map inspector.

BowEcho supports single, dual, triple, and quad pane layouts. Panes can share
map/time context while keeping independent product choices, which is useful for
reflectivity plus velocity, dual-pol, or multi-radar comparison workflows.

## Layers and overlays

Common overlay workflows include warnings, SPC outlooks and reports, mPING and
surface observations, GLM lightning, model fields, satellite basemaps, range
rings, radar-site labels, and GRLevelX-style placefiles. Overlay visibility and
style controls live in the sidebar/settings surfaces rather than in product
math itself.

## Verification

- Gallery rendering:
  `cargo run --release -p render2d --example product_gallery -- <level2-file> <out-dir>`
- App tests:
  `cargo test -p app_ui`
- Lints:
  `cargo clippy -p app_ui --all-targets -- -D warnings`

The products are intended for radar analysis and documentation, not as official
warning products. Confirm life-safety decisions with official National Weather
Service warnings and local emergency guidance.

## Current limits and next polish

- Product metadata is still partly UI-driven. The long-term target is a central
  product registry that owns labels, units, palettes, dependencies, cache
  policy, and inspector behavior.
- Some international feeds expose different products, ranges, or scan heights
  for reflectivity and velocity. BowEcho should show what is available rather
  than pretending every radar behaves like NEXRAD.
- Derived severe-weather products are guidance. They depend on volume coverage,
  velocity quality, environmental assumptions, and viewing geometry.
- More golden render tests and curated screenshots/GIFs would make future UI
  and algorithm changes easier to trust.
