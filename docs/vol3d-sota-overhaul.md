# BowEcho 3D second-generation renderer

This stage retains the compute-cached positive log-SH lighting implementation
and replaces uniform blind sampling with a hierarchy-aware scientific volume
renderer.

## Data contract

The original physical field remains the source of color and transfer. The new
support field is a display-support diagnostic derived from the reconstruction
geometry. It is **not** official radar QC and it does not estimate calibration,
attenuation, blockage, contamination, or algorithmic uncertainty.

The resampler now returns:

- physical values;
- beam-stack support in `0..255`;
- no-data as support `0`.

## GPU-resident acceleration

The existing `192 x 192 x 48` field is retained. The worker builds two
conservative min/max/support levels:

- `24 x 24 x 6`, one texel per `8 x 8 x 8` source brick;
- `6 x 6 x 2`, aggregating `4 x 4 x 3` fine bricks.

Each texel bounds the brick's READ WINDOW, not the brick: the volume, colour
and support textures are sampled with linear filtering, so a sample a hair
inside a brick face interpolates the neighbouring brick's voxel. The gather is
therefore dilated by one voxel per axis and includes no-data (stored as `0`),
which is what makes the `min >= threshold` skip in Below/Outside sound. This is
the same requirement Wilhelms & Van Gelder (ACM TOG 11(3), 1992) meet by
building their octree over cells with shared corners, and Knoll, Wald, Parker &
Hansen (IEEE Symp. Interactive Ray Tracing 2006) restate for ray-traced
interpolated isosurfaces. Gathering over disjoint bricks instead was measured
here to under-bound 1129-2511 of 3456 bricks on real KDMX/KABR/KTLX volumes.

The ray traverser first tests the macro hierarchy, then the fine hierarchy, and
jumps directly to the next cell boundary when the active transfer function
cannot possibly contribute. Conservative min/max bounds mean the optimization
must not remove visible data.

## Render modes

- Direct volume rendering
- Hybrid illuminated shell plus volume interior
- Refined first-hit isosurface
- Maximum-intensity projection
- Three orthogonal slices
- Beam-support inspection

Velocity keeps the existing two-box contract: reflectivity determines geometry,
opacity, hierarchy and support; signed velocity determines color.

## Integration quality

Inside occupied bricks the renderer uses adaptive steps, a stable per-pixel
sub-voxel offset, Beer-Lambert opacity correction, and an optional 256x256
segment-preintegrated transfer table. The preintegrated path is disabled for
velocity two-box rendering because its structure and color fields differ.

## Operational defaults

The default is direct volume rendering with weak-support fading. Full
reconstruction remains available, but the UI makes the distinction explicit.
The support inspection mode is intended to reveal the beam/interpolation
anatomy behind apparent 3D structures.
