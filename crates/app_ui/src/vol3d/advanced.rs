//! BowEcho 3D second-generation acceleration and transfer preparation.
//!
//! This module is deliberately CPU-side. The Cartesian reconstruction already
//! runs on a worker; building the tiny conservative hierarchy there avoids
//! storage-texture feature gates and makes the renderer portable across every
//! wgpu backend. The hierarchy is uploaded once and consumed by the GPU ray
//! traversal. No meteorological field is changed by this module.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub const FINE_BRICK_X: usize = 8;
pub const FINE_BRICK_Y: usize = 8;
pub const FINE_BRICK_Z: usize = 8;
pub const FINE_X: usize = super::BOX_N / FINE_BRICK_X;
pub const FINE_Y: usize = super::BOX_N / FINE_BRICK_Y;
pub const FINE_Z: usize = super::BOX_NZ / FINE_BRICK_Z;

pub const COARSE_GROUP_X: usize = 4;
pub const COARSE_GROUP_Y: usize = 4;
pub const COARSE_GROUP_Z: usize = 3;
pub const COARSE_X: usize = FINE_X / COARSE_GROUP_X;
pub const COARSE_Y: usize = FINE_Y / COARSE_GROUP_Y;
pub const COARSE_Z: usize = FINE_Z.div_ceil(COARSE_GROUP_Z);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dRenderMode {
    DirectVolume,
    HybridShell,
    Isosurface,
    MaximumProjection,
    OrthogonalSlices,
    SupportInspection,
}

impl Vol3dRenderMode {
    pub const ALL: [Self; 6] = [
        Self::DirectVolume,
        Self::HybridShell,
        Self::Isosurface,
        Self::MaximumProjection,
        Self::OrthogonalSlices,
        Self::SupportInspection,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::DirectVolume => "Direct volume",
            Self::HybridShell => "Hybrid shell + volume",
            Self::Isosurface => "Isosurface",
            Self::MaximumProjection => "Maximum projection",
            Self::OrthogonalSlices => "Orthogonal slices",
            Self::SupportInspection => "Beam-support inspection",
        }
    }

    pub fn shader_value(self) -> f32 {
        match self {
            Self::DirectVolume => 0.0,
            Self::HybridShell => 1.0,
            Self::Isosurface => 2.0,
            Self::MaximumProjection => 3.0,
            Self::OrthogonalSlices => 4.0,
            Self::SupportInspection => 5.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportMode {
    HonestFade,
    FullReconstruction,
    Inspect,
}

impl SupportMode {
    pub const ALL: [Self; 3] = [Self::HonestFade, Self::FullReconstruction, Self::Inspect];

    pub fn label(self) -> &'static str {
        match self {
            Self::HonestFade => "Fade weak support",
            Self::FullReconstruction => "Show full reconstruction",
            Self::Inspect => "Color by support",
        }
    }

    pub fn shader_value(self) -> f32 {
        match self {
            Self::HonestFade => 0.0,
            Self::FullReconstruction => 1.0,
            Self::Inspect => 2.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct VolumeAcceleration {
    pub support: Vec<u8>,
    /// RGBA = minimum field, maximum field, minimum support, maximum support.
    /// Each range covers the brick's `read_window`, not the brick, so it bounds
    /// every value the trilinear sampler can return inside the brick.
    pub fine_minmax: Vec<u8>,
    /// Same encoding, conservatively aggregated from the fine hierarchy.
    pub coarse_minmax: Vec<u8>,
    /// Share of fine bricks with no support anywhere in their read window, i.e.
    /// bricks the traversal can skip outright. Reported as occupancy telemetry.
    pub empty_fine_fraction: f32,
}

fn index3(x: usize, y: usize, z: usize, nx: usize, ny: usize) -> usize {
    z * nx * ny + y * nx + x
}

/// Half-open lattice range a trilinear read can touch while the sample point
/// is anywhere inside brick `brick` along one axis.
///
/// The volume, color and support textures are all sampled with
/// `FilterMode::Linear` + `AddressMode::ClampToEdge` (see `vol3d.rs`,
/// `vol3d-sampler`). For a sample at normalized coordinate `c` inside brick
/// `b`, i.e. `c` in `[b*W/DIM, (b+1)*W/DIM)`, the sampler reads texels
/// `floor(c*DIM - 0.5)` and that plus one, so the reachable indices run from
/// `b*W - 1` to `b*W + W`. The brick's OWN `W` voxels are therefore not a
/// bound: the window is the brick dilated by one voxel on each side, clamped
/// to the lattice exactly as ClampToEdge clamps.
///
/// This is the standard requirement on min/max spatial hierarchies over an
/// interpolated field, not a BowEcho refinement: Wilhelms & Van Gelder,
/// "Octrees for faster isosurface generation", ACM TOG 11(3), 1992, 201-227,
/// build their octree over cells whose corners are SHARED between neighbours
/// for exactly this reason, and Knoll, Wald, Parker & Hansen, "Interactive
/// isosurface ray tracing of large octree volumes", IEEE Symposium on
/// Interactive Ray Tracing 2006, 115-124, restate it for ray-traced
/// interpolated isosurfaces. Gathering over the disjoint brick instead lets a
/// brick be skipped while a sample inside it still reads a neighbour's voxel
/// through the filter - which for a forecaster means a storm can vanish.
fn read_window(brick: usize, brick_width: usize, dim: usize) -> (usize, usize) {
    let low = (brick * brick_width).saturating_sub(1);
    let high = ((brick + 1) * brick_width + 1).min(dim);
    (low, high.max(low))
}

fn pack_range(dst: &mut [u8], index: usize, min_v: u8, max_v: u8, min_s: u8, max_s: u8) {
    let offset = index * 4;
    dst[offset] = min_v;
    dst[offset + 1] = max_v;
    dst[offset + 2] = min_s;
    dst[offset + 3] = max_s;
}

pub fn build_acceleration(
    normalized: &[u8],
    support_input: &[u8],
    n: usize,
    nz: usize,
) -> VolumeAcceleration {
    let expected = n.saturating_mul(n).saturating_mul(nz);
    let mut support = vec![0u8; expected];
    for (dst, src) in support.iter_mut().zip(support_input.iter().copied()) {
        *dst = src;
    }

    debug_assert_eq!(n, super::BOX_N);
    debug_assert_eq!(nz, super::BOX_NZ);
    let mut fine = vec![0u8; FINE_X * FINE_Y * FINE_Z * 4];
    let mut empty = 0usize;
    for fz in 0..FINE_Z {
        let (z0, z1) = read_window(fz, FINE_BRICK_Z, nz);
        for fy in 0..FINE_Y {
            let (y0, y1) = read_window(fy, FINE_BRICK_Y, n);
            for fx in 0..FINE_X {
                let (x0, x1) = read_window(fx, FINE_BRICK_X, n);
                let mut min_v = u8::MAX;
                let mut max_v = 0u8;
                let mut min_s = u8::MAX;
                let mut max_s = 0u8;
                let mut supported = false;
                for z in z0..z1 {
                    for y in y0..y1 {
                        for x in x0..x1 {
                            let i = index3(x, y, z, n, n);
                            let s = support.get(i).copied().unwrap_or(0);
                            // Every texel in the read window bounds the value,
                            // INCLUDING no-data. No-data is stored as 0 in the
                            // R8 volume, and the linear sampler blends that 0
                            // into any read within half a voxel of it, so a
                            // min taken only over supported voxels is not a
                            // lower bound on what the shader actually sees.
                            // That matters for Below/Outside, whose skip test
                            // is `min >= threshold`.
                            let v = normalized.get(i).copied().unwrap_or(0);
                            min_v = min_v.min(v);
                            max_v = max_v.max(v);
                            min_s = min_s.min(s);
                            max_s = max_s.max(s);
                            supported |= s != 0;
                        }
                    }
                }
                if !supported {
                    // Nothing in the read window is observed, so every sample
                    // inside this brick fails the shader's per-sample
                    // `support <= 0.0001` gate. Publish an all-zero range:
                    // `range.a <= 0.0001` makes the traversal skip it outright.
                    min_v = 0;
                    max_v = 0;
                    min_s = 0;
                    max_s = 0;
                    empty += 1;
                }
                let oi = index3(fx, fy, fz, FINE_X, FINE_Y);
                pack_range(&mut fine, oi, min_v, max_v, min_s, max_s);
            }
        }
    }

    let mut coarse = vec![0u8; COARSE_X * COARSE_Y * COARSE_Z * 4];
    for cz in 0..COARSE_Z {
        for cy in 0..COARSE_Y {
            for cx in 0..COARSE_X {
                let mut min_v = u8::MAX;
                let mut max_v = 0u8;
                let mut min_s = u8::MAX;
                let mut max_s = 0u8;
                let mut observed = false;
                for lz in 0..COARSE_GROUP_Z {
                    let z = cz * COARSE_GROUP_Z + lz;
                    if z >= FINE_Z {
                        break;
                    }
                    for ly in 0..COARSE_GROUP_Y {
                        let y = cy * COARSE_GROUP_Y + ly;
                        for lx in 0..COARSE_GROUP_X {
                            let x = cx * COARSE_GROUP_X + lx;
                            let i = index3(x, y, z, FINE_X, FINE_Y) * 4;
                            let child_max_s = fine[i + 3];
                            // A child with no support anywhere in its own
                            // (already dilated) read window cannot contribute:
                            // every sample inside it reads support 0 and is
                            // rejected by the shader's per-sample gate. Folding
                            // its all-zero range in would only widen the coarse
                            // interval and lose skips, so leave it out. The
                            // coarse cell stays conservative because each
                            // remaining child's range already covers the reads
                            // of every sample that lands in that child.
                            if child_max_s == 0 {
                                continue;
                            }

                            min_v = min_v.min(fine[i]);
                            max_v = max_v.max(fine[i + 1]);
                            min_s = min_s.min(fine[i + 2]);
                            max_s = max_s.max(child_max_s);
                            observed = true;
                        }
                    }
                }
                if !observed {
                    min_v = 0;
                    min_s = 0;
                }
                let oi = index3(cx, cy, cz, COARSE_X, COARSE_Y);
                pack_range(&mut coarse, oi, min_v, max_v, min_s, max_s);
            }
        }
    }

    VolumeAcceleration {
        support,
        fine_minmax: fine,
        coarse_minmax: coarse,
        empty_fine_fraction: empty as f32 / (FINE_X * FINE_Y * FINE_Z) as f32,
    }
}

/// WGSL `smoothstep`, so the CPU preintegration table and the shader agree.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(1.0e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Exact CPU mirror of `threshold_strength` in the `SHADER` WGSL (vol3d.rs).
///
/// It must stay branch-for-branch and curve-for-curve identical, because
/// `build_preintegrated_lut` bakes this ramp into the table the direct-volume
/// path reads when preintegration is ON, while the same path evaluates the
/// WGSL version when it is OFF. Any divergence means toggling one checkbox
/// silently changes how much opacity a given dBZ gets right at the
/// forecaster's chosen threshold. The bundle shipped a LINEAR ramp here
/// against the shader's `smoothstep`, which differed by up to 0.1 of the
/// transfer across the whole 0.08-wide band around the threshold.
///
/// The `-1.0` sentinel for "no contribution" is the shader's, kept so the two
/// bodies diff cleanly; `build_preintegrated_lut` clamps it back to 0.
fn threshold_strength(value: f32, low: f32, high: f32, mode: f32, width: f32) -> f32 {
    if mode > 1.5 {
        if value <= low {
            return smoothstep(0.0, width, low - value);
        }
        if value >= high {
            return smoothstep(0.0, width, value - high);
        }
        return -1.0;
    }
    if mode > 0.5 {
        if value >= low {
            return -1.0;
        }
        return smoothstep(0.0, width, low - value);
    }
    if value <= low {
        return -1.0;
    }
    smoothstep(low, low + width, value)
}

fn lut_sample(lut: &[u8], value: f32) -> [f32; 4] {
    if lut.len() < 256 * 4 {
        return [0.0; 4];
    }
    let x = value.clamp(0.0, 1.0) * 255.0;
    let i0 = x.floor() as usize;
    let i1 = (i0 + 1).min(255);
    let t = x - i0 as f32;
    let mut out = [0.0f32; 4];
    for channel in 0..4 {
        let a = lut[i0 * 4 + channel] as f32 / 255.0;
        let b = lut[i1 * 4 + channel] as f32 / 255.0;
        out[channel] = a + (b - a) * t;
    }
    out
}

/// Build a 256x256 segment-preintegration table. RGB is stored straight-alpha;
/// alpha is the reference-segment opacity. The shader applies Beer-Lambert
/// step correction for the actual ray step and density.
pub fn build_preintegrated_lut(
    lut: &[u8],
    threshold_low: f32,
    threshold_high: f32,
    threshold_mode: f32,
    opacity: f32,
) -> Vec<u8> {
    const SUBSTEPS: usize = 16;
    let mut out = vec![0u8; 256 * 256 * 4];
    for start in 0..256usize {
        for end in 0..256usize {
            let mut color = [0.0f32; 3];
            let mut accumulated = 0.0f32;
            for step in 0..SUBSTEPS {
                let t = (step as f32 + 0.5) / SUBSTEPS as f32;
                let value = (start as f32 + (end as f32 - start as f32) * t) / 255.0;
                let palette = lut_sample(lut, value);
                let transfer =
                    threshold_strength(value, threshold_low, threshold_high, threshold_mode, 0.08);
                let raw = (palette[3] * opacity * transfer).clamp(0.0, 0.9999);
                let alpha = 1.0 - (1.0 - raw).powf(1.0 / SUBSTEPS as f32);
                for c in 0..3 {
                    color[c] += (1.0 - accumulated) * alpha * palette[c];
                }
                accumulated += (1.0 - accumulated) * alpha;
            }
            let oi = (start * 256 + end) * 4;
            if accumulated > 1.0e-6 {
                for c in 0..3 {
                    out[oi + c] = ((color[c] / accumulated).clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
            out[oi + 3] = (accumulated.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    out
}

pub fn preintegration_signature(
    lut: &[u8],
    threshold_low: f32,
    threshold_high: f32,
    threshold_mode: f32,
    opacity: f32,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    lut.hash(&mut hasher);
    threshold_low.to_bits().hash(&mut hasher);
    threshold_high.to_bits().hash(&mut hasher);
    threshold_mode.to_bits().hash(&mut hasher);
    opacity.to_bits().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::MomentType;

    const N: usize = super::super::BOX_N;
    const NZ: usize = super::super::BOX_NZ;

    /// Texel window a trilinear read can touch while the sample point is
    /// anywhere inside fine brick (fx, fy, fz). Written out longhand so the
    /// test does not simply re-assert `read_window` against itself.
    fn fine_read_window(fx: usize, fy: usize, fz: usize) -> [(usize, usize); 3] {
        let axis = |brick: usize, width: usize, dim: usize| {
            let low = if brick == 0 { 0 } else { brick * width - 1 };
            let high = ((brick + 1) * width + 1).min(dim);
            (low, high)
        };
        [
            axis(fx, FINE_BRICK_X, N),
            axis(fy, FINE_BRICK_Y, N),
            axis(fz, FINE_BRICK_Z, NZ),
        ]
    }

    /// Brute-force (min value, max value, max support) over a texel window.
    fn window_extremes(data: &[u8], support: &[u8], window: [(usize, usize); 3]) -> (u8, u8, u8) {
        let mut min_v = u8::MAX;
        let mut max_v = 0u8;
        let mut max_s = 0u8;
        for z in window[2].0..window[2].1 {
            for y in window[1].0..window[1].1 {
                for x in window[0].0..window[0].1 {
                    let i = index3(x, y, z, N, N);
                    min_v = min_v.min(data[i]);
                    max_v = max_v.max(data[i]);
                    max_s = max_s.max(support[i]);
                }
            }
        }
        (min_v, max_v, max_s)
    }

    /// Rust mirror of the WGSL `range_can_contribute`, direct-volume branch.
    /// `mode` uses the shader encoding: 0 Above, 1 Below, 2 Outside.
    fn can_contribute(range: &[u8], mode: f32, low: f32, high: f32) -> bool {
        if range[3] as f32 / 255.0 <= 0.0001 {
            return false;
        }
        let min_v = range[0] as f32 / 255.0;
        let max_v = range[1] as f32 / 255.0;
        if mode > 1.5 {
            return min_v < low || max_v > high;
        }
        if mode > 0.5 {
            return min_v < low;
        }
        max_v > low
    }

    /// The contract, stated as an assertion: a brick the traversal skips must
    /// be unable to contribute. Sweeps every representable normalized value in
    /// the brick's reachable interval, because bounding the interval is only
    /// half the proof - the transfer function has to be zero across all of it.
    fn assert_skip_is_safe(
        label: &str,
        reachable: (u8, u8, u8),
        range: &[u8],
        mode: f32,
        low: f32,
        high: f32,
    ) {
        if reachable.2 == 0 || can_contribute(range, mode, low, high) {
            return;
        }
        for raw in reachable.0..=reachable.1 {
            let value = f32::from(raw) / 255.0;
            let transfer = threshold_strength(value, low, high, mode, 0.08);
            assert!(
                transfer <= 0.0,
                "{label}: skipped brick can still paint. mode={mode} low={low} high={high} \
                 reachable=[{}, {}] offending value={raw} transfer={transfer}",
                reachable.0,
                reachable.1
            );
        }
    }

    /// Whole-hierarchy check: every published range must bound every texel the
    /// GPU can read from inside that brick, and every skip the shader would
    /// take under Above, Below or Outside must be provably empty.
    ///
    /// Returns how many (brick, threshold) pairs stay skippable per mode, out
    /// of `FINE_X * FINE_Y * FINE_Z * probes.len()`. Correctness is free to
    /// cost skips - a conservative min over a window that includes no-data is
    /// 0 wherever a brick touches the edge of the echo, which is most of them -
    /// so the numbers matter when reading a GPU capture.
    fn assert_hierarchy_bounds_reads(
        label: &str,
        data: &[u8],
        accel: &VolumeAcceleration,
    ) -> [usize; 3] {
        let support = &accel.support;
        // Normalized threshold pairs straddling realistic operating points: a
        // low gate, the 35 dBZ-ish default, and a core-isolation gate.
        let probes: [(f32, f32); 3] = [(0.20, 0.85), (0.45, 0.90), (0.62, 0.95)];
        let mut skippable = [0usize; 3];
        for fz in 0..FINE_Z {
            for fy in 0..FINE_Y {
                for fx in 0..FINE_X {
                    let reachable = window_extremes(data, support, fine_read_window(fx, fy, fz));
                    let oi = index3(fx, fy, fz, FINE_X, FINE_Y) * 4;
                    let range = &accel.fine_minmax[oi..oi + 4];
                    if range[3] == 0 {
                        // Published as unreachable. Prove it really is.
                        assert_eq!(
                            reachable.2, 0,
                            "{label}: fine brick ({fx},{fy},{fz}) published max support 0 but a \
                             read inside it reaches support {}",
                            reachable.2
                        );
                        for count in &mut skippable {
                            *count += probes.len();
                        }
                        continue;
                    }
                    assert!(
                        range[0] <= reachable.0
                            && range[1] >= reachable.1
                            && range[3] >= reachable.2,
                        "{label}: fine brick ({fx},{fy},{fz}) range [{}, {}] support<={} does not \
                         bound its read window [{}, {}] support<={}",
                        range[0],
                        range[1],
                        range[3],
                        reachable.0,
                        reachable.1,
                        reachable.2
                    );
                    for (low, high) in probes {
                        for (slot, mode) in [0.0f32, 1.0, 2.0].into_iter().enumerate() {
                            if !can_contribute(range, mode, low, high) {
                                skippable[slot] += 1;
                            }
                            assert_skip_is_safe(label, reachable, range, mode, low, high);
                        }
                    }
                }
            }
        }

        // Coarse cells aggregate fine children, and a coarse skip skips all of
        // them at once, so every child that can contribute must be bounded by
        // its parent.
        for cz in 0..COARSE_Z {
            for cy in 0..COARSE_Y {
                for cx in 0..COARSE_X {
                    let ci = index3(cx, cy, cz, COARSE_X, COARSE_Y) * 4;
                    let parent = &accel.coarse_minmax[ci..ci + 4];
                    for lz in 0..COARSE_GROUP_Z {
                        let fz = cz * COARSE_GROUP_Z + lz;
                        if fz >= FINE_Z {
                            break;
                        }
                        for ly in 0..COARSE_GROUP_Y {
                            let fy = cy * COARSE_GROUP_Y + ly;
                            for lx in 0..COARSE_GROUP_X {
                                let fx = cx * COARSE_GROUP_X + lx;
                                let reachable =
                                    window_extremes(data, support, fine_read_window(fx, fy, fz));
                                if reachable.2 == 0 {
                                    continue;
                                }
                                assert!(
                                    parent[0] <= reachable.0
                                        && parent[1] >= reachable.1
                                        && parent[3] >= reachable.2,
                                    "{label}: coarse cell ({cx},{cy},{cz}) range [{}, {}] \
                                     support<={} does not bound child ({fx},{fy},{fz}) read \
                                     window [{}, {}] support<={}",
                                    parent[0],
                                    parent[1],
                                    parent[3],
                                    reachable.0,
                                    reachable.1,
                                    reachable.2
                                );
                            }
                        }
                    }
                }
            }
        }

        skippable
    }

    #[test]
    fn hierarchy_is_conservative() {
        let mut data = vec![0u8; N * N * NZ];
        let mut support = vec![0u8; data.len()];
        let i = index3(17, 25, 9, N, N);
        data[i] = 231;
        support[i] = 207;
        let accel = build_acceleration(&data, &support, N, NZ);
        let fi = index3(17 / 8, 25 / 8, 9 / 8, FINE_X, FINE_Y) * 4;
        assert!(accel.fine_minmax[fi] <= 231);
        assert!(accel.fine_minmax[fi + 1] >= 231);
        assert!(accel.fine_minmax[fi + 3] >= 207);
    }

    /// The neighbour case a disjoint-brick gather gets wrong. Voxel (16,24,8)
    /// is the first voxel of brick (2,3,1); a sample just inside the low face
    /// of that brick still reads it through the linear filter, so the adjacent
    /// bricks have to report it too or they can be skipped while a sample
    /// inside them paints it.
    #[test]
    fn hierarchy_bounds_neighbour_faces() {
        let mut data = vec![0u8; N * N * NZ];
        let mut support = vec![0u8; data.len()];
        let i = index3(16, 24, 8, N, N);
        data[i] = 240;
        support[i] = 200;
        let accel = build_acceleration(&data, &support, N, NZ);
        for (fx, fy, fz) in [(2, 3, 1), (1, 3, 1), (2, 2, 1), (2, 3, 0), (1, 2, 0)] {
            let oi = index3(fx, fy, fz, FINE_X, FINE_Y) * 4;
            assert!(
                accel.fine_minmax[oi + 1] >= 240,
                "brick ({fx},{fy},{fz}) max {} misses the neighbouring voxel a linear read reaches",
                accel.fine_minmax[oi + 1]
            );
            assert!(
                accel.fine_minmax[oi + 3] >= 200,
                "brick ({fx},{fy},{fz}) max support {} misses the neighbouring voxel",
                accel.fine_minmax[oi + 3]
            );
        }
        assert_hierarchy_bounds_reads("synthetic neighbour", &data, &accel);
    }

    /// Real-data proof of contract 3. Ignored by default because it needs
    /// archive files that are not in the repository; point `BOWECHO_VOL3D_L2`
    /// at one or more `;`-separated Level II volumes and run
    ///
    /// ```text
    /// cargo test --release -p app_ui -- --ignored hierarchy_bounds_real_level2 --nocapture
    /// ```
    ///
    /// Synthetic data does not stand in for this. The failure mode is a brick
    /// whose interior sits below threshold while the voxel one step across its
    /// face is a storm core, which is what a real reflectivity gradient looks
    /// like and what a uniform random fill does not reliably produce.
    #[test]
    #[ignore = "needs local Level II volumes in BOWECHO_VOL3D_L2"]
    fn hierarchy_bounds_real_level2() {
        let list = std::env::var("BOWECHO_VOL3D_L2")
            .expect("set BOWECHO_VOL3D_L2 to one or more ';'-separated Level II paths");
        let mut checked = 0usize;
        for path in list.split(';').filter(|entry| !entry.trim().is_empty()) {
            let path = path.trim();
            let bytes = std::fs::read(path).expect("read Level II file");
            let volume =
                nexrad_io::decode_supported_volume_bytes(&bytes).expect("decode Level II volume");
            for (moment, policy, value_min, value_max) in [
                (
                    MomentType::Reflectivity,
                    render2d::InterpPolicy::LinearAngle,
                    -32.0f32,
                    95.0f32,
                ),
                (
                    MomentType::Velocity,
                    render2d::InterpPolicy::VelocityGuard,
                    -100.0,
                    100.0,
                ),
            ] {
                let Some(resample) = render2d::volume_box_resample_moment_with_support(
                    &volume,
                    &moment,
                    policy,
                    0.0,
                    0.0,
                    super::super::BOX_HALF_KM,
                    N,
                    NZ,
                    super::super::BOX_TOP_M,
                ) else {
                    continue;
                };
                let data = super::super::normalize_values(&resample.values, value_min, value_max);
                let accel = build_acceleration(&data, &resample.support, N, NZ);
                let observed = resample.support.iter().filter(|s| **s > 0).count();
                assert!(
                    observed > 10_000,
                    "{path} {moment:?}: only {observed} observed voxels, too sparse to prove \
                     anything"
                );
                let label = format!("{path} {moment:?}");
                let skippable = assert_hierarchy_bounds_reads(&label, &data, &accel);
                let pairs = (FINE_X * FINE_Y * FINE_Z * 3) as f32;
                println!(
                    "{label}: {observed} observed voxels, {:.1}% occupied bricks; still \
                     skippable Above {:.1}% / Below {:.1}% / Outside {:.1}% - every published \
                     range bounds every readable voxel, and every skip under Above, Below and \
                     Outside is provably empty",
                    (1.0 - accel.empty_fine_fraction) * 100.0,
                    skippable[0] as f32 / pairs * 100.0,
                    skippable[1] as f32 / pairs * 100.0,
                    skippable[2] as f32 / pairs * 100.0
                );
                checked += 1;
            }
        }
        assert!(checked > 0, "no volume resampled; nothing was proved");
    }

    #[test]
    fn preintegration_stays_bounded() {
        let mut lut = vec![0u8; 256 * 4];
        for i in 0..256 {
            lut[i * 4] = i as u8;
            lut[i * 4 + 1] = i as u8;
            lut[i * 4 + 2] = i as u8;
            lut[i * 4 + 3] = 255;
        }
        let table = build_preintegrated_lut(&lut, 0.2, 0.8, 0.0, 0.5);
        assert_eq!(table.len(), 256 * 256 * 4);
        assert!(table.iter().any(|value| *value > 0));
    }
}
