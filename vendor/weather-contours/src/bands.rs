//! Critical Ordinal Band-Rail Mesh (COBRM).
//!
//! Every finite bilinear cell is decomposed into a four-triangle fan. Saddle
//! cells place the fan center at the exact bilinear critical point; other cells
//! use the bilinear center. Triangle-band clipping then emits indexed geometry.
//! Crossings are keyed by `(integer rail, level ordinal)`, so adjacent cells and
//! fan triangles share vertices without hashing floating-point coordinates.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::{
    check_limit, normalize_levels_checked, validate_inputs_with_limits, ContourError, ContourLimits,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BandMeshStats {
    pub grid_points: usize,
    pub cells_total: usize,
    pub cells_finite: usize,
    pub cells_skipped_non_finite: usize,
    pub critical_saddle_cells: usize,
    pub fan_triangles: usize,
    pub band_polygons: usize,
    pub output_vertices: usize,
    pub output_triangles: usize,
}

/// Indexed interior isobands. Band `k` represents `[levels[k], levels[k + 1])`;
/// only the final band includes its upper bound. `triangle_band_indices` has
/// one ordinal per index triplet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackedBands {
    pub levels: Vec<f32>,
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    pub triangle_band_indices: Vec<u32>,
    pub stats: BandMeshStats,
}

impl PackedBands {
    pub fn vertex_count(&self) -> usize {
        self.vertices.len() / 2
    }

    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    pub fn validate(&self) -> Result<(), BandError> {
        if self.levels.len() < 2 {
            return Err(BandError::Invariant("isobands require at least two levels"));
        }
        if !self.vertices.len().is_multiple_of(2) {
            return Err(BandError::Invariant("vertices are not x/y pairs"));
        }
        if self
            .vertices
            .iter()
            .any(|coordinate| !coordinate.is_finite())
        {
            return Err(BandError::Invariant(
                "vertices contain a non-finite coordinate",
            ));
        }
        if !self.indices.len().is_multiple_of(3) {
            return Err(BandError::Invariant("indices are not triangle triplets"));
        }
        if self.triangle_band_indices.len() != self.triangle_count() {
            return Err(BandError::Invariant(
                "triangle band ordinals do not match triangle count",
            ));
        }
        if self
            .indices
            .iter()
            .any(|&index| index as usize >= self.vertex_count())
        {
            return Err(BandError::Invariant("triangle references a missing vertex"));
        }
        let band_count = self.levels.len() - 1;
        if self
            .triangle_band_indices
            .iter()
            .any(|&band| band as usize >= band_count)
        {
            return Err(BandError::Invariant("triangle references a missing band"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BandError {
    Contour(ContourError),
    TooFewLevels,
    TooManyVertices(u64),
    Invariant(&'static str),
}

impl Display for BandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contour(error) => Display::fmt(error, formatter),
            Self::TooFewLevels => write!(formatter, "isobands require at least two finite levels"),
            Self::TooManyVertices(count) => {
                write!(
                    formatter,
                    "isoband mesh has too many vertices for u32 indices: {count}"
                )
            }
            Self::Invariant(message) => write!(formatter, "isoband invariant failed: {message}"),
        }
    }
}

impl Error for BandError {}

impl From<ContourError> for BandError {
    fn from(value: ContourError) -> Self {
        Self::Contour(value)
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum RailKey {
    Horizontal(u32),
    Vertical(u32),
    Spoke { cell: u32, corner: u8 },
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum VertexKey {
    Grid(u32),
    Center(u32),
    Crossing { rail: RailKey, level: u32 },
}

#[derive(Clone, Copy, Debug)]
struct ClipVertex {
    x: f64,
    y: f64,
    value: f64,
    key: VertexKey,
    /// Bit 0/1/2 identifies the original triangle edge supporting this point.
    support_mask: u8,
}

struct MeshBuilder {
    ids: HashMap<VertexKey, u32>,
    vertices: Vec<f32>,
    limits: ContourLimits,
    clip_operations: usize,
}

impl MeshBuilder {
    fn new(limits: ContourLimits) -> Self {
        Self {
            ids: HashMap::new(),
            vertices: Vec::new(),
            limits,
            clip_operations: 0,
        }
    }

    fn vertex_id(&mut self, vertex: ClipVertex) -> Result<u32, BandError> {
        if let Some(&id) = self.ids.get(&vertex.key) {
            return Ok(id);
        }
        let count = self.vertices.len() / 2;
        let requested = count.checked_add(1).ok_or(ContourError::SizeOverflow)?;
        check_limit("isoband vertices", requested, self.limits.max_band_vertices)?;
        let id = u32::try_from(count).map_err(|_| BandError::TooManyVertices(count as u64))?;
        self.vertices
            .try_reserve(2)
            .map_err(|_| ContourError::AllocationFailed {
                resource: "isoband vertices",
                requested: requested.saturating_mul(2),
            })?;
        self.ids
            .try_reserve(1)
            .map_err(|_| ContourError::AllocationFailed {
                resource: "isoband vertex index",
                requested,
            })?;
        self.vertices.push(vertex.x as f32);
        self.vertices.push(vertex.y as f32);
        self.ids.insert(vertex.key, id);
        Ok(id)
    }

    fn record_clip_operations(&mut self, count: usize) -> Result<(), BandError> {
        self.clip_operations = self
            .clip_operations
            .checked_add(count)
            .ok_or(ContourError::SizeOverflow)?;
        check_limit(
            "isoband clip operations",
            self.clip_operations,
            self.limits.max_band_clip_operations,
        )?;
        Ok(())
    }
}

fn upper_bound(values: &[f32], target: f64) -> usize {
    let mut low = 0;
    let mut high = values.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if values[middle] as f64 <= target {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn lower_bound(values: &[f32], target: f64) -> usize {
    let mut low = 0;
    let mut high = values.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if (values[middle] as f64) < target {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn one_support_bit(mask: u8) -> Option<usize> {
    if mask.count_ones() == 1 {
        Some(mask.trailing_zeros() as usize)
    } else {
        None
    }
}

fn crossing(
    a: ClipVertex,
    b: ClipVertex,
    threshold: f64,
    level_index: usize,
    rails: [RailKey; 3],
) -> Result<ClipVertex, BandError> {
    let denominator = b.value - a.value;
    if denominator == 0.0 || !denominator.is_finite() {
        return Err(BandError::Invariant(
            "clipping crossed a constant-value segment",
        ));
    }
    let t = ((threshold - a.value) / denominator).clamp(0.0, 1.0);
    let endpoint_epsilon = f64::EPSILON * 32.0;
    if t <= endpoint_epsilon {
        return Ok(ClipVertex {
            value: threshold,
            ..a
        });
    }
    if t >= 1.0 - endpoint_epsilon {
        return Ok(ClipVertex {
            value: threshold,
            ..b
        });
    }

    let support_mask = a.support_mask & b.support_mask;
    let edge = one_support_bit(support_mask).ok_or(BandError::Invariant(
        "clipped crossing lost its source rail",
    ))?;
    let level =
        u32::try_from(level_index).map_err(|_| BandError::TooManyVertices(level_index as u64))?;
    Ok(ClipVertex {
        x: a.x + t * (b.x - a.x),
        y: a.y + t * (b.y - a.y),
        value: threshold,
        key: VertexKey::Crossing {
            rail: rails[edge],
            level,
        },
        support_mask,
    })
}

fn deduplicate_polygon(vertices: &mut Vec<ClipVertex>) {
    vertices.dedup_by(|a, b| a.key == b.key);
    if vertices.len() > 1 && vertices.first().map(|v| v.key) == vertices.last().map(|v| v.key) {
        vertices.pop();
    }
}

#[derive(Clone, Copy)]
enum ClipRule {
    AtOrAbove,
    Below,
    AtOrBelow,
}

fn clip_polygon(
    input: &[ClipVertex],
    threshold: f64,
    rule: ClipRule,
    level_index: usize,
    rails: [RailKey; 3],
) -> Result<Vec<ClipVertex>, BandError> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let inside = |value: f64| match rule {
        ClipRule::AtOrAbove => value >= threshold,
        ClipRule::Below => value < threshold,
        ClipRule::AtOrBelow => value <= threshold,
    };

    let mut output = Vec::with_capacity(input.len() + 2);
    let mut previous = *input.last().expect("non-empty polygon");
    let mut previous_inside = inside(previous.value);
    for &current in input {
        let current_inside = inside(current.value);
        match (previous_inside, current_inside) {
            (true, true) => output.push(current),
            (true, false) => {
                output.push(crossing(previous, current, threshold, level_index, rails)?);
            }
            (false, true) => {
                output.push(crossing(previous, current, threshold, level_index, rails)?);
                output.push(current);
            }
            (false, false) => {}
        }
        previous = current;
        previous_inside = current_inside;
    }
    deduplicate_polygon(&mut output);
    Ok(output)
}

fn triangle_area2(a: ClipVertex, b: ClipVertex, c: ClipVertex) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn significant_triangle(a: ClipVertex, b: ClipVertex, c: ClipVertex) -> bool {
    let span_x = a.x.max(b.x).max(c.x) - a.x.min(b.x).min(c.x);
    let span_y = a.y.max(b.y).max(c.y) - a.y.min(b.y).min(c.y);
    let scale = span_x.max(span_y).max(f64::MIN_POSITIVE);
    triangle_area2(a, b, c).abs() > f64::EPSILON * 128.0 * scale * scale
}

fn emit_polygon(
    polygon: &[ClipVertex],
    band: usize,
    builder: &mut MeshBuilder,
    output: &mut PackedBands,
) -> Result<(), BandError> {
    if polygon.len() < 3 {
        return Ok(());
    }
    let mut ids = Vec::with_capacity(polygon.len());
    for &vertex in polygon {
        ids.push(builder.vertex_id(vertex)?);
    }
    for index in 1..polygon.len() - 1 {
        if !significant_triangle(polygon[0], polygon[index], polygon[index + 1]) {
            continue;
        }
        let triangle_count = output
            .stats
            .output_triangles
            .checked_add(1)
            .ok_or(ContourError::SizeOverflow)?;
        check_limit(
            "isoband triangles",
            triangle_count,
            builder.limits.max_band_triangles,
        )?;
        output
            .indices
            .try_reserve(3)
            .map_err(|_| ContourError::AllocationFailed {
                resource: "isoband triangle indices",
                requested: triangle_count.saturating_mul(3),
            })?;
        output.triangle_band_indices.try_reserve(1).map_err(|_| {
            ContourError::AllocationFailed {
                resource: "isoband triangle ordinals",
                requested: triangle_count,
            }
        })?;
        output
            .indices
            .extend_from_slice(&[ids[0], ids[index], ids[index + 1]]);
        output
            .triangle_band_indices
            .push(u32::try_from(band).map_err(|_| BandError::TooManyVertices(band as u64))?);
        output.stats.output_triangles = triangle_count;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_fan_triangle(
    vertices: [ClipVertex; 3],
    rails: [RailKey; 3],
    levels: &[f32],
    builder: &mut MeshBuilder,
    output: &mut PackedBands,
) -> Result<(), BandError> {
    output.stats.fan_triangles += 1;
    let minimum = vertices
        .iter()
        .map(|vertex| vertex.value)
        .fold(f64::INFINITY, f64::min);
    let maximum = vertices
        .iter()
        .map(|vertex| vertex.value)
        .fold(f64::NEG_INFINITY, f64::max);
    if maximum < levels[0] as f64 || minimum > levels[levels.len() - 1] as f64 {
        return Ok(());
    }

    let first_band = lower_bound(levels, minimum)
        .saturating_sub(1)
        .min(levels.len() - 2);
    let end_band = upper_bound(levels, maximum).min(levels.len() - 1);
    for band in first_band..end_band {
        builder.record_clip_operations(2)?;
        let lower = levels[band] as f64;
        let upper = levels[band + 1] as f64;
        let lower_clipped = clip_polygon(&vertices, lower, ClipRule::AtOrAbove, band, rails)?;
        let upper_rule = if band + 1 == levels.len() - 1 {
            ClipRule::AtOrBelow
        } else {
            ClipRule::Below
        };
        let polygon = clip_polygon(&lower_clipped, upper, upper_rule, band + 1, rails)?;
        if polygon.len() >= 3 {
            output.stats.band_polygons += 1;
            emit_polygon(&polygon, band, builder, output)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct CellCenter {
    u: f64,
    v: f64,
    value: f64,
    is_critical: bool,
}

fn cell_center(sw: f64, se: f64, ne: f64, nw: f64) -> CellCenter {
    let b = se - sw;
    let c = nw - sw;
    let d = ne - se - nw + sw;
    let scale = sw.abs().max(se.abs()).max(ne.abs()).max(nw.abs()).max(1.0);
    let d_tolerance = f64::EPSILON * 64.0 * scale;
    if d.abs() > d_tolerance {
        let u = -c / d;
        let v = -b / d;
        let interior = f64::EPSILON * 64.0;
        if u > interior && u < 1.0 - interior && v > interior && v < 1.0 - interior {
            let value = sw + b * u + c * v + d * u * v;
            return CellCenter {
                u,
                v,
                value,
                is_critical: true,
            };
        }
    }
    CellCenter {
        u: 0.5,
        v: 0.5,
        value: 0.25 * (sw + se + ne + nw),
        is_critical: false,
    }
}

/// Build watertight interior isoband triangles for a rectilinear structured grid.
pub fn isobands(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    requested_levels: &[f32],
) -> Result<PackedBands, BandError> {
    isobands_with_limits(
        values,
        nx,
        ny,
        xs,
        ys,
        requested_levels,
        ContourLimits::DEFAULT,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn isobands_with_limits(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    requested_levels: &[f32],
    limits: ContourLimits,
) -> Result<PackedBands, BandError> {
    validate_inputs_with_limits(values, nx, ny, xs, ys, limits)?;
    let levels = normalize_levels_checked(requested_levels, limits)?;
    if levels.len() < 2 {
        return Err(BandError::TooFewLevels);
    }
    if levels.len() > u32::MAX as usize {
        return Err(BandError::TooManyVertices(levels.len() as u64));
    }
    let cell_count = (nx - 1)
        .checked_mul(ny - 1)
        .ok_or(ContourError::SizeOverflow)?;
    if values.len() > u32::MAX as usize || cell_count > u32::MAX as usize {
        return Err(BandError::TooManyVertices(
            values.len().max(cell_count) as u64
        ));
    }

    let mut processing_levels = Vec::new();
    processing_levels
        .try_reserve_exact(levels.len())
        .map_err(|_| ContourError::AllocationFailed {
            resource: "isoband processing levels",
            requested: levels.len(),
        })?;
    processing_levels.extend_from_slice(&levels);
    let mut output = PackedBands {
        levels,
        stats: BandMeshStats {
            grid_points: values.len(),
            cells_total: cell_count,
            ..BandMeshStats::default()
        },
        ..PackedBands::default()
    };
    let mut builder = MeshBuilder::new(limits);

    for j in 0..ny - 1 {
        for i in 0..nx - 1 {
            let sw_index = j * nx + i;
            let se_index = sw_index + 1;
            let nw_index = (j + 1) * nx + i;
            let ne_index = nw_index + 1;
            let sw = values[sw_index] as f64;
            let se = values[se_index] as f64;
            let ne = values[ne_index] as f64;
            let nw = values[nw_index] as f64;
            if !sw.is_finite() || !se.is_finite() || !ne.is_finite() || !nw.is_finite() {
                output.stats.cells_skipped_non_finite += 1;
                continue;
            }
            output.stats.cells_finite += 1;

            let center = cell_center(sw, se, ne, nw);
            output.stats.critical_saddle_cells += if center.is_critical { 1 } else { 0 };
            let x0 = f64::from(xs[i]);
            let y0 = f64::from(ys[j]);
            let center_x = x0 + center.u * (f64::from(xs[i + 1]) - x0);
            let center_y = y0 + center.v * (f64::from(ys[j + 1]) - y0);
            let cell = (j * (nx - 1) + i) as u32;

            let corners = [
                ClipVertex {
                    x: xs[i] as f64,
                    y: ys[j] as f64,
                    value: sw,
                    key: VertexKey::Grid(sw_index as u32),
                    support_mask: 0,
                },
                ClipVertex {
                    x: xs[i + 1] as f64,
                    y: ys[j] as f64,
                    value: se,
                    key: VertexKey::Grid(se_index as u32),
                    support_mask: 0,
                },
                ClipVertex {
                    x: xs[i + 1] as f64,
                    y: ys[j + 1] as f64,
                    value: ne,
                    key: VertexKey::Grid(ne_index as u32),
                    support_mask: 0,
                },
                ClipVertex {
                    x: xs[i] as f64,
                    y: ys[j + 1] as f64,
                    value: nw,
                    key: VertexKey::Grid(nw_index as u32),
                    support_mask: 0,
                },
            ];
            let center_vertex = ClipVertex {
                x: center_x,
                y: center_y,
                value: center.value,
                key: VertexKey::Center(cell),
                support_mask: 0,
            };

            let horizontal_south = RailKey::Horizontal((j * (nx - 1) + i) as u32);
            let horizontal_north = RailKey::Horizontal(((j + 1) * (nx - 1) + i) as u32);
            let vertical_west = RailKey::Vertical((j * nx + i) as u32);
            let vertical_east = RailKey::Vertical((j * nx + i + 1) as u32);
            let spokes = [
                RailKey::Spoke { cell, corner: 0 },
                RailKey::Spoke { cell, corner: 1 },
                RailKey::Spoke { cell, corner: 2 },
                RailKey::Spoke { cell, corner: 3 },
            ];

            let fans = [
                (
                    [corners[0], corners[1], center_vertex],
                    [horizontal_south, spokes[1], spokes[0]],
                ),
                (
                    [corners[1], corners[2], center_vertex],
                    [vertical_east, spokes[2], spokes[1]],
                ),
                (
                    [corners[2], corners[3], center_vertex],
                    [horizontal_north, spokes[3], spokes[2]],
                ),
                (
                    [corners[3], corners[0], center_vertex],
                    [vertical_west, spokes[0], spokes[3]],
                ),
            ];

            for (mut triangle, rails) in fans {
                // Local edge support masks: v0 touches edges 0/2, v1 0/1, v2 1/2.
                triangle[0].support_mask = 0b101;
                triangle[1].support_mask = 0b011;
                triangle[2].support_mask = 0b110;
                process_fan_triangle(
                    triangle,
                    rails,
                    &processing_levels,
                    &mut builder,
                    &mut output,
                )?;
            }
        }
    }

    output.vertices = builder.vertices;
    output.stats.output_vertices = output.vertex_count();
    output.validate()?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords(count: usize) -> Vec<f32> {
        (0..count).map(|value| value as f32).collect()
    }

    fn mesh_area(output: &PackedBands) -> f64 {
        output
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|indices| {
                let point = |index: u32| {
                    let index = index as usize;
                    (
                        output.vertices[2 * index] as f64,
                        output.vertices[2 * index + 1] as f64,
                    )
                };
                let a = point(indices[0]);
                let b = point(indices[1]);
                let c = point(indices[2]);
                0.5 * ((b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)).abs()
            })
            .sum()
    }

    fn mesh_area_for_band(output: &PackedBands, band: u32) -> f64 {
        output
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .zip(output.triangle_band_indices.iter().copied())
            .filter(|(_, candidate)| *candidate == band)
            .map(|(indices, _)| {
                let point = |index: u32| {
                    let index = index as usize;
                    (
                        output.vertices[2 * index] as f64,
                        output.vertices[2 * index + 1] as f64,
                    )
                };
                let a = point(indices[0]);
                let b = point(indices[1]);
                let c = point(indices[2]);
                0.5 * ((b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)).abs()
            })
            .sum()
    }

    #[test]
    fn plane_bands_cover_cell_once() {
        let output = isobands(
            &[0.0, 2.0, 0.0, 2.0],
            2,
            2,
            &coords(2),
            &coords(2),
            &[0.0, 1.0, 2.0],
        )
        .unwrap();
        output.validate().unwrap();
        assert!((mesh_area(&output) - 1.0).abs() < 1.0e-6);
        assert_eq!(output.stats.cells_finite, 1);
    }

    #[test]
    fn bilinear_saddle_uses_critical_center() {
        let output = isobands(
            &[1.0, -1.0, -1.0, 1.0],
            2,
            2,
            &coords(2),
            &coords(2),
            &[-1.0, 0.0, 1.0],
        )
        .unwrap();
        assert_eq!(output.stats.critical_saddle_cells, 1);
        assert!((mesh_area(&output) - 1.0).abs() < 1.0e-6);
    }

    #[test]
    fn adjacent_cells_share_boundary_vertices() {
        let output = isobands(
            &[0.0, 1.0, 2.0, 0.0, 1.0, 2.0],
            3,
            2,
            &coords(3),
            &coords(2),
            &[0.0, 0.5, 1.5, 2.0],
        )
        .unwrap();
        output.validate().unwrap();
        assert!((mesh_area(&output) - 2.0).abs() < 1.0e-6);
        let shared = output
            .vertices
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|point| (point[0] - 1.0).abs() < 1.0e-6)
            .count();
        assert!(shared < output.stats.output_triangles * 2);
    }

    #[test]
    fn nan_cell_is_skipped() {
        let output = isobands(
            &[0.0, 1.0, 2.0, 0.0, f32::NAN, 2.0],
            3,
            2,
            &coords(3),
            &coords(2),
            &[0.0, 1.0, 2.0],
        )
        .unwrap();
        assert_eq!(output.stats.cells_skipped_non_finite, 2);
        assert_eq!(output.triangle_count(), 0);
    }

    #[test]
    fn constant_boundary_plateaus_have_one_sided_band_ownership() {
        for (value, expected_band) in [(-1.0, 0), (0.0, 1), (1.0, 1)] {
            let output =
                isobands(&[value; 4], 2, 2, &coords(2), &coords(2), &[-1.0, 0.0, 1.0]).unwrap();

            output.validate().unwrap();
            assert!((mesh_area(&output) - 1.0).abs() < 1.0e-12);
            assert!((mesh_area_for_band(&output, expected_band) - 1.0).abs() < 1.0e-12);
            assert!(output
                .triangle_band_indices
                .iter()
                .all(|&band| band == expected_band));
        }
    }

    #[test]
    fn triangle_budget_returns_a_fallible_error() {
        let limits = ContourLimits {
            max_band_triangles: 0,
            ..ContourLimits::DEFAULT
        };
        let error = isobands_with_limits(
            &[0.0, 2.0, 0.0, 2.0],
            2,
            2,
            &coords(2),
            &coords(2),
            &[0.0, 1.0, 2.0],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            BandError::Contour(ContourError::ResourceLimit {
                resource: "isoband triangles",
                requested: 1,
                limit: 0,
            })
        );
    }

    #[test]
    fn vertex_budget_returns_a_fallible_error() {
        let limits = ContourLimits {
            max_band_vertices: 0,
            ..ContourLimits::DEFAULT
        };
        let error = isobands_with_limits(
            &[0.0, 2.0, 0.0, 2.0],
            2,
            2,
            &coords(2),
            &coords(2),
            &[0.0, 1.0, 2.0],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            BandError::Contour(ContourError::ResourceLimit {
                resource: "isoband vertices",
                requested: 1,
                limit: 0,
            })
        );
    }

    #[test]
    fn clip_work_budget_returns_a_fallible_error() {
        let limits = ContourLimits {
            max_band_clip_operations: 0,
            ..ContourLimits::DEFAULT
        };
        let error = isobands_with_limits(
            &[0.0, 2.0, 0.0, 2.0],
            2,
            2,
            &coords(2),
            &coords(2),
            &[0.0, 1.0, 2.0],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            BandError::Contour(ContourError::ResourceLimit {
                resource: "isoband clip operations",
                requested: 2,
                limit: 0,
            })
        );
    }
}
