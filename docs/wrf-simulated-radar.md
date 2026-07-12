# WRF simulated radar

BowEcho samples a WRF atmosphere onto a native polar radar volume. Every
elevation, radial, and gate enters the ordinary radar viewer, so loops,
cross-sections, readouts, derived products, velocity dealiasing, and CfRadial
export use the same path as observed radar.

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
