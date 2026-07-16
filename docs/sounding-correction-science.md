# Sounding correction science contract

This document defines the scientific behavior of BowEcho's manual sounding-correction tools. The feature creates an analyst-authored, derived profile for display and diagnostic recomputation. It is **not data assimilation**, does not estimate an objectively analyzed atmospheric state, and must not overwrite or masquerade as the source observation or model profile. The corrected profile must retain its source and correction provenance.

## Conventions and constants

Use SI units internally and the project's canonical thermodynamic constants and saturation-vapor-pressure implementation. Do not introduce a second set of approximations in the UI layer.

- Pressure `p` is total air pressure and must be positive.
- Temperature and dewpoint are absolute temperatures internally.
- Reference pressure `p0` is 100,000 Pa.
- `kappa = Rd / cpd` and `epsilon = Rd / Rv` (approximately 0.622).
- Heights used for blend extents are AGL unless the UI explicitly says otherwise.
- Meteorological wind direction is the direction **from** which the wind blows, clockwise from true north.

Values such as `p0`, `Rd`, `cpd`, and `Rv` are physical definitions or constants. UI defaults, warning tolerances, blend depths, and severity thresholds below are product heuristics and must remain labeled as such.

## Thermal correction coordinate

Dry potential temperature is

```text
theta = T * (p0 / p)^kappa
Pi    = (p / p0)^kappa
T     = Pi * theta
```

The editor must support both `Potential temperature (theta)` and `Temperature (T)` correction coordinates. Potential temperature is the default for boundary-layer corrections because the daytime convective mixed layer is approximately well mixed in potential temperature. A constant positive temperature increment applied through decreasing pressure does not represent the same dry-adiabatic perturbation.

For an anchor at pressure `pa`, a requested temperature change converts to potential-temperature space as

```text
delta_theta_a = delta_T_a / Pi(pa)
```

Changing the editor toggle must convert existing anchors so that the already-corrected physical profile does not change. It must never reinterpret a stored numeric `delta_T` as `delta_theta`, or vice versa.

Temperature mode remains useful for a genuinely level-local temperature bias. It should not be described as a mixed-layer warming preset.

## Moisture correction coordinate

Dewpoint is convenient for display but nonlinear under vertical blending. Specific humidity `q` is the canonical moisture correction coordinate. The UI may accept or display dewpoint, but it must transform every anchor to `q`, blend in `q`, then derive dewpoint diagnostically from the completed corrected profile.

Let `e` be vapor pressure, `es,w(T)` saturation vapor pressure over liquid water, `r` water-vapor mixing ratio per mass of dry air, and `q` specific humidity per mass of moist air:

```text
e = es,w(Td)
r = epsilon * e / (p - e)
q = r / (1 + r)
  = epsilon * e / (p - (1 - epsilon) * e)
```

The inverse transformation is

```text
e  = p * q / (epsilon + (1 - epsilon) * q)
Td = inverse_es,w(e)
```

Saturation specific humidity and relative humidity are

```text
qs(T, p) = epsilon * es,w(T) / (p - (1 - epsilon) * es,w(T))
RH       = 100 * e / es,w(T)
```

Apply the thermal and moisture corrections before deriving `Td` and `RH`. Use one consistent liquid-water or ice convention throughout. The WMO convention evaluates ordinary relative humidity with respect to liquid water below 0 C; frost point is a distinct quantity. If BowEcho offers an ice-relative quantity, label it explicitly.

The editor must reject non-finite states, `q < 0`, `q >= 1`, `e <= 0` except for an explicitly represented perfectly dry limit, and `e >= p`. Supersaturation is a QC condition, not something to hide by silently clipping `q`. An explicit saturation-limiting operation would remove vapor and therefore requires a phase-change and energy treatment that is outside this correction contract.

## Wind correction coordinate

Convert speed and meteorological direction to earth-relative Cartesian components before interpolation or blending:

```text
u = -speed * sin(direction)
v = -speed * cos(direction)
```

with direction in radians. Convert back with

```text
speed     = sqrt(u*u + v*v)
direction = mod(degrees(atan2(-u, -v)), 360)
```

Apply corrections to `delta_u` and `delta_v` using one shared wind blend function. Do not interpolate speed and direction independently: doing so is discontinuous across 0/360 degrees and ill-defined near calm wind. A calm wind has no meaningful direction.

## Blend extents and shapes

Thermal, moisture, and wind corrections must each have independent vertical extent and shape. The two wind components share one wind extent and shape. A single global blend height is not scientifically adequate.

For any canonical correction variable `x` (`theta` or `T`, `q`, `u`, or `v`), apply

```text
x_corrected(z) = x_source(z) + W_x(z) * delta_x_core(z)
```

`delta_x_core(z)` is constant for a simple surface preset or piecewise linear through multiple correction anchors. Construct one correction profile per variable; do not sum overlapping independent tapers that can create unintended extrema.

### Mixed-layer core with cosine cap

The default boundary-layer thermal shape applies the full potential-temperature increment through the correction core and tapers only near its top:

```text
W(z) = 1                                             z <= z_core
     = 0.5 * (1 + cos(pi * (z-z_core)/d_taper))     z_core < z < z_core+d_taper
     = 0                                             z >= z_core+d_taper
```

This shape has zero slope at both joins and avoids creating a seam at the surface. It does **not** guarantee static stability inside the upper taper: a positive warming increment decreases upward there and can weaken or overturn a weak cap. Post-edit QC is still required.

### Custom piecewise-linear shape

A custom shape is an ordered list of finite control points `(zj, wj)`, with strictly increasing `zj`. The normal UI constrains `wj` to `[0, 1]`. Between adjacent points,

```text
W(z) = wj + (wj+1 - wj) * (z - zj) / (zj+1 - zj)
```

`W` is zero outside the explicitly defined correction extent unless an endpoint is intentionally marked as a surface-attached constant core. Piecewise-linear interpolation does not overshoot its control points. The preview must show the resulting `W(z)` and the final corrected profile, not only the control table.

Core depth, taper depth, and custom control points are analyst choices. A diagnosed model boundary-layer height may seed the controls, but it is not ground truth and must remain editable.

## Project, interchange, and batch workflow

Correction project JSON is bound to the untouched physical source column with a SHA-256 fingerprint. Loading must validate that fingerprint before replacing the current recipe; a mismatch is an error and must never be applied silently. Project files retain the recipe and application/QC provenance but do not embed or overwrite the source sounding.

The evaluated corrected column can be exported as numeric CSV or conventional six-column SPC/SHARPpy `%RAW%` text. RAW import creates a new native, editable sounding. Rows with conventional missing sentinels are reported and skipped rather than interpolated, and the remaining profile must pass the normal structural validation.

A batch experiment selects one correction row and varies one to four available numeric fields. Members are the deterministic Cartesian product (last axis varying fastest), capped at 256 before allocation or evaluation. Every member starts from the same untouched source, applies its complete recipe through the correction engine, and runs the normal SHARPpy analysis builder. Tables and member CSV output use only computed diagnostic values: non-finite or failed members remain explicitly missing and contribute to the displayed finite/failed counts; they are never replaced by zero or another invented value.

## Post-edit quality control

Run QC against the complete corrected profile before recomputing sounding diagnostics. Warnings should identify affected layers and remain visible; only structurally invalid numeric states should prevent preview. Committing the optional dry adjustment has stricter rules described below.

The implemented editor checks:

- finite `p`, `z`, `T`, `q`, `u`, and `v`; positive pressure; pressure decreasing and height increasing upward;
- `q >= 0`, `e < p`, supersaturation, and `Td > T` under the selected saturation convention;
- dry static stability from potential temperature;
- vector-wind shear and edit-induced shear kinks at blend joins;
- the dry-adjustment column-enthalpy residual when that optional operation is applied.

Virtual-potential-temperature stability context and hypsometric/hydrostatic residual checks are useful future extensions, but are not presented by the current UI as completed QC gates. Before/after sounding diagnostics are recomputed and shown by the sounding renderer rather than duplicated in the QC issue list.

Slight supersaturation can be real, and subfreezing water-versus-ice conventions matter. Report it as `supersaturated`, not automatically `impossible`.

Suggested initial **heuristics**, not physical constants:

- surface-layer stability exemption: 100 m AGL, user adjustable;
- minor dry-instability warning: a resolved-layer potential-temperature decrease greater than 0.1 K;
- deep-instability warning: contiguous unstable depth greater than `max(100 m, two native layer spacings)`;
- gross superadiabatic warning: approximately 2 K beyond the dry-adiabatic layer relation, consistent with the tolerance used by NOAA MADIS radiosonde QC;
- supersaturation warning: `RH > 100.5%` or `Td > T + 0.1 K` under one consistent convention;
- wind-seam warning: an edit-induced component-shear kink exceeding both 5 m/s/km and three times the robust local pre-edit shear variation.

These thresholds must be configurable and warnings only. Operational radiosonde QC thresholds are designed to detect gross observational errors and are not universal limits on physically possible atmospheric structure.

## Optional dry convective adjustment

The one-click adjustment is opt-in, previewed, reversible, and labeled **Dry convective adjustment**. It is not a moist-convection solver. It enforces nondecreasing dry potential temperature above the configured surface-layer exemption while conserving pressure-mass-weighted column sensible enthalpy.

For layer `i`, define pressure mass and Exner factor

```text
mi = delta_pi / g
Pi = (pi / p0)^kappa
Ti = Pi * theta_i
```

Apply a pool-adjacent-violators algorithm to levels ordered from bottom to top:

1. Initialize each eligible level as a block.
2. A lower block followed by a smaller upper-block `theta` violates dry stability.
3. Merge violating adjacent blocks.
4. Give the merged block the neutral value

```text
theta_B = sum(mi * Ti) / sum(mi * Pi)
        = sum(mi * Pi * theta_i) / sum(mi * Pi)
```

5. Continue merging until block `theta` is nondecreasing upward.
6. Set `Ti_new = Pi * theta_B` within each merged block.

The weights are therefore `mi * Pi`, not merely layer count or geometric thickness. With constant `cpd`, the block calculation preserves

```text
sum(mi * cpd * Ti_new) = sum(mi * cpd * Ti)
```

which is the discrete pressure-coordinate sensible-enthalpy constraint for dry convective adjustment.

This first dry adjustment leaves `q` unchanged. Before committing it, recompute `qs(T_new,p)` throughout every affected block. Abort if a block is saturated initially under the selected convention or if the candidate adjustment produces `q > qs`. Clipping moisture would violate the stated conservation contract; that case requires a phase-aware moist adjustment with latent heating and is outside this feature.

The preview must show affected layers, maximum temperature change, before/after stability, sensible-enthalpy residual, saturation status, and major diagnostic changes. The action remains off by default.

## Authoritative references

- American Meteorological Society, [Potential temperature](https://glossary.ametsoc.org/wiki/potential-temperature/), [Static stability](https://glossary.ametsoc.org/wiki/vertical-stability/), and [Virtual potential temperature](https://glossary.ametsoc.org/wiki/virtual-potential-temperature/).
- UCAR MetEd, [The atmospheric boundary layer](https://www.meted.ucar.edu/tropical/textbook_2nd_edition/navmenu.php?page=3.0.0&tab=7), including the approximately well-mixed daytime profiles of potential temperature, specific humidity, and momentum.
- Manabe and Strickler (1964), [Thermal Equilibrium of the Atmosphere with a Convective Adjustment](https://www.gfdl.noaa.gov/bibliography/related_files/sm6401.pdf).
- Miyakoda, [Weather Forecasts and the Effects of Sub-grid Scale Processes](https://www.ecmwf.int/sites/default/files/elibrary/1975/11167-weather-forecasts-and-effects-sub-grid-scale-processes.pdf), including the neutral-lapse-rate and pressure-integrated enthalpy constraints for dry adjustment.
- World Meteorological Organization, [Guide to Meteorological Instruments and Methods of Observation](https://www.weather.gov/media/epz/mesonet/CWOP-WMO8.pdf), humidity, dewpoint, frost-point, and subfreezing RH conventions.
- NOAA/NWS, [Mixing Ratio](https://www.weather.gov/media/epz/wxcalc/mixingRatio.pdf), and UCAR, [CEOP Derived Parameter Equations](https://archive.eol.ucar.edu/projects/ceop/dm/documents/refdata_report/eqns.html), for vapor-pressure, mixing-ratio, specific-humidity, and dewpoint transformations.
- NCAR, [Wind component](https://www.ncl.ucar.edu/Document/Functions/Contributed/wind_component.shtml), for the meteorological speed/direction to `u/v` convention.
- NOAA MADIS, [Radiosonde quality-control checks](https://madis.ncep.noaa.gov/madis_raob_qc_notes.shtml), for operational superadiabatic, hydrostatic, and wind-shear QC context.
