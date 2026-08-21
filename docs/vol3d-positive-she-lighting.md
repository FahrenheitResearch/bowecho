# BowEcho 3D positive-energy spherical-harmonic lighting

## Status

This document describes the implementation added to the native BowEcho 3D radar explorer. It is a visualization pipeline, not a radar retrieval or cloud-radiative-transfer product.

## Motivation

The previous fragment shader estimated a gradient with six 3D texture reads for every visible ray sample, treated that gradient as a normal, lit it with a hard-coded direction, and used `abs(dot(N, L))`. That had three costs:

1. Six extra source-volume reads were paid repeatedly inside the ray marcher.
2. `abs(N dot L)` made both sides of a structure respond as front-facing, weakening depth cues.
3. The ambient term was not an environment representation and could not be expanded cleanly.

The replacement separates infrequent lighting construction from frequent volume rendering.

## Positive log-SH ambient

The ambient irradiance is represented as

```text
E(n) = exp(sum_i c_i Y_i(n))
```

where `Y_i` are the 16 real, orthonormal spherical-harmonic basis functions through degree 3.

The coefficients are fitted by least squares in log space:

```text
argmin_c sum_j (Y(n_j) c - log(max(E_j, epsilon)))^2
         + lambda * sum_(l,m) [l(l+1)]^2 c_(l,m)^2
```

The degree-weighted term damps unstable high-order coefficients. After the log fit, the constant coefficient is adjusted so the mean reconstructed linear irradiance matches the mean target irradiance.

This follows the critical positive-energy rule from spherical harmonic exponentials: fit the logarithm and exponentiate the truncated expansion. The reconstruction therefore stays positive in every direction without clamping negative SH lobes.

The direct key light is intentionally not encoded into low-order SH. It remains an analytic directional term, because a sharp source should not be blurred into a low-order environment basis.

## Presets

- **Flat**: exactly uniform unit irradiance. Useful for diagnostics.
- **Operational**: broad sky/fill response with restrained directional variation.
- **Sculpted**: stronger sky-ground separation for presentation and structure inspection.

The presets are fitted once through `OnceLock`. The fitter remains in-tree and is covered by numerical unit tests rather than hiding opaque hand-authored coefficients.

## Coordinate convention

BowEcho's volume world axes are:

```text
+X east
+Y north
+Z up
```

Key-light azimuth is meteorological: clockwise from north.

The default azimuth/elevation (`140 degrees`, `51 degrees`) reproduces the previous normalized vector `(0.40, -0.48, 0.78)` to within a small angular difference while exposing it as a user control.

## Compute light cache

A compute pass writes a `96 x 96 x 24` `Rgba8Unorm` texture.

```text
RGB = outward normal encoded from [-1, 1] to [0, 1]
A   = scalar illumination / LIGHTING_ENCODE_MAX
```

Memory is:

```text
96 * 96 * 24 * 4 = 884,736 bytes
```

The fragment ray marcher samples this cache once and normalizes the interpolated normal before adding the view-dependent rim term. Final illumination is scalar, so the reflectivity or velocity palette retains its hue and only brightness changes.

## Normal construction

The compute shader samples six source neighbors once per cache voxel.

- At a transfer boundary, the gradient of thresholded occupancy determines which direction points into the visible body.
- In a saturated interior, where smoothstep occupancy is flat, the raw source-field gradient is used with a mode-dependent sign.
- The outward normal is the negative of the into-body gradient.
- Above, Below, and Outside transfer modes are handled explicitly.
- Velocity mode always uses the reflectivity structure texture for the normal.
- A locally homogeneous field emits a zero-normal sentinel and unit lighting, so it preserves the former unlit palette behavior instead of inventing an upward-facing surface.

This removes the previous need for `abs(N dot L)`.

## Self-shadowing

For front-facing cache voxels, the compute shader marches from the sample toward the key light until the volume boundary. It integrates thresholded visual occupancy:

```text
tau_visual += occupancy * step_distance * shadow_density
T_visual = exp(-tau_visual)
```

The key term is

```text
key = key_strength * max(N dot L, 0)
      * mix(1, T_visual, shadow_strength)
```

This is deliberately named pseudo-optical or visual occupancy shadowing. NEXRAD reflectivity is not visible-light extinction, so the result must never be presented as physical cloud optical depth.

## Cache invalidation

The cached light volume is keyed from:

- the generation of the structure texture actually uploaded;
- transfer threshold low/high and mode;
- velocity mode and reflectivity structure gate;
- z-span;
- SH preset;
- key azimuth/elevation;
- ambient and key strength;
- shadow blend, density, and step count.

It is not keyed from:

- camera;
- FOV;
- opacity;
- volume-render density;
- final shading blend;
- rim strength;
- LUT/color-table changes;
- floor controls.

The upload generation is incremented in the GPU callback only after `pending.volume` is actually drained. This avoids incorrectly treating a UI request key as proof that the matching texture has reached the GPU.

When lighting is disabled or the final shading blend is zero, no new compute dispatch is performed. A later re-enable still rebuilds if a volume was uploaded or any compute input changed while lighting was bypassed.

## Render path

The ray marcher retains the existing transfer functions, alpha correction, front-to-back compositing, floor behavior, quality settings, and velocity two-box path.

For a visible ray sample it now performs:

1. Existing structure/transfer and palette work.
2. One trilinear light-cache sample.
3. Decode and normalize the cached normal.
4. Add the fragment-only view rim.
5. Multiply palette RGB by one scalar lighting value.

The six central-difference reads formerly inside `shaded_rgb` are no longer repeated for every ray sample.

## Files

```text
crates/app_ui/src/vol3d.rs
crates/app_ui/src/vol3d/lighting.rs
crates/app_ui/src/vol3d/light_volume.wgsl
crates/app_ui/src/main.rs
docs/vol3d-positive-she-lighting.md
```

No Cargo dependency is added. Existing `naga` dev-dependency coverage is expanded to validate both WGSL modules headlessly.

## References and attribution

The mathematical design is inspired by the positive log-space representation demonstrated in:

- https://github.com/Avelina9X/SphericalHarmonicExponentials
- https://diglib.eg.org/items/581aa76a-4ecb-4f4e-a8aa-30fbc814706d

This BowEcho implementation was written independently around the existing wgpu volume renderer; it does not copy the Direct3D shader implementation. If source from the reference repository is copied later, retain its MIT license notice.
