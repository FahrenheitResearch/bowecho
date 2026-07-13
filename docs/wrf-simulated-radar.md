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
availability, and per-ray Nyquist. When the decoded source carries ray-local
instrument metadata, replay also copies PRT, unambiguous range, pulse count,
and independent-sample count to the matching simulated rays. The result opens
as linked **Observed / Simulated / Difference** panes. This is exact
acquisition replay, not a reconstructed base VCP.

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

## Operational HRRR/RRFS forecast radar

The **Operational forecast radar** card uses the same polar renderer, radar
location/range controls, loop engine, refresh action, product picker, and
CfRadial export as raw WRF. Its input is native hybrid-level model GRIB, not a
pre-rendered reflectivity image.

- **Latest HRRR - build** resolves one current published cycle through the
  existing SimSat HRRR source logic and can build f00, f01, or the two-frame
  f00/f01 loop. Downloads use SimSat's resumable native-file downloader and
  input directory; BowEcho does not create a second HRRR cache.
- **Build cached HRRR** discovers complete native files in the shared SimSat
  input and model-cache directories.
- **Choose local HRRR/RRFS files...** is an unrestricted multi-file picker for
  native GRIB. Compatible valid times become ordinary radar-loop frames.
- **Refresh** re-resolves a latest-HRRR request, but deliberately reuses the
  exact snapshot for local/cached selections.

The reader crops the native model around the virtual radar, retains hybrid
height and earth-relative winds, converts pressure vertical velocity (omega)
to geometric `w`, rotates grid-relative winds when the GRIB declares them, and
streams cloud liquid, cloud ice, rain, snow, and graupel through additive
scattering without keeping duplicate full species volumes. SPFH is converted
using its declared moisture basis; optional TKE enters spectrum width.

HRRR uses an explicitly versioned Thompson-family category mapping. RRFS uses
the categories actually present in the file and does not infer an unencoded
microphysics scheme. Missing number fields use documented bulk-PSD defaults.
These are **bulk S-band dual-pol-like forecast assumptions**, not property
T-matrix science, observed calibration, or proof that the operational model
ran a particular hidden scheme configuration. Incomplete or inconsistent
base/species inventories fail closed and every assumption is retained in
volume/CfRadial provenance.

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
- **P3/ISHMAEL T-matrix (research)** selects the shipped legacy embedded
  2.8 GHz S-band, Full-property, property-aware research operator without
  changing the chosen antenna or scan geometry. Expert controls can instead
  request a validated local exact-frequency pack, but never silently change
  the source or band.

Changing an advanced control after applying a recipe labels the setup as
`Custom tuning`. Selecting a recipe again resets all interacting physics,
calibration, and instrument values together, so stale expert values cannot
leak into a new run.

The recipes also choose coherent temporal defaults. **Storm view**, **Clean
model truth**, and **Clean dual-pol** start Frozen. **Real radar** and
**Maximum fidelity** start with Timed volume, fast derived/additive adjacent
sampling, and Hold last. **P3/ISHMAEL T-matrix** starts with the slower
Raw-state pre-closure reference, legacy embedded 2.8 GHz S tables,
Full-property sensitivity, and Hold last. These remain editable after applying
the recipe, subject to the exact table/time compatibility rules below.

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

Ray-local instrument metadata is part of that exact replay contract when the
source actually provides it. BowEcho preserves aligned per-ray PRT,
unambiguous range, pulse count, and independent-sample count beside the copied
Nyquist value and writes those arrays through CfRadial. A source that lacks a
field stays missing; replay does not infer it from a named VCP code or a
volume-wide average.

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

The default beam path uses standard 4/3-Earth geometry. An explicit **WRF
refractivity (research)** choice instead reads `P`, `PB`, `T`, and `QVAPOR` at
the actual virtual-radar site, derives the vertical refractivity profile, and
ray-traces gate ground range and height through that model atmosphere. The
same resolved path is used for model sampling and terrain blockage at every
cut and quadrature point. Missing thermodynamic fields, incomplete vertical
coverage, or a profile that does not match the selected site stop the build;
BowEcho does not silently fall back to 4/3 Earth. The output records the local
refractivity gradient, propagation regime, and a visible ducting warning when
applicable. Operational HRRR/RRFS forecast radar remains on standard 4/3-Earth
geometry because that ingest path does not yet carry this qualified site
profile.

Terrain blockage uses a cumulative apparent horizon for every azimuth and
range under the selected beam geometry. Blocked quadrature weights remove
received power instead of being renormalized, which gives partial beam
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

The current Raw-state implementation is deliberately narrower than the
ordinary compact-scene path: it requires the legacy embedded S source at
exactly 2.8 GHz with **Full property** rain/melting sensitivity. A validated
local S/C/X pack or **Frozen-only** sensitivity must use Frozen or **Linear
adjacent (derived/additive)** timing. The UI disables new incompatible
Raw-state selection, and backend validation rejects any retained incompatible
combination.

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

### Gate support emission and coverage mask

**Gate support fields (MCOV / TUNB / MSIG)** is enabled by default. The three
compact fractions are emitted for every generated gate and remain audit
products rather than display guesses. Disabling emission removes those moment
grids from new volumes; it does not change the underlying pulse-volume
sampling used to build the physical moments.

**Minimum model coverage** is a separate persisted threshold from 0 to 1.
Physical moments are masked when MCOV is below that threshold, while emitted
MCOV/TUNB/MSIG remain unmasked so the rejection can be diagnosed. The default
is zero for historical behavior. TUNB and MSIG do not independently mask a
gate: they report terrain-unblocked and meteorological-signal support inside
the same quadrature contract.

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

### Exact table source and band

The T-matrix controls separate **Table source** from **Exact band**:

- **Legacy embedded S** is the shipped research-v1 five-table bundle and is
  valid only at exactly **2.8 GHz**. Selecting it makes C and X unavailable.
- **Validated local pack** requests one manifest-qualified five-role pack at
  exactly **2.8 GHz S**, **5.6 GHz C**, or **9.4 GHz X**. Selection never uses
  a nearest frequency, substitutes another band, or falls back to the embedded
  source. A missing, ambiguous, corrupt, or `unvalidated_research` pack stops
  the build before the large WRF read.

No validated C- or X-band packs ship with BowEcho. Therefore the C and X UI
choices are capability gates, not bundled science: they remain unavailable
until an evidence-backed local pack is installed. BowEcho also ships no
validated-local replacement S pack; the usable bundled S source remains the
explicitly research-only legacy embedded bundle.

Each local pack is a directory below the deterministic, override-aware model
cache path `bowecho-simradar/tmatrix-packs`. The UI shows the fully resolved
path on the running system. A pack contains `pack.json` plus exactly five
role/config/LUT records: dry oblate, dry prolate, wet oblate, wet prolate, and
standalone/residual rain. Runtime discovery validates the declared exact band,
science revision, byte lengths, SHA-256 digests, role identities, and the
`validated_research` status before typed LUT decoding.

The reproducible generator is
`crates/radar_scattering/tools/pytmatrix-0.3.3/generate_band_pack.py`; its full
format and locked-container workflow are documented in the adjacent
`PACK_FORMAT.md`. It can generate deterministic S/C/X packs, but deliberately
marks every output `unvalidated_research`. Generation proves reproducibility
and internal integrity, not scientific validation, so its output is rejected
by the runtime until a separate evidence-bound promotion produces a new
reviewed manifest.

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

P3 50-53 uses the matching official WRF P3 v5.4 lookup table to reconstruct
the scheme-native distribution: 50-52 use the exact two-moment table and 53
uses the exact triple-moment table, including WRF's five-iteration M3/M6 shape
solve. BowEcho downloads that 1.6 or 17.9 MB asset only when required, caches
it outside the executable, and accepts it only after the pinned byte length,
SHA-256, header, record layout, source commit, and scheme/table-kind checks all
pass. PSD quadrature then integrates additive scattering through the dry
T-matrix node tables. Table-support omissions remain fail-closed and are
audited independently by number, mass, and P3's scheme-consistent
mass-squared (equivalent-ice-volume-squared) radar weight. The latter matches
the variable-density ice reflectivity weight in the official P3 table
generator; using raw `Dmax^6` here would incorrectly give a fluffy aggregate
the radar weight of a solid constant-density sphere. Native P3 M6 remains the
separate PSD reconstruction and quadrature-tail closure authority. The
production projected-area policy permits at most 0.25% omitted P3 radar
weight (about 0.011 dB in the Rayleigh-equivalent limit), while number and
mass retain their independent hard gates; no missing node is clamped or
fabricated.

P3 predicts maximum dimension, mass, and projected area, but not one unique
spheroidal habit or canting distribution. The usable research policy therefore
keeps lambda, mu, PSD weights, mass, and area scheme-native while mapping each
node to an explicitly named projected-area-equivalent oblate spheroid with the
table's Gaussian-20 canting assumption. Provenance states that this shape and
orientation are external research assumptions, not P3 predictions. A stricter
shape-authoritative policy exists and evaluates only genuinely spherical P3
regions under omission budgets; neither policy silently revives the removed
single-characteristic-particle production path.

### Full-property and Frozen-only sensitivity

**Full property** requires the complete rain state, evaluates standalone and
residual rain, and permits qualified homogeneous wet-frozen/rain coexistence
inside the selected five-table envelope. **Frozen-only** deliberately omits
all rain and wet-coexistence scattering and retains only dry frozen P3 or
ISHMAEL categories. It is a sensitivity experiment for isolating the frozen
contribution, not a claim that the source atmosphere contains no rain.

This switch does not create a prognostic melting model. Full-property wet
coexistence is a declared mass-conserving diagnostic partition and symmetric
effective-medium topology; Frozen-only is an explicit omission. Both choices
are recorded in the configuration fingerprint, progress/provenance, and
scattering-model label.

The shipped legacy embedded research tables use PyTMatrix at exactly
**2.8 GHz**, with distinct oblate and prolate spheroids and a symmetric
Bruggeman effective-medium mixture of air, ice, and liquid water. Their
radar-view axis covers pulse-volume offsets around all 19 custom/named cut
centers from 0.1 degrees through 19.5 degrees, including center plus/minus the
correctly converted 0.95-degree-FWHM Gaussian
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
Liebe-Hufford-Manabe table over 225-313.15 K; paired liquid is removed exactly
once before residual rain is evaluated.

For scheme-native P3 and ISHMAEL dry-frozen PSD nodes, the table coordinate is
the phase-constrained particle/material temperature, not blindly the ambient
air temperature: it follows ambient air below freezing and is capped at
273.15 K while the source retains ice in above-freezing air. The raw
environment, PSD, mass, shape, and any separate rain remain unchanged. This is
an explicit melting-phase constraint, not generic LUT clamping or a diagnosed
liquid fraction; neither scheme supplies enough native state here to invent a
wet-particle PSD.

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

This remains **research-only and not independently validated**. ISHMAEL and P3
now integrate their reconstructable native PSDs, but P3 spheroid shape/canting
remains the explicit projected-area-equivalent research mapping described
above. The reproducible generator checks table integrity and held-out
interpolation, not agreement with an operational radar. No PSD implementation,
table, orientation model, VCP choice, or visually plausible output creates an
operational-calibration claim.

## Coupled instrument stages and Algorithm Truth Lab

The optional coupled single-PRF estimator makes frequency, pulse width, PRF,
dwell, pulse count, independent-sample fraction, and sensitivity one physical
contract. It derives wavelength, Nyquist velocity, unambiguous range, and the
matched-filter range response instead of letting those values contradict one
another. Named VCP PRF *codes* remain unresolved rather than being treated as
hertz; the coupled path therefore applies only where a literal research PRF is
known.

For a custom coupled scan, BowEcho stamps the resolved PRT, unambiguous range,
transmitted pulse count, and effective independent-sample count on every ray.
Those ray-aligned values survive loop installation, merge only into missing
slots, and export as per-ray CfRadial arrays. The legacy volume-level PRT and
unambiguous-range fields remain a compatibility fallback for sources without
ray-local values; they are not used to flatten differing ray metadata.

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

### Why This Gate and on-demand spectrum

Right-click a synthetic gate and choose **Why this synthetic gate?** for the
retained geometry, acquisition/temporal bracket, MCOV/TUNB/MSIG support,
beam/refractivity geometry, propagation, and Ideal/Measured/Presented stages.
The immediate cards use data already present in the volume. The deeper
explanation requires the session-private retained WRF source descriptor; it
first verifies the frame configuration fingerprint and geometry witness, then
reopens the exact source file/time index rather than trusting a changed path.

The worker recomputes the selected radial from its first gate through the
selected gate. That radial-prefix behavior is required because PhiDP,
horizontal/differential attenuation, refracted terrain blockage, and Presented
values depend on all preceding gates; recomputing only the selected cell would
give a different physical answer. Source paths remain session-private and are
not written into the exported provenance.

For the bulk-Rayleigh WRF kernel, the same recomputation can expose individual
bulk hydrometeor contributions and synthesize a selected-gate true, aliased,
noise, measured, and noise-subtracted Doppler spectrum. The property T-matrix
path currently exposes the aggregate physical/polar and instrument-stage
answer only. Its P3/ISHMAEL per-category contribution seam is not yet qualified,
so the inspector explicitly withholds the category decomposition and Doppler
spectrum instead of reverse-engineering them from aggregate REF/VEL/dual-pol
moments. Operational HRRR/RRFS frames and volumes whose retained source no
longer matches are likewise reported unavailable.

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

Instrument controls include bulk-path S-band frequency or an exact property
table band, beam width, pulse width, PRF, range-dependent sensitivity,
calibration phase/ZDR bias, Nyquist folding, rotation rate, and inter-sweep
transition. Named Build 24 patterns own their physical rows, source rates,
periods, waveforms, and PRF-code provenance, so the custom timing/PRF-Hz
controls do not overwrite the source definition.
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
each read in addition to input, the selected table source, read/build/cut
scratch, and retained output volumes. Raw-state mode deliberately retains the
two normalized
raw property scenes and dense dynamics/thermodynamics needed for on-demand
pre-closure evaluation; its stricter preflight includes that extra ownership.
It does not silently substitute a smaller kernel when the configured ceiling
is exceeded.

For one generated frame, CfRadial export opens a `.nc` save dialog. For a loop,
it opens a folder picker and writes one CfRadial-1 file per frame. Every file
includes all native and attenuation moments, real ray times, frequency, beam
width, pulse width, ray-local PRT, unambiguous range, transmitted pulse count,
independent-sample count when present, scan name, model and microphysics
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
  independently validated. ISHMAEL and P3 have scheme-native PSD paths; P3's
  equivalent-oblate shape and Gaussian-20 canting remain external assumptions.
  Neither is operational calibration.
- The legacy embedded 2.8 GHz S bundle is the only shipped property-table
  source. No validated C- or X-band pack ships; selecting those exact bands
  requires a separately installed, evidence-backed `validated_research` pack
  and otherwise fails closed.
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
results still require real independent reference cases. Category- or
flow-dependent frozen-particle orientation table families, prognostic
melting-state evolution, scattering beyond each strict role-specific PyTMatrix
convergence envelope, independently validated C/X-band table packs, and
adaptive operational VCP behaviors remain outside the current production
contract.
