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
   changing.
3. Choose **Build from files...** to multi-select WRF files, or **Build from
   folder...**. Every selected WRF time becomes one radar-loop frame.
4. Inspect the generated loop with the ordinary product, tilt, cross-section,
   readout, derived-product, and velocity workflows.
5. Change controls and choose **Refresh current frame(s)** to rerun the same
   source snapshot without another picker.
6. Use **Export latest as CfRadial...** when a portable radar volume with
   model/operator provenance is needed.

Extensionless **wrfout_*** names from every domain are valid. A folder build
captures the files found at selection time. Refresh deliberately reuses that
ordered session snapshot; it does not rescan the folder or silently add files
created later.

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

Changing an advanced control after applying a recipe labels the setup as
`Custom tuning`. Selecting a recipe again resets all interacting physics,
calibration, and instrument values together, so stale expert values cannot
leak into a new run.

### Recipe comparison

| Recipe | Best use | Pulse volume | Main output character |
|---|---|---:|---|
| Storm view (fast) | Fast loop browsing | Center | Textured REF, clean unfolded VEL |
| Clean model truth | Model diagnosis | Center | No presentation/instrument effects |
| Clean dual-pol | Microphysics comparison | Balanced (9) | Polarimetry/propagation without noise, folds, or blockage |
| Real radar (balanced) | Practical virtual radar | Balanced (9) | Full S-band instrument path |
| Maximum fidelity (slow) | One frame or short loop | Reference (27) | Full deterministic 3 x 3 x 3 integration |

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

Timed volumes stamp a monotonic acquisition offset on every ray using the
configured rotation rate and inter-sweep transition. The current timed mode
samples one WRF scene; it does not claim temporal interpolation between model
files or a numbered operational VCP.

## S-band dual polarization

The first dual-pol operator is a scheme-aware bulk S-band Rayleigh model. It
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

Corrected fields are not generic post-processing guesses. They are the
intrinsic values retained by the same radial propagation calculation that
produces the observed moments.

The operator recognizes common Lin/WSM/WDM, Thompson, Morrison,
Milbrandt-Yau, and NSSL bulk schemes and records whether the available fields
provide full two-moment, partial two-moment, or assumption-heavy mass-only
closure. P3 and ISHMAEL use property-based ice categories that cannot honestly
be relabeled as snow/graupel/hail; BowEcho therefore falls back to scalar
REF/VEL with an explicit note for those schemes.

This implementation is **not a T-matrix solver**. Frozen-particle orientation,
full melting-layer coexistence, non-Rayleigh resonance, and scheme-native P3
properties require an offline, versioned T-matrix lookup table. The scattering
kernel has a stable additive-moment interface so that LUT can replace the
current bulk kernel without changing sampling, propagation, the viewer, or
CfRadial output.

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
rotation rate, and inter-sweep transition. Reflectivity texture, velocity
wobble, and ground clutter are deterministic virtual-instrument/presentation
effects. They do not add model-resolved meteorological structure.

## Memory and export

Large convection-resolving WRF files make one f32 3-D field hundreds of MiB.
BowEcho reads hydrometeor species sequentially and retains polarimetric ratios,
phase, propagation coefficients, and fall moments in compact one-byte planes;
it does not keep every raw mass/number field resident.

CfRadial export includes all native and attenuation moments, real ray times,
frequency, beam width, pulse width, PRT, unambiguous range, scan name, model and
microphysics provenance, calibration settings, and the forward-operator
configuration. The pinned NetCDF writer cannot yet emit the required character
variables for strict `sweep_mode` and `prt_mode`; BowEcho does not fake them as
numeric variables.

## Honest limitations

- Source-model resolution is the information ceiling. Smaller radar gates and
  more quadrature samples cannot recover unresolved storm structure.
- Timed rays sample one WRF atmosphere. There is no interpolation between
  forecast files and no claim to reproduce a numbered operational VCP.
- The current dual-pol kernel is bulk S-band Rayleigh, not T-matrix.
- Full melting-layer coexistence, frozen-particle orientation, non-Rayleigh
  resonance, and scheme-native P3 properties are not implemented.
- P3, ISHMAEL, missing hydrometeors, and unsupported closures fall back
  explicitly to scalar REF/VEL with a diagnostic note.
- Deterministic clutter and texture are optional synthetic instrument effects,
  not observations and not additional WRF physics.
- Strict CfRadial character variables for sweep/prt mode remain blocked by the
  pinned writer; numeric substitutes are not fabricated.

## Clearly labeled future work

A versioned offline T-matrix scattering lookup table, true temporal
interpolation between model outputs, and explicit operational-VCP emulation
are possible future extensions. They are **not** features of the v0.33
operator.
