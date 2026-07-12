# WRF simulated radar

BowEcho samples a WRF atmosphere onto a native polar radar volume. Every
elevation, radial, and gate enters the ordinary radar viewer, so loops,
cross-sections, readouts, derived products, velocity dealiasing, and CfRadial
export use the same path as observed radar.

Open **Windows > WRF**, then use the **WRF simulated radar** card. This path
does not import fields into the model store. If the desired output is a
surface field, sounding, or custom gridded diagnostic, use **Open WRF /
NetCDF**, **WRF full diagnostics**, or **Formula Lab** instead.

## Quick workflow

1. Choose a complete **What do you want?** recipe. **Real radar (balanced)**
   is the recommended first choice for a practical virtual S-band scan.
2. Open **Radar location & fine tuning (advanced)** only when the recipe's
   antenna, geometry, moments, presentation, or instrument assumptions need
   changing. Atmosphere controls are under **Instrument & propagation >
   Atmosphere time**.
3. Choose the custom ladder or a source-qualified **Build 24 VCP**. Choose
   whether timed rays hold one WRF scene or interpolate a compatible adjacent
   scene.
4. Choose **Build from files...** to multi-select WRF files, or **Build from
   folder...**. Every selected WRF time becomes one radar-loop frame.
5. Inspect the generated loop with the ordinary product, tilt, cross-section,
   readout, derived-product, and velocity workflows.
6. Change controls and choose **Refresh current frame(s)** to rerun the same
   source snapshot without another picker.
7. Use **Export latest as CfRadial...** when a portable radar volume with
   model/operator provenance is needed.

If an observed radar volume is already displayed, **Replay displayed observed
scan...** is the validation workflow. Choose one raw WRF file and BowEcho will
reuse the observation's actual cuts, individual ray azimuths/elevations,
acquisition times, gate geometry, split cuts, missing sectors, moment
availability, Nyquist, and PRT. The result opens as linked **Observed / Simulated
/ Difference** panes. This is exact acquisition replay, not a reconstructed base
VCP.

Extensionless **wrfout_*** names from every domain are valid. A folder build
captures the files found at selection time. Refresh deliberately reuses that
ordered session snapshot; it does not rescan the folder or silently add files
created later.

One simulated-radar loop must be exactly one compatible WRF run, domain, and
grid. BowEcho accepts d01, d02, and other domains individually; it rejects a
mixed d01/d02 selection, remeshed grids, duplicate/restart valid times, or
untimed scenes rather than silently combining them. Multi-time files are
inventoried by internal time index.

## Related WRF import and processing controls

The lighter **Open file...** action imports one WRF/NetCDF file; use **Open
folder...** for a light batch. **Process files...** under WRF full diagnostics
and **Build from files...** under WRF simulated radar are the multi-select
actions for one to hundreds of files. Their folder counterparts scan and sort
supported files before starting.

**Automatically plot new imports** is on by default and persisted. It can write
every field and hour from light, full-diagnostic, and GDEX imports to the
screenshots folder. It is separate from simulated radar, which enters the radar
viewer instead of the model store.

## Operating modes

- **Truth** keeps one model instant, center sampling, unfolded air motion, and
  no virtual-instrument effects. It is intended for model diagnosis.
- **Instrument** applies the balanced pulse-volume rule, hydrometeor fall
  speed, terrain blockage, spectrum width, S-band dual polarization,
  propagation, timed rays, velocity folding, and range-dependent sensitivity.
- **Presentation** keeps the fast center rule and familiar deterministic gate
  texture. It is intended for legible model loops, not quantitative instrument
  comparison.

Choosing a mode applies a preset once. Every control remains independently
editable afterward, and the complete data-changing configuration participates
in the loop fingerprint and export provenance.

## User recipes

BowEcho exposes the operator through **Windows > WRF**, separate from the
Models run library and plotter. The `What do you want?` selector applies a
complete compatible configuration while preserving the chosen radar location,
range, and gate geometry:

- **Storm view (fast)** produces readable textured REF/VEL loops without
  virtual-instrument effects.
- **Clean model truth** removes texture, noise, folding, blockage, and scan
  timing for model diagnosis.
- **Clean dual-pol** enables the polarimetric operator and propagation without
  noise, folding, or terrain blockage.
- **Real radar (balanced)** is the recommended practical S-band simulation.
- **Maximum fidelity (slow)** uses the 27-point pulse-volume rule for one file
  or a short loop.
- **P3/ISHMAEL T-matrix (research)** selects the exact 2.8 GHz,
  property-aware, fail-closed research operator without changing the chosen
  antenna or scan geometry.

Changing an advanced control after applying a recipe labels the setup as
`Custom tuning`. Selecting a recipe again resets all interacting physics,
calibration, and instrument values together, so stale expert values cannot
leak into a new run.

The recipes also choose coherent temporal defaults. **Storm view**, **Clean
model truth**, and **Clean dual-pol** start Frozen. **Real radar** and
**Maximum fidelity** start with Timed volume, fast derived/additive adjacent
sampling, and Hold last. **P3/ISHMAEL T-matrix** starts with the slower
Raw-state pre-closure reference and Hold last. These remain editable after
applying the recipe.

### Recipe comparison

| Recipe | Best use | Pulse volume | Main output character |
|---|---|---:|---|
| Storm view (fast) | Fast loop browsing | Center | Textured REF, clean unfolded VEL |
| Clean model truth | Model diagnosis | Center | No presentation/instrument effects |
| Clean dual-pol | Microphysics comparison | Balanced (9) | Polarimetry/propagation without noise, folds, or blockage |
| Real radar (balanced) | Practical virtual radar | Balanced (9) | Full S-band instrument path |
| Maximum fidelity (slow) | One frame or short loop | Reference (27) | Full deterministic 3 x 3 x 3 integration |
| P3/ISHMAEL T-matrix (research) | Supported P3/ISHMAEL experiments | Balanced (9) | Research-only non-Rayleigh dual-pol; no fallback |

Recipes preserve antenna placement, maximum range, and gate geometry. They
reset the interacting physics and calibration controls; this is why selecting
a new recipe is safer than toggling one checkbox on top of an unrelated old
configuration.

## Build and refresh lifecycle

The most recent non-empty file selection is retained for the current BowEcho
session. **Refresh current frame(s)** validates the controls again, launches
the same worker path as a new build, and replaces the simulated-radar loop when
the worker completes. It never opens a file dialog.

The snapshot is retained after a failed run so a user can correct controls and
retry. A file that was deleted, replaced, or made unreadable still produces an
explicit worker error; retaining a path is not a promise that its contents
remain unchanged.

The complete data-changing configuration participates in frame identity and
export provenance. Presentation-only viewer settings do not silently alter the
forward operator.

An exact replay retains both the chosen WRF file and observed-volume snapshot
for the session, so **Refresh current frame(s)** reruns the same validation
pair after tuning. A normal simulated-radar build clears replay mode. The
comparison workspace is transient and does not overwrite the user's saved pane
layout or independent-panel settings.

## Exact observed-scan replay and differences

Replay fails closed unless the displayed source has valid site coordinates,
cuts, rays, and gate geometry. It also rejects BowEcho synthetic/difference
volumes as observation templates. Geometry is copied from the decoded source;
BowEcho does not fill missing sectors, merge duplicate-elevation split cuts, or
invent moment coverage.

Observed radial acquisition time drives the atmosphere-time weight for every
replayed ray. Terrain-horizon sampling uses that ray's real azimuth. The
simulated output retains the three gate-quality products described below, and
the difference builder compares only moments available on the exact same
cut/radial/gate geometry. Missing observed moments are listed explicitly
instead of becoming silent all-NaN products.

The comparison workspace starts with REF / REF / DIF_REF and supports the
other exact-overlap difference products through the ordinary product picker.
Difference phase uses circular angular subtraction. Polling and live refresh
are disabled while the local three-volume comparison owns the panes.

## Forward operator

Reflectivity is interpolated and integrated in linear equivalent reflectivity
factor, `Z = 10^(dBZ/10)`, before conversion back to dBZ. The legacy direct-dBZ
interpolator remains selectable for reproducing older renders.

The antenna/pulse rule can use one center sample, a symmetric nine-point rule,
or a 3 x 3 x 3 reference rule. The operator accumulates linear Z,
Z-weighted radial velocity, velocity variance, polarimetric covariance, and
the fraction occulted by the terrain horizon. Radial velocity can represent
the scattering particles rather than air alone by adding species-weighted
terminal fall speed. Spectrum width combines pulse-volume velocity variance,
terminal-speed diversity, optional WRF TKE/QKE, and a configured instrument
floor.

Terrain blockage uses a cumulative apparent horizon for every azimuth and
range under the same 4/3-Earth geometry as the beam. Blocked quadrature weights
remove received power instead of being renormalized, which gives partial beam
blockage in the multi-sample rules.

Timed volumes stamp a monotonic acquisition offset on every ray. The custom
ladder uses its configured rotation rate and inter-sweep transition. A named
Build 24 VCP instead uses each physical source row's azimuth rate and period.
Atmosphere-time sampling is a separate choice described below.

## Named Build 24 VCP base patterns

The scan selector includes the custom legacy ladder and source-qualified base
patterns for **VCP 12, 34, 35, 112, 212, and 215**. BowEcho transcribes the
WSR-88D Radar Operations Center's Build 24.0 interface control document
2620002AA, revision AA, Appendix C. Across the six definitions, all **94
physical rows** remain in source order:

| VCP | Regime | Physical rows | Approximate base cadence |
|---:|---|---:|---:|
| 12 | Precipitation, short pulse | 17 | 236.23 s |
| 34 | Clear air, long pulse | 10 | 524.52 s |
| 35 | Clear air, short pulse | 12 | 403.49 s |
| 112 | Precipitation, short pulse | 20 | 321.37 s |
| 212 | Precipitation, short pulse | 17 | 258.38 s |
| 215 | Precipitation, short pulse | 18 | 340.37 s |

Equal-elevation rows are not collapsed. That preserves surveillance/Doppler
split cuts and the two fixed MPDA Doppler cuts in VCP 112. Each row retains its
elevation, azimuth rate, source period, waveform, moment coverage, surveillance
PRF code and pulse count, and Doppler PRF policy. Appendix C gives numbered
PRF **codes**, not frequencies, so BowEcho does not turn those codes into fake
PRF-Hz, PRT, or unambiguous-range metadata.

These are versioned **base-pattern simulations**, not claims that BowEcho has
reproduced an actual site's live operational volume. SAILS, MRLE, AVSET,
Add-MPDA, and site-specific low-tilt adaptations are outside this catalog.
The source document, revision, RDA build, figure, physical-row metadata, and
that adaptations caveat are retained in volume/CfRadial provenance. The source
of record is the [ROC Build 24 interface control document](https://www.roc.noaa.gov/public-documents/icds/2620002AA.pdf).

## Atmosphere time sampling

**Frozen at volume start** remains available and makes every ray sample the
anchor WRF scene, even when the ray carries an acquisition time. Both adjacent
options require the next chronologically later scene in the same compatible
run/domain/grid group. For each ray, BowEcho derives one weight from its
acquisition offset within the model-time bracket. The weight must remain
between the two WRF times; the renderer never extrapolates.

This interpolation changes the atmosphere sampled *inside one output radar
volume*: low-level/early rays stay nearer the anchor scene and later rays or
cuts blend slightly farther toward the next scene. It does not create extra
loop frames, add rapid low-level-update frames, run WRF forward, or update in
real time. Selecting it automatically enables **Timed volume**, because an
instantaneous scan has no per-ray acquisition offsets to interpolate.

With N compatible WRF scenes, BowEcho normally presents N radar-loop frames.
Frames 1 through N-1 can use the following scene as their temporal bracket. The
last frame has no later scene and therefore follows **Hold last**, **Drop**, or
**Error**; Drop can make the output loop shorter than N frames. A scan that
extends beyond its next scene follows the same explicit policy.

**Linear adjacent (derived/additive)** interpolates linear received power
(`Z`), winds, and additive polarimetric scattering quantities. ZDR and rhoHV
are derived afterward rather than interpolated as ratios. This is the fast,
well-behaved compatibility path: the property-aware kernel closes each source
cell first and blends its additive scattering afterward.

**Raw-state pre-closure** is the slow research reference for the P3/ISHMAEL
property T-matrix kernel. At every pulse-volume quadrature point it forms the
actual up-to-eight trilinear model-cell weights in each scene, multiplies those
groups by `(1-alpha)` and `alpha`, and blends the resulting up-to-sixteen raw
property contributors. Winds, TKE, temperature, pressure, and density use the
same spatial/time weights. BowEcho then performs exactly one nonlinear
microphysical closure and one validated T-matrix/PSD evaluation for that
intermediate state. Endpoints are exact, and scheme, property-inventory,
category-layout, rain-availability, spatial-coverage, or table mismatches stop
the run rather than falling back to additive-time interpolation.

Raw-state and additive-scattering interpolation are intentionally not expected
to agree in rapidly growing, melting, or riming regions. Comparing them is the
purpose of the reference mode; neither creates extra forecast frames or claims
to integrate WRF forward in time.

If there is no complete later scene, or the scan would cross beyond it, the
explicit policy can **hold the anchor**, **drop the frame**, or **stop with an
error**. A held frame records why it was held; it is not labeled interpolated.
The worker keeps at most a rolling two-scene input cache and preflights the
configured memory ceiling before reading the second scene. Provenance records
the scene identities, bracket, interpolation space, outcome, and policy.

## Default S-band dual polarization

The default dual-pol operator is a scheme-aware bulk S-band Rayleigh model. It
reads WRF mass mixing ratios and available number concentrations, closes a
gamma or fixed-intercept particle-size distribution per species, and sums the
species in additive linear scattering space. Rain shape follows the Brandes
axis-ratio relation. The design follows the bulk forward-operator lineage of
Jung et al. (2008, 2010).

It emits:

- ZH/REF, ZDR, rhoHV, KDP, and unwrapped PhiDP
- Doppler velocity and spectrum width
- specific attenuation AH and ADP
- integrated PIA and PIDA
- intrinsic/corrected REFC and ZDRC

Propagation proceeds from near to far along each radial:

- `PhiDP = system_phase + 2 * integral(KDP ds)`
- `PIA = 2 * integral(AH ds)`
- `PIDA = 2 * integral(ADP ds)`
- observed `REF = REFC - PIA`
- observed `ZDR = ZDRC + bias - PIDA`

## Moment glossary

| BowEcho product | Meaning |
|---|---|
| REF | Observed horizontal reflectivity after modeled propagation loss |
| REFC | Intrinsic/corrected horizontal reflectivity before modeled loss |
| VEL | Scatterer-weighted radial Doppler velocity; optionally Nyquist-folded |
| SW | Spectrum width from pulse-volume variance, fall-speed diversity, optional model TKE/QKE, and instrument floor |
| ZDR | Observed differential reflectivity, including bias and differential attenuation |
| ZDRC | Intrinsic/corrected differential reflectivity |
| CC / rhoHV | Copolar correlation from the bulk species covariance |
| KDP | Specific differential phase |
| PHI / PhiDP | Accumulated two-way differential phase plus configured system phase |
| AH / ADP | Specific horizontal / differential attenuation |
| PIA / PIDA | Integrated two-way horizontal / differential attenuation |
| MCOV | Model support fraction of the pulse-volume quadrature at the gate |
| TUNB | Terrain-unblocked fraction; blocked weight is retained as lost signal |
| MSIG | Meteorological-signal fraction after model support and terrain blockage |
| IREF / IVEL / ISW / IZDR / IRHO / IKDP | Ideal pulse-volume-integrated moments before virtual receiver/estimator effects |
| MREF / MVEL / MSW / MZDR / MRHO / MKDP | Measured moments after the coupled instrument/estimator and before presentation effects |
| DIF_* | Exact-gate simulated-minus-observed validation products |

Corrected fields are not generic post-processing guesses. They are the
intrinsic values retained by the same radial propagation calculation that
produces the observed moments.

The bulk operator recognizes common Lin/WSM/WDM, Thompson, Morrison,
Milbrandt-Yau, and NSSL bulk schemes and records whether the available fields
provide full two-moment, partial two-moment, or assumption-heavy mass-only
closure. P3 and ISHMAEL use property-based ice categories that cannot honestly
be relabeled as snow/graupel/hail. When the **bulk Rayleigh** kernel is selected,
those schemes therefore fall back to scalar REF/VEL with an explicit note.

## Opt-in property-aware T-matrix research contract

BowEcho defines a separate, opt-in research-mode contract for P3
`mp_physics` 50-53 and ISHMAEL `mp_physics` 55. It is not the default bulk
operator. A build may evaluate this mode only when its versioned property-aware
tables and runtime descriptor match the request exactly; an absent or
inapplicable asset is an error, never a silent fallback to Rayleigh.

### Practical file check

The global `MP_PHYSICS` value must be 50-53 for P3 or 55 for ISHMAEL, **and**
the raw WRF output must retain the native property variables required by the
matching reader. The scheme number alone is not sufficient. Thompson
`mp_physics=8` and other conventional schemes should use Storm view, Clean
model truth, Clean dual-pol, Real radar, or Maximum fidelity. The research
recipe fails closed with a property-reader error rather than guessing or
falling back.

The data path preserves each scheme's native raw tuples. P3 category
1/category 2 and ISHMAEL planar/columnar/aggregate mass, number, rime, bulk
density, liquid fraction, aspect ratio, temperature, and air density are read
as available. Each active source cell is closed once before its bounded LUT
evaluation; the resulting ZH/ZV/covariance/KDP/attenuation/fall moments remain
additive through spatial, beam, and time sampling. Nonlinear products are
derived only after those sums.

ISHMAEL `mp_physics=55` uses a scheme-native dry-frozen PSD integration. Each
planar, columnar, or aggregate tuple reconstructs its native gamma distribution
from QICE/QNICE/QVOLI/QAOLI and exact dry-air density. Diameter quadrature
selects oblate or prolate support from the node's diagnosed habit, enforces the
table's terminal-speed law and value, and audits convergence plus the omitted
number, mass, and sixth-moment tails. A node is never clipped to a table edge;
the cell fails when its declared support/omission envelope is exceeded.

P3 50-53 remains explicitly labeled **characteristic-particle** science in the
current production path. Its scheme-native distribution requires the matching
source-qualified P3 lookup/mass-property contract and is not inferred from
Q/N/rime bulk values with a generic gamma approximation.

The research tables use PyTMatrix at exactly **2.8 GHz**, with distinct oblate
and prolate spheroids and a symmetric Bruggeman effective-medium mixture of
air, ice, and liquid water. Their radar-view axis covers pulse-volume offsets
around all 19 custom/named cut centers from 0.1 degrees through 19.5 degrees,
including center plus/minus the correctly converted 0.95-degree-FWHM Gaussian
beam sigma. The declared view axis is -0.5 through 20 degrees. A custom beam or
quadrature sample outside that axis fails closed. Frozen-particle orientation
is a mean-zero Gaussian canting distribution with a fixed 20-degree standard
deviation and deterministic 5 by 10 (50-point) orientation integration.

Dielectric applicability is table-specific. Dry-ice nodes use the declared
Mätzler parameterization over roughly 190-273.15 K; Warren and Brandt
(2008) note the underlying fits over 190-258 K, so the warmer dry nodes remain
part of the research uncertainty rather than evidence of validation. Wet
coexistence nodes use Liebe-Hufford-Manabe liquid water with symmetric
Bruggeman mixing only over 269.15-275.15 K (-4 to +2 degrees C).
Standalone and unpaired residual rain uses its separate temperature-dependent
Liebe-Hufford-Manabe table over 250-313.15 K; paired liquid is removed exactly
once before residual rain is evaluated.

Lookup is bounded: there is no clamping or extrapolation across particle,
material, temperature, frequency, orientation, or view axes. Signed KDP is
preserved through per-particle lookup, accumulation, compact storage, and
radial PhiDP integration. Within those declared bounds, a T-matrix table can
represent nonspherical Waterman T-matrix and non-Rayleigh/resonant scattering
that the bulk Rayleigh kernel cannot. The reproducible PyTMatrix solver uses
role-specific, contiguous envelopes rather than reducing every particle to
the hardest wet-column corner: dry oblate reaches 89 mm, dry prolate 32.312
mm, wet oblate 15 mm, and wet prolate 6.3 mm. The dry-prolate cap stops before
an unresolved, solver-sensitive KDP resonance exposed by the next 36.126 mm
node; higher isolated passes are not bridged. A particle outside its own
phase/shape envelope fails closed instead of being clamped.

The five embedded property tables contain **2,640,848 grid points**: 616,000
dry-oblate, 132,160 dry-prolate, 1,017,600 wet-oblate, 864,000 wet-prolate,
and 11,088 residual-rain points. The frozen solver contract is
`ddelt=0.001`, `ndgs=14`. The v9 12-to-14 convergence report (SHA-256
`352e259f02b606ac71579b7d3b4591c3088b41f8e74becd949a27e5721030067`)
is reusable by exact config hash only for dry-prolate, wet-oblate, and
wet-prolate: 2,013,760 points and 18,123,840 component comparisons. It does
not establish all-node convergence for the later-refined dry-oblate or rain
configs.

The predeclared diameter-only design audit stopped after its depth-three bound
without passing (report SHA-256
`e883060f67f72d8be7abe6bf0e53d8e572f2d7299fbb04d4a4ad96ee97d8832c`).
The final candidate therefore uses a finite midpoint refinement of the
affected diameter cells and implicated non-diameter intervals; it does not
copy held-out coordinates, extend a physical domain, change a threshold, or
reroll a seed. The one frozen held-out request (SHA-256
`2b4d143d86aaf78913df165b329ae62f6076e2574f34c1d393b6b9ddd90a45c5`)
then passed all **30 of 30** selected nodes across the five embedded property
tables. Its shared eight-table report (SHA-256
`82f07bc736f6b5f20c7a59117204b69d97b9cbcb9f915cc54be394b0d8b742ce`)
is not an all-eight pass: two of six nodes failed in the separate,
non-embedded conventional dry-ice fixture. Exact table/config hashes and the
scope boundary are recorded in
`validation/tmatrix/refined_grid_v10_property_bundle_acceptance.json`.

This remains **research-only and not independently validated**. ISHMAEL now
integrates its reconstructable native PSD, while P3 still scales one
closure-derived characteristic particle by number concentration. The
reproducible generator checks table integrity and held-out interpolation, not
agreement with an operational radar. No PSD implementation, table,
orientation model, VCP choice, or visually plausible output creates an
operational-calibration claim.

## Coupled instrument stages and Algorithm Truth Lab

The optional coupled single-PRF estimator makes frequency, pulse width, PRF,
dwell, pulse count, independent-sample fraction, and sensitivity one physical
contract. It derives wavelength, Nyquist velocity, unambiguous range, and the
matched-filter range response instead of letting those values contradict one
another. Named VCP PRF *codes* remain unresolved rather than being treated as
hertz; the coupled path therefore applies only where a literal research PRF is
known.

Enable **Emit Ideal + Measured diagnostic moments** to retain all three stages:

1. **Ideal**: perfect pulse-volume-integrated scattering.
2. **Measured**: matched pulse weighting, PRF ambiguity, sample-count/SNR
   uncertainty, receiver sensitivity, blockage, and attenuation.
3. **Presented**: the ordinary products after optional deterministic texture,
   clutter, display thresholds, and other presentation choices.

Open **Windows > Algorithm Truth Lab** on a synthetic pane to compare the exact
co-gridded I/M/Presented stages. It reports signed bias, MAE, RMSE, percentile
and maximum errors for the retained moment triples, plus analytic folding
exposure. When an exact-geometry DVEL grid is present it also reports recovered
folds, branch errors, false unfolds, and velocity error. The current lab does
not fabricate VWP, GBVTD, tracking, or vortex truth metrics; those remain
unavailable until their production algorithms have dedicated model-truth
adapters.

## Geometry, instrument, and presentation controls

The virtual antenna can use the WRF domain center, an explicit
latitude/longitude, or a catalog NEXRAD site. Its altitude follows model
terrain plus the configured tower offset. Default geometry uses the standard
14-tilt ladder, 720 radials, 250 m gates, and a 230 km range under 4/3-Earth
beam geometry. An optional 0.1 degree cut precedes the normal 0.5 degree
lowest tilt.

Range can extend to 1000 km. Automatic spacing preserves the classic gate
count as range grows; **Match gate size to grid resolution** instead uses WRF
DX (bounded to the supported range) so a coarse model is not oversampled into
many identical radar gates.

Reflectivity can use model-native REFL_10CM when present or the classic
Stoelinga/community diagnostic. The scientific sampling default is linear Z;
legacy direct-dBZ interpolation exists only to reproduce older BowEcho
renders.

Instrument controls include S-band frequency, beam width, pulse width, PRF,
range-dependent sensitivity, calibration phase/ZDR bias, Nyquist folding,
rotation rate, and inter-sweep transition. Named Build 24 patterns own their
physical rows, source rates, periods, waveforms, and PRF-code provenance, so
the custom timing/PRF-Hz controls do not overwrite the source definition.
Reflectivity texture, velocity wobble, and ground clutter are deterministic
virtual-instrument/presentation effects. They do not add model-resolved
meteorological structure.

## Memory and export

Large convection-resolving WRF files make one f32 3-D field hundreds of MiB.
The bulk path reads hydrometeor species sequentially and retains compact
scattering fields rather than every raw mass/number field. The property path
reads a sparse native scene, includes that raw state in its pre-build peak,
precomputes only active-cell additive scattering at the five elevation nodes,
and then drops the raw property arrays. Adjacent-scene mode uses a bounded
two-input-scene cache and checks the exact retained sparse-scene size after
each read in addition to input, embedded tables, read/build/cut scratch, and
retained output volumes. Raw-state mode deliberately retains the two normalized
raw property scenes and dense dynamics/thermodynamics needed for on-demand
pre-closure evaluation; its stricter preflight includes that extra ownership.
It does not silently substitute a smaller kernel when the configured ceiling
is exceeded.

For one generated frame, CfRadial export opens a `.nc` save dialog. For a loop,
it opens a folder picker and writes one CfRadial-1 file per frame. Every file
includes all native and attenuation moments, real ray times, frequency, beam
width, pulse width, PRT, unambiguous range, scan name, model and microphysics
provenance, calibration settings, and the forward-operator configuration. The
pinned NetCDF writer cannot yet emit the required character variables for
strict `sweep_mode` and `prt_mode`; BowEcho does not fake them as numeric
variables.

## Honest limitations

- Source-model resolution is the information ceiling. Smaller radar gates and
  more quadrature samples cannot recover unresolved storm structure.
- Adjacent-scene modes are bounded interpolation between two compatible WRF
  scenes, not model integration and never temporal extrapolation. Derived/
  additive and raw pre-closure spaces remain explicitly distinct. Frozen,
  held, dropped, and failed outcomes remain distinguishable in provenance.
- Named Build 24 VCPs reproduce their checked Appendix C base rows, not SAILS,
  MRLE, AVSET, Add-MPDA, site adaptations, or an observed operational volume.
- The default dual-pol kernel remains bulk S-band Rayleigh. In that mode, P3,
  ISHMAEL, missing hydrometeors, and unsupported closures fall back explicitly
  to scalar REF/VEL with a diagnostic note.
- The opt-in T-matrix contract is research-only, table-bounded, and not
  independently validated. ISHMAEL has a scheme-native dry-frozen PSD path;
  P3 remains a characteristic-particle approximation. Neither is operational
  calibration.
- Symmetric Bruggeman air/ice/water mixing represents a declared effective
  medium; it is not a complete prognostic melting-layer microphysics model.
- Deterministic clutter and texture are optional synthetic instrument effects,
  not observations and not additional WRF physics.
- Strict CfRadial character variables for sweep/prt mode remain blocked by the
  pinned writer; numeric substitutes are not fabricated.

## Remaining scientific boundary

Independent comparison against trusted polarimetric forward operators and
observations is still required before any production-science claim. The
repository includes an exact-geometry multi-case validation harness with
independent-operator provenance, JSON/Markdown scorecards, pooled bias/MAE/
RMSE/percentile/correlation metrics, and fail-closed geometry matching; useful
results still require real independent reference cases. P3 scheme-native PSD,
category- or flow-dependent frozen-particle orientation table families,
prognostic melting-state evolution, scattering beyond each strict
role-specific PyTMatrix convergence envelope, and adaptive operational VCP
behaviors remain outside the current production contract.
