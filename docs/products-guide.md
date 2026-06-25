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
