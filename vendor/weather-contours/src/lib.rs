//! Structured-grid weather contour geometry.
//!
//! The crate contains two output-sensitive engines:
//! - OIRT (Ordinal Iso-Rail Traversal) for connected isolines;
//! - COBRM (Critical Ordinal Band-Rail Mesh) for watertight filled isobands.
//!
//! OIRT is specialized for weather grids and sorted isoline levels:
//! - each grid edge stores the contiguous ordinal range of levels it crosses;
//! - a prefix sum gives every `(edge, level)` crossing an exact integer ID;
//! - cells connect IDs directly into a degree-two graph;
//! - degree-one chains are emitted first, then remaining cycles.
//!
//! This removes floating-point endpoint hashes, per-segment heap objects, and
//! repeated front/back vector splicing from the contour hot path.

#![deny(unsafe_op_in_unsafe_fn)]

use std::error::Error;
use std::fmt::{Display, Formatter};

mod bands;
pub use bands::*;

const INVALID_NODE: u32 = u32::MAX;
pub const CLOSED_PATH_FLAG: u8 = 1;

/// Checked resource budgets used by the native contour engines.
///
/// The defaults admit a 1799 x 1059 HRRR grid and one measured full-grid
/// isoband output (3,513,603 vertices and 7,006,930 triangles), while rejecting
/// requests large enough to plausibly exhaust a desktop process. Callers with
/// a controlled batch workload may opt into different limits through the
/// `*_with_limits` entry points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContourLimits {
    pub max_grid_points: usize,
    pub max_grid_edges: usize,
    pub max_levels: usize,
    pub max_edge_crossings: usize,
    pub max_band_vertices: usize,
    pub max_band_triangles: usize,
    pub max_band_clip_operations: usize,
}

impl ContourLimits {
    pub const DEFAULT: Self = Self {
        max_grid_points: 4 * 1024 * 1024,
        max_grid_edges: 8 * 1024 * 1024,
        max_levels: 4_096,
        max_edge_crossings: 4 * 1024 * 1024,
        max_band_vertices: 4 * 1024 * 1024,
        max_band_triangles: 8 * 1024 * 1024,
        max_band_clip_operations: 64 * 1024 * 1024,
    };
}

impl Default for ContourLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContourStats {
    pub grid_points: usize,
    pub cells_total: usize,
    pub cells_finite: usize,
    pub cells_skipped_non_finite: usize,
    pub level_count: usize,
    pub edge_crossings: usize,
    pub connected_crossings: usize,
    /// Drawable segments after exact duplicate-point canonicalization.
    pub segment_count: usize,
    pub open_path_count: usize,
    pub closed_path_count: usize,
}

/// Packed contours. `vertices` is flat x/y pairs. `path_offsets` is measured
/// in points, not floats. Closed paths do not duplicate their first point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PackedContours {
    pub levels: Vec<f32>,
    pub vertices: Vec<f32>,
    pub path_offsets: Vec<u32>,
    pub path_level_indices: Vec<u32>,
    pub path_flags: Vec<u8>,
    pub stats: ContourStats,
}

impl PackedContours {
    pub fn point_count(&self) -> usize {
        self.vertices.len() / 2
    }

    pub fn path_count(&self) -> usize {
        self.path_level_indices.len()
    }

    pub fn path_is_closed(&self, path: usize) -> bool {
        self.path_flags
            .get(path)
            .map(|flags| flags & CLOSED_PATH_FLAG != 0)
            .unwrap_or(false)
    }

    pub fn validate(&self) -> Result<(), ContourError> {
        if !self.vertices.len().is_multiple_of(2) {
            return Err(ContourError::Invariant("vertices are not x/y pairs"));
        }
        if self
            .vertices
            .iter()
            .any(|coordinate| !coordinate.is_finite())
        {
            return Err(ContourError::Invariant(
                "vertices contain a non-finite coordinate",
            ));
        }
        if self.path_offsets.len() != self.path_count() + 1 {
            return Err(ContourError::Invariant(
                "path_offsets length is not path_count + 1",
            ));
        }
        if self.path_flags.len() != self.path_count() {
            return Err(ContourError::Invariant(
                "path_flags length does not match path count",
            ));
        }
        if self.path_offsets.first().copied().unwrap_or(0) != 0 {
            return Err(ContourError::Invariant("path offsets do not start at zero"));
        }
        let point_count = self.point_count();
        let mut previous_offset = 0_u32;
        for (index, &offset) in self.path_offsets.iter().enumerate() {
            if offset as usize > point_count {
                return Err(ContourError::Invariant(
                    "path offset exceeds packed point count",
                ));
            }
            if index > 0 && offset < previous_offset {
                return Err(ContourError::Invariant("path offsets are non-monotonic"));
            }
            previous_offset = offset;
        }
        if self.path_offsets.last().copied().unwrap_or(0) as usize != point_count {
            return Err(ContourError::Invariant(
                "last path offset does not equal point count",
            ));
        }
        for (path, offsets) in self.path_offsets.windows(2).enumerate() {
            let begin = offsets[0] as usize;
            let end = offsets[1] as usize;
            let vertices = &self.vertices[begin * 2..end * 2];
            let closed = self.path_is_closed(path);
            let minimum_points = if closed { 3 } else { 2 };
            if !has_minimum_distinct_points(vertices, minimum_points) {
                return Err(ContourError::Invariant(
                    "path contains too few distinct drawable points",
                ));
            }
            if vertices
                .as_chunks::<2>()
                .0
                .iter()
                .zip(vertices[2..].as_chunks::<2>().0.iter())
                .any(|(a, b)| same_stored_point((a[0], a[1]), (b[0], b[1])))
            {
                return Err(ContourError::Invariant(
                    "path contains an exact zero-length segment",
                ));
            }
            if closed {
                let first = (vertices[0], vertices[1]);
                let last = (vertices[vertices.len() - 2], vertices[vertices.len() - 1]);
                if same_stored_point(first, last) {
                    return Err(ContourError::Invariant(
                        "closed path duplicates its first point",
                    ));
                }
            }
        }
        if self
            .path_level_indices
            .iter()
            .any(|&level| level as usize >= self.levels.len())
        {
            return Err(ContourError::Invariant("path references a missing level"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContourError {
    GridTooSmall {
        nx: usize,
        ny: usize,
    },
    DataLength {
        expected: usize,
        actual: usize,
    },
    XLength {
        expected: usize,
        actual: usize,
    },
    YLength {
        expected: usize,
        actual: usize,
    },
    NonFiniteX(usize),
    NonFiniteY(usize),
    NonMonotonicX(usize),
    NonMonotonicY(usize),
    InvalidInterval(f32),
    InvalidAnchor(f32),
    TooManyLevels(usize),
    SizeOverflow,
    TooManyCrossings(u64),
    ResourceLimit {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    AllocationFailed {
        resource: &'static str,
        requested: usize,
    },
    MissingCrossing {
        edge: usize,
        level: usize,
    },
    LevelMismatch {
        node_a: u32,
        node_b: u32,
    },
    DegreeOverflow(u32),
    Invariant(&'static str),
}

impl Display for ContourError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GridTooSmall { nx, ny } => {
                write!(f, "grid must be at least 2 x 2, got {nx} x {ny}")
            }
            Self::DataLength { expected, actual } => {
                write!(f, "data length mismatch: expected {expected}, got {actual}")
            }
            Self::XLength { expected, actual } => {
                write!(
                    f,
                    "x-coordinate length mismatch: expected {expected}, got {actual}"
                )
            }
            Self::YLength { expected, actual } => {
                write!(
                    f,
                    "y-coordinate length mismatch: expected {expected}, got {actual}"
                )
            }
            Self::NonFiniteX(index) => write!(f, "x-coordinate {index} is non-finite"),
            Self::NonFiniteY(index) => write!(f, "y-coordinate {index} is non-finite"),
            Self::NonMonotonicX(index) => write!(
                f,
                "x-coordinates change direction or collapse between indices {} and {}",
                index.saturating_sub(1),
                index
            ),
            Self::NonMonotonicY(index) => write!(
                f,
                "y-coordinates change direction or collapse between indices {} and {}",
                index.saturating_sub(1),
                index
            ),
            Self::InvalidInterval(value) => {
                write!(
                    f,
                    "contour interval must be finite and positive, got {value}"
                )
            }
            Self::InvalidAnchor(value) => write!(f, "contour anchor must be finite, got {value}"),
            Self::TooManyLevels(count) => write!(f, "too many contour levels: {count}"),
            Self::SizeOverflow => write!(f, "grid or output size overflow"),
            Self::TooManyCrossings(count) => {
                write!(
                    f,
                    "too many edge/level crossings for packed u32 IDs: {count}"
                )
            }
            Self::ResourceLimit {
                resource,
                requested,
                limit,
            } => write!(
                f,
                "{resource} resource limit exceeded: requested {requested}, limit {limit}"
            ),
            Self::AllocationFailed {
                resource,
                requested,
            } => write!(f, "could not allocate {requested} elements for {resource}"),
            Self::MissingCrossing { edge, level } => {
                write!(f, "missing crossing for edge {edge}, level index {level}")
            }
            Self::LevelMismatch { node_a, node_b } => {
                write!(
                    f,
                    "attempted to connect nodes from different levels: {node_a}, {node_b}"
                )
            }
            Self::DegreeOverflow(node) => write!(f, "contour node {node} exceeded degree two"),
            Self::Invariant(message) => write!(f, "internal contour invariant failed: {message}"),
        }
    }
}

impl Error for ContourError {}

pub(crate) fn check_limit(
    resource: &'static str,
    requested: usize,
    limit: usize,
) -> Result<(), ContourError> {
    if requested > limit {
        return Err(ContourError::ResourceLimit {
            resource,
            requested: u64::try_from(requested).unwrap_or(u64::MAX),
            limit: u64::try_from(limit).unwrap_or(u64::MAX),
        });
    }
    Ok(())
}

pub(crate) fn try_filled_vec<T: Clone>(
    length: usize,
    value: T,
    resource: &'static str,
) -> Result<Vec<T>, ContourError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| ContourError::AllocationFailed {
            resource,
            requested: length,
        })?;
    output.resize(length, value);
    Ok(output)
}

pub(crate) fn normalize_levels_checked(
    levels: &[f32],
    limits: ContourLimits,
) -> Result<Vec<f32>, ContourError> {
    check_limit("contour levels", levels.len(), limits.max_levels)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(levels.len())
        .map_err(|_| ContourError::AllocationFailed {
            resource: "contour levels",
            requested: levels.len(),
        })?;
    output.extend(levels.iter().copied().filter(|value| value.is_finite()));
    output.sort_by(f32::total_cmp);
    output.dedup_by(|a, b| *a == *b);
    Ok(output)
}

#[derive(Clone, Copy, Debug, Default)]
struct EdgeRange {
    first: u32,
    count: u32,
    base: u32,
}

impl EdgeRange {
    fn node_for(self, level_index: usize) -> Option<u32> {
        let level = u32::try_from(level_index).ok()?;
        let end = self.first.checked_add(self.count)?;
        if level < self.first || level >= end {
            return None;
        }
        self.base.checked_add(level - self.first)
    }
}

#[derive(Clone, Copy)]
enum CellEdge {
    South = 0,
    East = 1,
    North = 2,
    West = 3,
}

/// Generate regular levels `anchor + n * interval` spanning finite data.
pub fn interval_levels(
    values: &[f32],
    interval: f32,
    anchor: f32,
) -> Result<Vec<f32>, ContourError> {
    interval_levels_with_limits(values, interval, anchor, ContourLimits::DEFAULT)
}

/// Generate regular contour levels while enforcing explicit resource budgets.
pub fn interval_levels_with_limits(
    values: &[f32],
    interval: f32,
    anchor: f32,
    limits: ContourLimits,
) -> Result<Vec<f32>, ContourError> {
    if !interval.is_finite() || interval <= 0.0 {
        return Err(ContourError::InvalidInterval(interval));
    }
    if !anchor.is_finite() {
        return Err(ContourError::InvalidAnchor(anchor));
    }

    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    if !minimum.is_finite() || !maximum.is_finite() {
        return Ok(Vec::new());
    }

    let step = interval as f64;
    let origin = anchor as f64;
    let first = (((minimum as f64) - origin) / step).ceil();
    let last = (((maximum as f64) - origin) / step).floor();
    if first > last {
        return Ok(Vec::new());
    }
    let count_f64 = last - first + 1.0;
    if !count_f64.is_finite() || count_f64 > u32::MAX as f64 {
        return Err(ContourError::TooManyLevels(count_f64.max(0.0) as usize));
    }

    let count = count_f64 as usize;
    check_limit("contour levels", count, limits.max_levels)?;
    let mut levels = Vec::new();
    levels
        .try_reserve_exact(count)
        .map_err(|_| ContourError::AllocationFailed {
            resource: "contour levels",
            requested: count,
        })?;
    for offset in 0..count {
        levels.push((origin + (first + offset as f64) * step) as f32);
    }
    normalize_levels_checked(&levels, limits)
}

pub fn contour_interval(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    interval: f32,
    anchor: f32,
) -> Result<PackedContours, ContourError> {
    contour_interval_with_limits(
        values,
        nx,
        ny,
        xs,
        ys,
        interval,
        anchor,
        ContourLimits::DEFAULT,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn contour_interval_with_limits(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    interval: f32,
    anchor: f32,
    limits: ContourLimits,
) -> Result<PackedContours, ContourError> {
    let levels = interval_levels_with_limits(values, interval, anchor, limits)?;
    contour_levels_with_limits(values, nx, ny, xs, ys, &levels, limits)
}

pub fn contour_levels(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    requested_levels: &[f32],
) -> Result<PackedContours, ContourError> {
    contour_levels_with_limits(
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
pub fn contour_levels_with_limits(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    requested_levels: &[f32],
    limits: ContourLimits,
) -> Result<PackedContours, ContourError> {
    validate_inputs_with_limits(values, nx, ny, xs, ys, limits)?;
    let levels = normalize_levels_checked(requested_levels, limits)?;
    if levels.len() > u32::MAX as usize {
        return Err(ContourError::TooManyLevels(levels.len()));
    }

    let horizontal_count = (nx - 1).checked_mul(ny).ok_or(ContourError::SizeOverflow)?;
    let vertical_count = nx.checked_mul(ny - 1).ok_or(ContourError::SizeOverflow)?;
    let edge_count = horizontal_count
        .checked_add(vertical_count)
        .ok_or(ContourError::SizeOverflow)?;
    check_limit("grid edges", edge_count, limits.max_grid_edges)?;
    let cells_total = (nx - 1)
        .checked_mul(ny - 1)
        .ok_or(ContourError::SizeOverflow)?;

    let mut stats = ContourStats {
        grid_points: values.len(),
        cells_total,
        level_count: levels.len(),
        ..ContourStats::default()
    };

    if levels.is_empty() {
        return Ok(PackedContours {
            levels,
            path_offsets: vec![0],
            stats,
            ..PackedContours::default()
        });
    }

    // Phase 1: ordinal ranges for horizontal and vertical grid edges.
    let mut ranges = try_filled_vec(edge_count, EdgeRange::default(), "grid edge ranges")?;
    for j in 0..ny {
        let data_row = j * nx;
        let edge_row = j * (nx - 1);
        for i in 0..(nx - 1) {
            ranges[edge_row + i] =
                crossing_range(values[data_row + i], values[data_row + i + 1], &levels);
        }
    }
    for j in 0..(ny - 1) {
        let data_row = j * nx;
        let edge_row = horizontal_count + j * nx;
        for i in 0..nx {
            ranges[edge_row + i] =
                crossing_range(values[data_row + i], values[data_row + nx + i], &levels);
        }
    }

    let mut crossing_count = 0_u64;
    for range in &mut ranges {
        if crossing_count > u32::MAX as u64 {
            return Err(ContourError::TooManyCrossings(crossing_count));
        }
        range.base = crossing_count as u32;
        crossing_count = crossing_count
            .checked_add(u64::from(range.count))
            .ok_or(ContourError::SizeOverflow)?;
        if crossing_count > limits.max_edge_crossings as u64 {
            return Err(ContourError::ResourceLimit {
                resource: "edge crossings",
                requested: crossing_count,
                limit: limits.max_edge_crossings as u64,
            });
        }
    }
    if crossing_count > u32::MAX as u64 {
        return Err(ContourError::TooManyCrossings(crossing_count));
    }
    let node_count = usize::try_from(crossing_count).map_err(|_| ContourError::SizeOverflow)?;
    stats.edge_crossings = node_count;

    let mut node_x = try_filled_vec(node_count, 0.0_f32, "crossing x-coordinates")?;
    let mut node_y = try_filled_vec(node_count, 0.0_f32, "crossing y-coordinates")?;
    let mut node_level = try_filled_vec(node_count, 0_u32, "crossing level ordinals")?;

    for (j, &y) in ys.iter().enumerate().take(ny) {
        let data_row = j * nx;
        let edge_row = j * (nx - 1);
        for i in 0..(nx - 1) {
            fill_horizontal_nodes(
                ranges[edge_row + i],
                values[data_row + i],
                values[data_row + i + 1],
                xs[i],
                xs[i + 1],
                y,
                &levels,
                &mut node_x,
                &mut node_y,
                &mut node_level,
            );
        }
    }
    for j in 0..(ny - 1) {
        let data_row = j * nx;
        let edge_row = horizontal_count + j * nx;
        for i in 0..nx {
            fill_vertical_nodes(
                ranges[edge_row + i],
                values[data_row + i],
                values[data_row + nx + i],
                xs[i],
                ys[j],
                ys[j + 1],
                &levels,
                &mut node_x,
                &mut node_y,
                &mut node_level,
            );
        }
    }

    // Phase 2: cell topology becomes fixed two-slot node adjacency.
    let mut adjacency = try_filled_vec(node_count, [INVALID_NODE; 2], "contour node adjacency")?;
    let mut degree = try_filled_vec(node_count, 0_u8, "contour node degrees")?;

    for j in 0..(ny - 1) {
        let row = j * nx;
        for i in 0..(nx - 1) {
            let sw = values[row + i];
            let se = values[row + i + 1];
            let nw = values[row + nx + i];
            let ne = values[row + nx + i + 1];
            if !(sw.is_finite() && se.is_finite() && ne.is_finite() && nw.is_finite()) {
                stats.cells_skipped_non_finite += 1;
                continue;
            }
            stats.cells_finite += 1;

            let minimum = sw.min(se).min(ne).min(nw);
            let maximum = sw.max(se).max(ne).max(nw);
            let first_level = lower_bound(&levels, minimum);
            let end_level = lower_bound(&levels, maximum);
            if first_level == end_level {
                continue;
            }

            let cell_edges = [
                j * (nx - 1) + i,
                horizontal_count + j * nx + i + 1,
                (j + 1) * (nx - 1) + i,
                horizontal_count + j * nx + i,
            ];

            for (level_index, &level) in levels.iter().enumerate().take(end_level).skip(first_level)
            {
                let case_index = u8::from(sw > level)
                    | (u8::from(se > level) << 1)
                    | (u8::from(ne > level) << 2)
                    | (u8::from(nw > level) << 3);

                match case_index {
                    0 | 15 => {}
                    1 => connect_pair(
                        CellEdge::South,
                        CellEdge::West,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    2 => connect_pair(
                        CellEdge::South,
                        CellEdge::East,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    3 => connect_pair(
                        CellEdge::West,
                        CellEdge::East,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    4 => connect_pair(
                        CellEdge::East,
                        CellEdge::North,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    5 | 10 => {
                        let q = asymptotic_q(sw, se, ne, nw, level);
                        let se_nw_pairing = if q > 0.0 {
                            true
                        } else if q < 0.0 {
                            false
                        } else {
                            // A degenerate saddle uses one fixed pairing. Local
                            // cell parity would make a cropped/tiled extraction
                            // disagree with the same cell in the full grid.
                            true
                        };
                        if se_nw_pairing {
                            connect_pair(
                                CellEdge::South,
                                CellEdge::East,
                                &cell_edges,
                                level_index,
                                &ranges,
                                &node_level,
                                &mut adjacency,
                                &mut degree,
                                &mut stats,
                            )?;
                            connect_pair(
                                CellEdge::North,
                                CellEdge::West,
                                &cell_edges,
                                level_index,
                                &ranges,
                                &node_level,
                                &mut adjacency,
                                &mut degree,
                                &mut stats,
                            )?;
                        } else {
                            connect_pair(
                                CellEdge::South,
                                CellEdge::West,
                                &cell_edges,
                                level_index,
                                &ranges,
                                &node_level,
                                &mut adjacency,
                                &mut degree,
                                &mut stats,
                            )?;
                            connect_pair(
                                CellEdge::East,
                                CellEdge::North,
                                &cell_edges,
                                level_index,
                                &ranges,
                                &node_level,
                                &mut adjacency,
                                &mut degree,
                                &mut stats,
                            )?;
                        }
                    }
                    6 => connect_pair(
                        CellEdge::South,
                        CellEdge::North,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    7 => connect_pair(
                        CellEdge::West,
                        CellEdge::North,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    8 => connect_pair(
                        CellEdge::North,
                        CellEdge::West,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    9 => connect_pair(
                        CellEdge::South,
                        CellEdge::North,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    11 => connect_pair(
                        CellEdge::East,
                        CellEdge::North,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    12 => connect_pair(
                        CellEdge::West,
                        CellEdge::East,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    13 => connect_pair(
                        CellEdge::South,
                        CellEdge::East,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    14 => connect_pair(
                        CellEdge::South,
                        CellEdge::West,
                        &cell_edges,
                        level_index,
                        &ranges,
                        &node_level,
                        &mut adjacency,
                        &mut degree,
                        &mut stats,
                    )?,
                    _ => unreachable!(),
                }
            }
        }
    }

    stats.connected_crossings = degree.iter().filter(|&&value| value != 0).count();
    stats.segment_count = 0;

    // Phase 3: emit open chains, then closed cycles.
    let mut output = PackedContours {
        levels,
        path_offsets: vec![0],
        stats,
        ..PackedContours::default()
    };
    let vertex_capacity = node_count
        .checked_mul(2)
        .ok_or(ContourError::SizeOverflow)?;
    output
        .vertices
        .try_reserve_exact(vertex_capacity)
        .map_err(|_| ContourError::AllocationFailed {
            resource: "packed contour vertices",
            requested: vertex_capacity,
        })?;
    let path_capacity = node_count / 2;
    output
        .path_offsets
        .try_reserve_exact(path_capacity)
        .map_err(|_| ContourError::AllocationFailed {
            resource: "packed contour offsets",
            requested: path_capacity + 1,
        })?;
    output
        .path_level_indices
        .try_reserve_exact(path_capacity)
        .map_err(|_| ContourError::AllocationFailed {
            resource: "packed contour level ordinals",
            requested: path_capacity,
        })?;
    output
        .path_flags
        .try_reserve_exact(path_capacity)
        .map_err(|_| ContourError::AllocationFailed {
            resource: "packed contour flags",
            requested: path_capacity,
        })?;
    let mut visited = try_filled_vec(node_count, false, "contour traversal state")?;
    for node in 0..node_count {
        if degree[node] == 1 && !visited[node] {
            emit_path(
                node as u32,
                &adjacency,
                &degree,
                &node_x,
                &node_y,
                &node_level,
                &mut visited,
                &mut output,
            )?;
        }
    }
    for node in 0..node_count {
        if degree[node] == 2 && !visited[node] {
            emit_path(
                node as u32,
                &adjacency,
                &degree,
                &node_x,
                &node_y,
                &node_level,
                &mut visited,
                &mut output,
            )?;
        }
    }

    output.validate()?;
    Ok(output)
}

pub(crate) fn validate_inputs_with_limits(
    values: &[f32],
    nx: usize,
    ny: usize,
    xs: &[f32],
    ys: &[f32],
    limits: ContourLimits,
) -> Result<(), ContourError> {
    if nx < 2 || ny < 2 {
        return Err(ContourError::GridTooSmall { nx, ny });
    }
    let expected = nx.checked_mul(ny).ok_or(ContourError::SizeOverflow)?;
    if values.len() != expected {
        return Err(ContourError::DataLength {
            expected,
            actual: values.len(),
        });
    }
    check_limit("grid points", expected, limits.max_grid_points)?;
    if xs.len() != nx {
        return Err(ContourError::XLength {
            expected: nx,
            actual: xs.len(),
        });
    }
    if ys.len() != ny {
        return Err(ContourError::YLength {
            expected: ny,
            actual: ys.len(),
        });
    }
    if let Some(index) = xs.iter().position(|value| !value.is_finite()) {
        return Err(ContourError::NonFiniteX(index));
    }
    if let Some(index) = ys.iter().position(|value| !value.is_finite()) {
        return Err(ContourError::NonFiniteY(index));
    }
    if let Some(index) = first_non_monotonic_coordinate(xs) {
        return Err(ContourError::NonMonotonicX(index));
    }
    if let Some(index) = first_non_monotonic_coordinate(ys) {
        return Err(ContourError::NonMonotonicY(index));
    }
    Ok(())
}

fn first_non_monotonic_coordinate(coordinates: &[f32]) -> Option<usize> {
    let ascending = coordinates[1] > coordinates[0];
    coordinates
        .windows(2)
        .enumerate()
        .find(|(_, pair)| pair[0] == pair[1] || (pair[1] > pair[0]) != ascending)
        .map(|(index, _)| index + 1)
}

fn lower_bound(values: &[f32], target: f32) -> usize {
    let mut low = 0_usize;
    let mut high = values.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if values[middle] < target {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

fn crossing_range(a: f32, b: f32, levels: &[f32]) -> EdgeRange {
    if !a.is_finite() || !b.is_finite() || a == b {
        return EdgeRange::default();
    }
    let first = lower_bound(levels, a.min(b));
    let end = lower_bound(levels, a.max(b));
    EdgeRange {
        first: first as u32,
        count: (end - first) as u32,
        base: 0,
    }
}

fn next_down(value: f32) -> f32 {
    if value.is_nan() || value == f32::NEG_INFINITY {
        value
    } else if value == 0.0 {
        -f32::from_bits(1)
    } else if value > 0.0 {
        f32::from_bits(value.to_bits() - 1)
    } else {
        f32::from_bits(value.to_bits() + 1)
    }
}

fn tie_below(value: f32, level: f32) -> f32 {
    if value == level {
        next_down(value)
    } else {
        value
    }
}

fn interpolation_fraction(a: f32, b: f32, level: f32) -> f32 {
    let a = tie_below(a, level) as f64;
    let b = tie_below(b, level) as f64;
    let level = level as f64;
    let denominator = b - a;
    if denominator == 0.0 || !denominator.is_finite() {
        0.5
    } else {
        ((level - a) / denominator).clamp(0.0, 1.0) as f32
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_horizontal_nodes(
    range: EdgeRange,
    a: f32,
    b: f32,
    x0: f32,
    x1: f32,
    y: f32,
    levels: &[f32],
    node_x: &mut [f32],
    node_y: &mut [f32],
    node_level: &mut [u32],
) {
    for offset in 0..range.count {
        let level_index = range.first + offset;
        let node = (range.base + offset) as usize;
        let t = interpolation_fraction(a, b, levels[level_index as usize]);
        node_x[node] = (f64::from(x0) + f64::from(t) * (f64::from(x1) - f64::from(x0))) as f32;
        node_y[node] = y;
        node_level[node] = level_index;
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_vertical_nodes(
    range: EdgeRange,
    a: f32,
    b: f32,
    x: f32,
    y0: f32,
    y1: f32,
    levels: &[f32],
    node_x: &mut [f32],
    node_y: &mut [f32],
    node_level: &mut [u32],
) {
    for offset in 0..range.count {
        let level_index = range.first + offset;
        let node = (range.base + offset) as usize;
        let t = interpolation_fraction(a, b, levels[level_index as usize]);
        node_x[node] = x;
        node_y[node] = (f64::from(y0) + f64::from(t) * (f64::from(y1) - f64::from(y0))) as f32;
        node_level[node] = level_index;
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_pair(
    edge_a: CellEdge,
    edge_b: CellEdge,
    cell_edges: &[usize; 4],
    level_index: usize,
    ranges: &[EdgeRange],
    node_level: &[u32],
    adjacency: &mut [[u32; 2]],
    degree: &mut [u8],
    stats: &mut ContourStats,
) -> Result<(), ContourError> {
    let edge_id_a = cell_edges[edge_a as usize];
    let edge_id_b = cell_edges[edge_b as usize];
    let node_a = ranges[edge_id_a]
        .node_for(level_index)
        .ok_or(ContourError::MissingCrossing {
            edge: edge_id_a,
            level: level_index,
        })?;
    let node_b = ranges[edge_id_b]
        .node_for(level_index)
        .ok_or(ContourError::MissingCrossing {
            edge: edge_id_b,
            level: level_index,
        })?;
    if connect_nodes(node_a, node_b, node_level, adjacency, degree)? {
        stats.segment_count += 1;
    }
    Ok(())
}

fn connect_nodes(
    node_a: u32,
    node_b: u32,
    node_level: &[u32],
    adjacency: &mut [[u32; 2]],
    degree: &mut [u8],
) -> Result<bool, ContourError> {
    if node_a == node_b {
        return Err(ContourError::Invariant("self-connected contour segment"));
    }
    if node_level[node_a as usize] != node_level[node_b as usize] {
        return Err(ContourError::LevelMismatch { node_a, node_b });
    }
    if adjacency[node_a as usize].contains(&node_b) && adjacency[node_b as usize].contains(&node_a)
    {
        return Ok(false);
    }
    if degree[node_a as usize] >= 2 {
        return Err(ContourError::DegreeOverflow(node_a));
    }
    if degree[node_b as usize] >= 2 {
        return Err(ContourError::DegreeOverflow(node_b));
    }

    let slot_a = degree[node_a as usize] as usize;
    let slot_b = degree[node_b as usize] as usize;
    adjacency[node_a as usize][slot_a] = node_b;
    adjacency[node_b as usize][slot_b] = node_a;
    degree[node_a as usize] += 1;
    degree[node_b as usize] += 1;
    Ok(true)
}

fn asymptotic_q(sw: f32, se: f32, ne: f32, nw: f32, level: f32) -> f64 {
    let sw = tie_below(sw, level) as f64 - level as f64;
    let se = tie_below(se, level) as f64 - level as f64;
    let ne = tie_below(ne, level) as f64 - level as f64;
    let nw = tie_below(nw, level) as f64 - level as f64;
    sw * ne - se * nw
}

fn next_neighbor(node: u32, previous: u32, adjacency: &[[u32; 2]], degree: &[u8]) -> u32 {
    let neighbors = adjacency[node as usize];
    match degree[node as usize] {
        0 => INVALID_NODE,
        1 => {
            if neighbors[0] == previous {
                INVALID_NODE
            } else {
                neighbors[0]
            }
        }
        2 => {
            if previous == INVALID_NODE {
                neighbors[0].min(neighbors[1])
            } else if neighbors[0] != previous {
                neighbors[0]
            } else {
                neighbors[1]
            }
        }
        _ => INVALID_NODE,
    }
}

fn same_stored_point(a: (f32, f32), b: (f32, f32)) -> bool {
    a.0 == b.0 && a.1 == b.1
}

fn has_minimum_distinct_points(vertices: &[f32], minimum: usize) -> bool {
    debug_assert!(minimum <= 3);
    let mut distinct = [(0.0_f32, 0.0_f32); 3];
    let mut count = 0;
    for point in vertices
        .as_chunks::<2>()
        .0
        .iter()
        .map(|point| (point[0], point[1]))
    {
        if distinct[..count]
            .iter()
            .all(|&existing| !same_stored_point(existing, point))
        {
            distinct[count] = point;
            count += 1;
            if count == minimum {
                return true;
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn emit_path(
    start: u32,
    adjacency: &[[u32; 2]],
    degree: &[u8],
    node_x: &[f32],
    node_y: &[f32],
    node_level: &[u32],
    visited: &mut [bool],
    output: &mut PackedContours,
) -> Result<(), ContourError> {
    if visited[start as usize] || degree[start as usize] == 0 {
        return Ok(());
    }

    let first_point = output.point_count();
    let path_level = node_level[start as usize];
    let mut previous = INVALID_NODE;
    let mut current = start;
    let mut closed = false;

    loop {
        let index = current as usize;
        if visited[index] {
            closed = current == start;
            break;
        }
        if node_level[index] != path_level {
            return Err(ContourError::Invariant("path walk crossed contour levels"));
        }

        visited[index] = true;
        let point = (node_x[index], node_y[index]);
        let previous_point = output.vertices[first_point * 2..]
            .as_chunks::<2>()
            .0
            .iter()
            .next_back()
            .map(|stored| (stored[0], stored[1]));
        if previous_point.is_none_or(|previous| !same_stored_point(previous, point)) {
            output.vertices.push(point.0);
            output.vertices.push(point.1);
        }

        let next = next_neighbor(current, previous, adjacency, degree);
        if next == INVALID_NODE {
            break;
        }
        previous = current;
        current = next;
    }

    if closed && output.point_count() - first_point > 1 {
        let first = (
            output.vertices[first_point * 2],
            output.vertices[first_point * 2 + 1],
        );
        let last = (
            output.vertices[output.vertices.len() - 2],
            output.vertices[output.vertices.len() - 1],
        );
        if same_stored_point(first, last) {
            output.vertices.truncate(output.vertices.len() - 2);
        }
    }

    let point_count = output.point_count() - first_point;
    let minimum_points = if closed { 3 } else { 2 };
    if !has_minimum_distinct_points(&output.vertices[first_point * 2..], minimum_points) {
        output.vertices.truncate(first_point * 2);
        return Ok(());
    }

    output.path_level_indices.push(path_level);
    output
        .path_flags
        .push(if closed { CLOSED_PATH_FLAG } else { 0 });
    let path_end = u32::try_from(output.point_count()).map_err(|_| ContourError::SizeOverflow)?;
    output.path_offsets.push(path_end);
    if closed {
        output.stats.closed_path_count += 1;
        output.stats.segment_count += point_count;
    } else {
        output.stats.open_path_count += 1;
        output.stats.segment_count += point_count - 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords(count: usize) -> Vec<f32> {
        (0..count).map(|value| value as f32).collect()
    }

    fn path_points(output: &PackedContours, path: usize) -> Vec<(f32, f32)> {
        let begin = output.path_offsets[path] as usize;
        let end = output.path_offsets[path + 1] as usize;
        (begin..end)
            .map(|point| (output.vertices[2 * point], output.vertices[2 * point + 1]))
            .collect()
    }

    fn paths_at_level(output: &PackedContours, level: f32) -> Vec<Vec<(f32, f32)>> {
        let level_index = output
            .levels
            .iter()
            .position(|&candidate| candidate == level)
            .unwrap() as u32;
        output
            .path_level_indices
            .iter()
            .enumerate()
            .filter(|(_, candidate)| **candidate == level_index)
            .map(|(path, _)| path_points(output, path))
            .collect()
    }

    fn canonical_open_segments_at_level(
        output: &PackedContours,
        level: f32,
        translation: (f32, f32),
    ) -> Vec<((u32, u32), (u32, u32))> {
        let mut segments = paths_at_level(output, level)
            .into_iter()
            .map(|path| {
                assert_eq!(path.len(), 2);
                let point = |(x, y): (f32, f32)| {
                    ((x - translation.0).to_bits(), (y - translation.1).to_bits())
                };
                let mut endpoints = (point(path[0]), point(path[1]));
                if endpoints.1 < endpoints.0 {
                    endpoints = (endpoints.1, endpoints.0);
                }
                endpoints
            })
            .collect::<Vec<_>>();
        segments.sort_unstable();
        segments
    }

    fn next_deterministic_value(state: &mut u64) -> f32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let sample = (*state >> 40) as u32;
        sample as f32 / 16_777_215.0 * 4.0 - 2.0
    }

    #[test]
    fn plane_is_one_open_chain() {
        let values = [0.0, 1.0, 2.0, 1.0, 2.0, 3.0, 2.0, 3.0, 4.0];
        let output = contour_levels(&values, 3, 3, &coords(3), &coords(3), &[1.5]).unwrap();
        output.validate().unwrap();
        assert_eq!(output.path_count(), 1);
        assert!(!output.path_is_closed(0));
        assert_eq!(output.stats.segment_count, 3);
    }

    #[test]
    fn hill_is_one_closed_cycle() {
        let nx = 7;
        let ny = 7;
        let mut values = Vec::with_capacity(nx * ny);
        for j in 0..ny {
            for i in 0..nx {
                let dx = i as f32 - 3.0;
                let dy = j as f32 - 3.0;
                values.push(-(dx * dx + dy * dy));
            }
        }
        let output = contour_levels(&values, nx, ny, &coords(nx), &coords(ny), &[-6.5]).unwrap();
        assert_eq!(output.path_count(), 1);
        assert!(output.path_is_closed(0));
        assert_eq!(output.stats.closed_path_count, 1);
    }

    #[test]
    fn asymptotic_decider_connects_strong_positive_diagonal() {
        // SW and NE are strongly positive. Q > 0 selects S-E and N-W.
        let values = [10.0, -1.0, -1.0, 10.0];
        let output = contour_levels(&values, 2, 2, &[0.0, 1.0], &[0.0, 1.0], &[0.0]).unwrap();
        assert_eq!(output.path_count(), 2);
        assert_eq!(output.stats.segment_count, 2);
        for path in 0..2 {
            assert_eq!(path_points(&output, path).len(), 2);
        }
    }

    #[test]
    fn nan_cell_breaks_contours_without_branching() {
        let values = [
            0.0,
            1.0,
            f32::NAN,
            3.0,
            0.0,
            1.0,
            2.0,
            3.0,
            0.0,
            1.0,
            2.0,
            3.0,
        ];
        let output = contour_levels(&values, 4, 3, &coords(4), &coords(3), &[1.5]).unwrap();
        output.validate().unwrap();
        assert!(output.stats.cells_skipped_non_finite > 0);
    }

    #[test]
    fn regular_levels_match_zero_anchored_weather_intervals() {
        let levels = interval_levels(&[-2.2, 7.9, f32::NAN], 2.0, 0.0).unwrap();
        assert_eq!(levels, vec![-2.0, 0.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn zero_q_tie_is_independent_of_requested_level_position() {
        let values = [1.0, -1.0, -1.0, 1.0];
        let xs = [0.0, 1.0];
        let ys = [0.0, 1.0];
        let alone = contour_levels(&values, 2, 2, &xs, &ys, &[0.0]).unwrap();
        let with_unrelated_lower_level =
            contour_levels(&values, 2, 2, &xs, &ys, &[-2.0, 0.0]).unwrap();

        assert_eq!(
            paths_at_level(&alone, 0.0),
            paths_at_level(&with_unrelated_lower_level, 0.0)
        );
    }

    #[test]
    fn zero_q_tie_is_crop_and_translation_invariant() {
        let values = [1.0, -1.0, -1.0, 1.0];
        let standalone = contour_levels(&values, 2, 2, &[0.0, 1.0], &[0.0, 1.0], &[0.0]).unwrap();
        let embedded = contour_levels(
            &[f32::NAN, 1.0, -1.0, f32::NAN, -1.0, 1.0],
            3,
            2,
            &[-1.0, 0.0, 1.0],
            &[0.0, 1.0],
            &[0.0],
        )
        .unwrap();
        let translated =
            contour_levels(&values, 2, 2, &[10.0, 11.0], &[20.0, 21.0], &[0.0]).unwrap();

        let expected = canonical_open_segments_at_level(&standalone, 0.0, (0.0, 0.0));
        assert_eq!(
            canonical_open_segments_at_level(&embedded, 0.0, (0.0, 0.0)),
            expected
        );
        assert_eq!(
            canonical_open_segments_at_level(&translated, 0.0, (10.0, 20.0)),
            expected
        );
    }

    #[test]
    fn exact_duplicate_segment_is_not_emitted() {
        let values = [297.195_6, 297.758_1, 292.820_6, 296.945_6];
        let output = contour_levels(
            &values,
            2,
            2,
            &[311.0, 312.0],
            &[789.0, 790.0],
            &[292.820_6],
        )
        .unwrap();

        output.validate().unwrap();
        assert_eq!(output.path_count(), 0);
        assert_eq!(output.point_count(), 0);
        assert_eq!(output.stats.segment_count, 0);
    }

    #[test]
    fn grid_budget_fails_before_work_allocation() {
        let limits = ContourLimits {
            max_grid_points: 3,
            ..ContourLimits::DEFAULT
        };
        let error = contour_levels_with_limits(
            &[0.0, 1.0, 1.0, 2.0],
            2,
            2,
            &[0.0, 1.0],
            &[0.0, 1.0],
            &[0.5],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            ContourError::ResourceLimit {
                resource: "grid points",
                requested: 4,
                limit: 3,
            }
        );
    }

    #[test]
    fn edge_budget_fails_before_edge_range_allocation() {
        let limits = ContourLimits {
            max_grid_edges: 3,
            ..ContourLimits::DEFAULT
        };
        let error = contour_levels_with_limits(
            &[0.0, 1.0, 1.0, 2.0],
            2,
            2,
            &[0.0, 1.0],
            &[0.0, 1.0],
            &[0.5],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            ContourError::ResourceLimit {
                resource: "grid edges",
                requested: 4,
                limit: 3,
            }
        );
    }

    #[test]
    fn explicit_level_budget_is_checked_before_normalization() {
        let limits = ContourLimits {
            max_levels: 1,
            ..ContourLimits::DEFAULT
        };
        let error = contour_levels_with_limits(
            &[0.0, 1.0, 1.0, 2.0],
            2,
            2,
            &[0.0, 1.0],
            &[0.0, 1.0],
            &[0.5, 1.5],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            ContourError::ResourceLimit {
                resource: "contour levels",
                requested: 2,
                limit: 1,
            }
        );
    }

    #[test]
    fn interval_level_budget_prevents_unbounded_generation() {
        let limits = ContourLimits {
            max_levels: 8,
            ..ContourLimits::DEFAULT
        };
        let error = interval_levels_with_limits(&[-10.0, 10.0], 1.0, 0.0, limits).unwrap_err();
        assert_eq!(
            error,
            ContourError::ResourceLimit {
                resource: "contour levels",
                requested: 21,
                limit: 8,
            }
        );
    }

    #[test]
    fn crossing_budget_fails_before_node_allocation() {
        let limits = ContourLimits {
            max_edge_crossings: 0,
            ..ContourLimits::DEFAULT
        };
        let error = contour_levels_with_limits(
            &[0.0, 1.0, 0.0, 1.0],
            2,
            2,
            &[0.0, 1.0],
            &[0.0, 1.0],
            &[0.5],
            limits,
        )
        .unwrap_err();
        assert_eq!(
            error,
            ContourError::ResourceLimit {
                resource: "edge crossings",
                requested: 1,
                limit: 0,
            }
        );
    }

    #[test]
    fn coordinates_must_be_strictly_monotonic() {
        let error = contour_levels(
            &[0.0, 1.0, 1.0, 2.0, 2.0, 3.0],
            3,
            2,
            &[0.0, 2.0, 1.0],
            &[1.0, 0.0],
            &[0.5],
        )
        .unwrap_err();
        assert_eq!(error, ContourError::NonMonotonicX(2));
    }

    #[test]
    fn extreme_finite_coordinates_interpolate_without_overflow() {
        let output = contour_levels(
            &[0.0, 1.0, 0.0, 1.0],
            2,
            2,
            &[-f32::MAX, f32::MAX],
            &[0.0, 1.0],
            &[0.5],
        )
        .unwrap();

        output.validate().unwrap();
        assert!(output
            .vertices
            .iter()
            .all(|coordinate| coordinate.is_finite()));
        assert!(output
            .vertices
            .as_chunks::<2>()
            .0
            .iter()
            .all(|point| point[0] == 0.0));
    }

    #[test]
    fn malformed_offsets_return_an_error_without_panicking() {
        let malformed = PackedContours {
            levels: vec![0.0],
            vertices: vec![0.0, 0.0],
            path_offsets: vec![0, 1, 0],
            path_level_indices: vec![0, 0],
            path_flags: vec![0, 0],
            stats: ContourStats::default(),
        };

        let result = std::panic::catch_unwind(|| malformed.validate());
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Err(ContourError::Invariant("path offsets are non-monotonic"))
        );
    }

    #[test]
    fn default_limits_admit_native_hrrr_and_measured_band_output() {
        const NX: usize = 1_799;
        const NY: usize = 1_059;
        const MEASURED_BAND_VERTICES: usize = 3_513_603;
        const MEASURED_BAND_TRIANGLES: usize = 7_006_930;

        let points = NX * NY;
        let edges = (NX - 1) * NY + NX * (NY - 1);
        let limits = ContourLimits::DEFAULT;
        assert!(points <= limits.max_grid_points);
        assert!(edges <= limits.max_grid_edges);
        assert!(17 <= limits.max_levels);
        assert!(MEASURED_BAND_VERTICES <= limits.max_band_vertices);
        assert!(MEASURED_BAND_TRIANGLES <= limits.max_band_triangles);
    }

    #[test]
    fn deterministic_random_grids_preserve_packed_invariants() {
        let nx = 6;
        let ny = 5;
        let xs = coords(nx);
        let ys = coords(ny);
        let levels = [-2.0, -1.0, 0.0, 1.0, 2.0];
        let mut state = 0x4d59_5df4_d0f3_3173_u64;

        for case in 0..32 {
            let mut values = Vec::with_capacity(nx * ny);
            for index in 0..nx * ny {
                let value = next_deterministic_value(&mut state);
                values.push(if (case * nx * ny + index) % 23 == 0 {
                    f32::NAN
                } else {
                    value
                });
            }

            let contours = contour_levels(&values, nx, ny, &xs, &ys, &levels).unwrap();
            contours.validate().unwrap();
            let bands = isobands(&values, nx, ny, &xs, &ys, &levels).unwrap();
            bands.validate().unwrap();
        }
    }
}
