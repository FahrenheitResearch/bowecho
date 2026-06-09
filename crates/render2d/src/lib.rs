//! 2D radar rendering contracts.
//!
//! The long-term renderer will be GPU-backed, but this crate already provides a
//! CPU raster path for smoke tests, screenshots, and early visual validation.

use std::f32::consts::PI;
use std::ops::Range;
use std::path::Path;

pub use color_tables::{ColorSampler, ColorTable, ColorTableFamily, ColorTableSet};
mod shear;
mod volumetric;
use image::{ImageBuffer, ImageError, Rgba};
use radar_core::{
    ElevationCut, GateRange, MomentGrid, MomentStorage, MomentType, ProductId, RadarVolume,
};
use rayon::prelude::*;
pub use shear::{azimuthal_shear_grid, radial_divergence_grid};
use thiserror::Error;
pub use volumetric::{
    CrossSection, ECHO_TOP_THRESHOLD_DBZ, composite_reflectivity_grid, echo_top_grid,
    reflectivity_cross_section, velocity_cross_section, vil_density_grid, vil_grid,
};

const AZIMUTH_BINS: usize = 3600;
const AZIMUTH_BIN_WIDTH_DEG: f32 = 0.1;
const MAX_AZIMUTH_HALF_WIDTH_DEG: f32 = 3.0;
const MAX_AZIMUTH_CANDIDATES: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderLayer {
    pub product: ProductId,
    pub moment: Option<MomentType>,
    pub visible: bool,
}

impl RenderLayer {
    pub fn base(moment: MomentType) -> Self {
        Self {
            product: ProductId::from(moment.clone()),
            moment: Some(moment),
            visible: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RasterOptions {
    pub width: u32,
    pub height: u32,
    pub range_fraction: u8,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            width: 1024,
            height: 1024,
            range_fraction: 94,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewportRasterOptions {
    pub width: u32,
    pub height: u32,
    pub radar_x_px: f32,
    pub radar_y_px: f32,
    pub km_per_px_x: f32,
    pub km_per_px_y: f32,
}

pub fn viewport_rgba_buffer_len(options: ViewportRasterOptions) -> usize {
    let (width, height) = viewport_dimensions(options);
    rgba_len(width, height)
}

pub fn viewport_sample_cache_storage_upper_bound(options: ViewportRasterOptions) -> usize {
    let (width, height) = viewport_dimensions(options);
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(std::mem::size_of::<CachedSample>())
        .saturating_add((height as usize).saturating_mul(std::mem::size_of::<CachedRowSpan>()))
}

pub fn viewport_sample_cache_storage_upper_bound_for_grid(
    grid: &MomentGrid,
    options: ViewportRasterOptions,
) -> usize {
    let (_, height) = viewport_dimensions(options);
    let geometry = viewport_geometry(grid, options);
    let sample_slots = (0..height)
        .filter_map(|y| geometry.x_range_for_row(y))
        .map(|range| range.len())
        .sum::<usize>();
    sample_slots
        .saturating_mul(std::mem::size_of::<CachedSample>())
        .saturating_add((height as usize).saturating_mul(std::mem::size_of::<CachedRowSpan>()))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StormMotion {
    pub direction_deg: f32,
    pub speed_mps: f32,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("cut index {index} is out of range for {cut_count} cuts")]
    CutOutOfRange { index: usize, cut_count: usize },
    #[error("moment {moment} is not available in cut {cut_index}")]
    MissingMoment {
        cut_index: usize,
        moment: MomentType,
    },
    #[error("moment {moment} in cut {cut_index} has no decoded rows")]
    EmptyMoment {
        cut_index: usize,
        moment: MomentType,
    },
    #[error("RGBA buffer has {actual} bytes, expected {expected} for {width}x{height}")]
    BufferSizeMismatch {
        actual: usize,
        expected: usize,
        width: u32,
        height: u32,
    },
    #[error("viewport render cache belongs to a different radar volume")]
    CacheVolumeMismatch,
    #[error("viewport render cache is for cut {actual}, expected cut {expected}")]
    CacheCutMismatch { expected: usize, actual: usize },
    #[error("viewport render cache is for {actual}, expected {expected}")]
    CacheMomentMismatch {
        expected: MomentType,
        actual: MomentType,
    },
    #[error("viewport render cache storage no longer matches the moment storage")]
    CacheStorageMismatch,
    #[error("viewport geometry cache does not match this moment grid")]
    GeometryCacheMismatch,
    #[error("image write failed: {0}")]
    Image(#[from] ImageError),
}

pub type Result<T> = std::result::Result<T, RenderError>;

/// Render a decoded polar moment to a simple radar PNG.
pub fn render_moment_png(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    out_path: &Path,
    options: RasterOptions,
) -> Result<()> {
    let image = render_moment_image(volume, cut_index, moment, options)?;
    image.save(out_path)?;
    Ok(())
}

pub fn render_moment_image(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: RasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or(RenderError::CutOutOfRange {
            index: cut_index,
            cut_count: volume.cuts.len(),
        })?;
    let grid = cut
        .moments
        .get(&moment)
        .ok_or_else(|| RenderError::MissingMoment {
            cut_index,
            moment: moment.clone(),
        })?;

    if grid.radial_indices.is_empty() {
        return Err(RenderError::EmptyMoment { cut_index, moment });
    }

    let row_lookup = AzimuthLookup::new(cut, grid);
    let width = options.width.max(64);
    let height = options.height.max(64);
    let center_x = (width as f32 - 1.0) / 2.0;
    let center_y = (height as f32 - 1.0) / 2.0;
    let radius_px = center_x.min(center_y) * (f32::from(options.range_fraction) / 100.0);
    let max_range_m = max_range_m(grid).max(1.0);

    let mut pixels = vec![0; width as usize * height as usize * 4];
    let color_tables = ColorTableSet::default();
    let color_table = color_tables.for_family(color_family_for_moment(&grid.moment));

    match &grid.storage {
        MomentStorage::U8(values) => {
            let palette = build_u8_palette(grid, color_table);
            render_compact_storage(
                &mut pixels,
                values,
                &palette,
                grid,
                &row_lookup,
                RasterGeometry {
                    width,
                    center_x,
                    center_y,
                    radius_px,
                    radius_sq_px: radius_px * radius_px,
                    max_range_m,
                },
                false,
            );
        }
        MomentStorage::U16(values) => {
            let palette = build_u16_palette(grid, color_table);
            render_compact_storage(
                &mut pixels,
                values,
                &palette,
                grid,
                &row_lookup,
                RasterGeometry {
                    width,
                    center_x,
                    center_y,
                    radius_px,
                    radius_sq_px: radius_px * radius_px,
                    max_range_m,
                },
                false,
            );
        }
        MomentStorage::F32(values) => render_f32_storage(
            &mut pixels,
            values,
            grid,
            &row_lookup,
            color_table,
            RasterGeometry {
                width,
                center_x,
                center_y,
                radius_px,
                radius_sq_px: radius_px * radius_px,
                max_range_m,
            },
            false,
        ),
    }

    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_moment_viewport_image(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let (width, height, pixels) = render_moment_viewport_rgba(volume, cut_index, moment, options)?;
    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_moment_viewport_rgba(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
) -> Result<(u32, u32, Vec<u8>)> {
    let (width, height) = viewport_dimensions(options);
    let mut pixels = vec![0; rgba_len(width, height)];
    render_moment_viewport_rgba_into(volume, cut_index, moment, options, &mut pixels)?;
    Ok((width, height, pixels))
}

pub fn render_moment_viewport_rgba_into(
    volume: &RadarVolume,
    cut_index: usize,
    moment: MomentType,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
) -> Result<(u32, u32)> {
    let cache = ViewportMomentCache::new(volume, cut_index, moment)?;
    cache.render_moment_rgba_into(volume, options, pixels)
}

pub struct ViewportMomentCache {
    volume_ptr: usize,
    cut_index: usize,
    moment: MomentType,
    row_lookup: AzimuthLookup,
    color_lookup: CachedColorLookup,
    storm_motion_basis: Option<StormMotionBasis>,
    dealiased_grid: Option<MomentGrid>,
}

pub struct ViewportSampleCache {
    volume_ptr: usize,
    cut_index: usize,
    moment: MomentType,
    width: u32,
    height: u32,
    sample_count: usize,
    row_spans: Vec<CachedRowSpan>,
    samples: Vec<CachedSample>,
}

pub struct ViewportGeometryCache {
    width: u32,
    height: u32,
    gate_range: GateRange,
    sample_count: usize,
    row_spans: Vec<CachedRowSpan>,
    samples: Vec<CachedSample>,
}

pub struct StormRelativePaletteCache {
    volume_ptr: usize,
    cut_index: usize,
    row_palettes: Vec<[[u8; 4]; 256]>,
}

impl ViewportSampleCache {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    pub fn storage_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<CachedSample>()
            + self.row_spans.len() * std::mem::size_of::<CachedRowSpan>()
    }

    fn geometry(&self) -> CachedViewportGeometry<'_> {
        CachedViewportGeometry {
            row_spans: &self.row_spans,
            samples: &self.samples,
        }
    }
}

impl ViewportGeometryCache {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn sample_count(&self) -> usize {
        self.sample_count
    }

    pub fn storage_bytes(&self) -> usize {
        self.samples.len() * std::mem::size_of::<CachedSample>()
            + self.row_spans.len() * std::mem::size_of::<CachedRowSpan>()
    }

    fn geometry(&self) -> CachedViewportGeometry<'_> {
        CachedViewportGeometry {
            row_spans: &self.row_spans,
            samples: &self.samples,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CachedRowSpan {
    start: u32,
    end: u32,
    sample_offset: usize,
}

impl CachedRowSpan {
    fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            sample_offset: 0,
        }
    }

    fn range(self) -> Option<Range<u32>> {
        (self.start < self.end).then_some(self.start..self.end)
    }
}

struct CachedRowBuild {
    start: u32,
    samples: Vec<CachedSample>,
    sample_count: usize,
}

impl CachedRowBuild {
    fn empty() -> Self {
        Self {
            start: 0,
            samples: Vec::new(),
            sample_count: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CachedSample(u32);

impl CachedSample {
    const GATE_BITS: u32 = 16;
    const GATE_MASK: u32 = (1 << Self::GATE_BITS) - 1;
    const SKIP_FLAG: u32 = 1 << 31;
    const SKIP_MASK: u32 = Self::SKIP_FLAG - 1;
    const ROW_LIMIT: usize = 1 << (u32::BITS - Self::GATE_BITS - 1);

    fn new(sample: ResolvedSample) -> Option<Self> {
        if sample.row >= Self::ROW_LIMIT || sample.gate > Self::GATE_MASK as usize {
            return None;
        }
        Some(Self(
            ((sample.row as u32) << Self::GATE_BITS) | sample.gate as u32,
        ))
    }

    fn skip(pixel_count: u32) -> Option<Self> {
        (pixel_count > 0 && pixel_count <= Self::SKIP_MASK)
            .then_some(Self(Self::SKIP_FLAG | pixel_count))
    }

    #[cfg(test)]
    fn sample(self) -> Option<ResolvedSample> {
        (!self.is_skip()).then_some(ResolvedSample {
            row: (self.0 >> Self::GATE_BITS) as usize,
            gate: (self.0 & Self::GATE_MASK) as usize,
        })
    }

    #[inline]
    fn is_skip(self) -> bool {
        self.0 & Self::SKIP_FLAG != 0
    }

    #[inline]
    fn skip_len(self) -> Option<u32> {
        self.is_skip().then_some(self.0 & Self::SKIP_MASK)
    }

    #[inline]
    fn row(self) -> usize {
        (self.0 >> Self::GATE_BITS) as usize
    }

    #[inline]
    fn gate(self) -> usize {
        (self.0 & Self::GATE_MASK) as usize
    }
}

struct StormMotionBasis {
    beam_cos: Vec<f32>,
    beam_sin: Vec<f32>,
}

impl StormMotionBasis {
    fn new(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        let mut beam_cos = Vec::with_capacity(grid.radial_indices.len());
        let mut beam_sin = Vec::with_capacity(grid.radial_indices.len());
        for radial_index in &grid.radial_indices {
            let azimuth_rad = cut
                .radials
                .get(*radial_index)
                .map(|radial| radial.azimuth_deg.to_radians())
                .unwrap_or(0.0);
            beam_cos.push(azimuth_rad.cos());
            beam_sin.push(azimuth_rad.sin());
        }
        Self { beam_cos, beam_sin }
    }

    fn row_motion_components(&self, storm_motion: StormMotion) -> Vec<f32> {
        let direction_rad = storm_motion.direction_deg.to_radians();
        let storm_cos = storm_motion.speed_mps * direction_rad.cos();
        let storm_sin = storm_motion.speed_mps * direction_rad.sin();
        self.beam_cos
            .iter()
            .zip(&self.beam_sin)
            .map(|(beam_cos, beam_sin)| storm_cos * *beam_cos + storm_sin * *beam_sin)
            .collect()
    }
}

enum CachedColorLookup {
    U8 {
        palette: Box<[[u8; 4]; 256]>,
        color_table: ColorTable,
    },
    U16 {
        palette: Vec<[u8; 4]>,
        color_table: ColorTable,
    },
    F32 {
        color_table: ColorTable,
    },
}

impl CachedColorLookup {
    fn new(grid: &MomentGrid, color_tables: &ColorTableSet) -> Self {
        Self::new_for_family(grid, color_tables, color_family_for_moment(&grid.moment))
    }

    fn new_for_family(
        grid: &MomentGrid,
        color_tables: &ColorTableSet,
        family: ColorTableFamily,
    ) -> Self {
        let color_table = color_tables.for_family(family).clone();
        match &grid.storage {
            MomentStorage::U8(_) => Self::U8 {
                palette: Box::new(build_u8_palette(grid, &color_table)),
                color_table,
            },
            MomentStorage::U16(_) => Self::U16 {
                palette: build_u16_palette(grid, &color_table),
                color_table,
            },
            MomentStorage::F32(_) => Self::F32 { color_table },
        }
    }

    fn color_table(&self) -> &ColorTable {
        match self {
            Self::U8 { color_table, .. }
            | Self::U16 { color_table, .. }
            | Self::F32 { color_table } => color_table,
        }
    }
}

impl ViewportMomentCache {
    pub fn new(volume: &RadarVolume, cut_index: usize, moment: MomentType) -> Result<Self> {
        Self::new_with_color_tables(volume, cut_index, moment, &ColorTableSet::default())
    }

    pub fn new_with_color_tables(
        volume: &RadarVolume,
        cut_index: usize,
        moment: MomentType,
        color_tables: &ColorTableSet,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let grid = cut
            .moments
            .get(&moment)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index,
                moment: moment.clone(),
            })?;

        if grid.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment { cut_index, moment });
        }

        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            storm_motion_basis: (moment == MomentType::Velocity)
                .then(|| StormMotionBasis::new(cut, grid)),
            moment,
            row_lookup: AzimuthLookup::new(cut, grid),
            color_lookup: CachedColorLookup::new(grid, color_tables),
            dealiased_grid: None,
        })
    }

    pub fn new_dealiased_velocity(volume: &RadarVolume, cut_index: usize) -> Result<Self> {
        Self::new_dealiased_velocity_with_color_tables(volume, cut_index, &ColorTableSet::default())
    }

    pub fn new_dealiased_velocity_with_color_tables(
        volume: &RadarVolume,
        cut_index: usize,
        color_tables: &ColorTableSet,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        let source_grid =
            cut.moments
                .get(&MomentType::Velocity)
                .ok_or_else(|| RenderError::MissingMoment {
                    cut_index,
                    moment: MomentType::Velocity,
                })?;

        if source_grid.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment {
                cut_index,
                moment: MomentType::Velocity,
            });
        }

        let dealiased_grid = dealias_velocity_grid(cut, source_grid);
        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            moment: MomentType::Velocity,
            row_lookup: AzimuthLookup::new(cut, &dealiased_grid),
            color_lookup: CachedColorLookup::new(&dealiased_grid, color_tables),
            storm_motion_basis: Some(StormMotionBasis::new(cut, &dealiased_grid)),
            dealiased_grid: Some(dealiased_grid),
        })
    }

    /// Build a cache around a pre-computed derived grid (composite reflectivity,
    /// echo tops, VIL, …) drawn on `cut_index`'s geometry. The grid overrides
    /// the cut's moments via the same mechanism as the dealiased path.
    pub fn new_derived(
        volume: &RadarVolume,
        cut_index: usize,
        grid: MomentGrid,
        family: ColorTableFamily,
        color_tables: &ColorTableSet,
    ) -> Result<Self> {
        let cut = volume
            .cuts
            .get(cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: cut_index,
                cut_count: volume.cuts.len(),
            })?;
        if grid.radial_indices.is_empty() {
            return Err(RenderError::EmptyMoment {
                cut_index,
                moment: grid.moment.clone(),
            });
        }
        Ok(Self {
            volume_ptr: volume as *const RadarVolume as usize,
            cut_index,
            moment: grid.moment.clone(),
            row_lookup: AzimuthLookup::new(cut, &grid),
            color_lookup: CachedColorLookup::new_for_family(&grid, color_tables, family),
            storm_motion_basis: None,
            dealiased_grid: Some(grid),
        })
    }

    pub fn cut_index(&self) -> usize {
        self.cut_index
    }

    pub fn moment(&self) -> &MomentType {
        &self.moment
    }

    pub fn render_moment_rgba_into(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        let (_, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        ensure_rgba_buffer(pixels, width, height)?;
        render_moment_viewport_grid_into(
            grid,
            &self.row_lookup,
            &self.color_lookup,
            options,
            pixels,
            true,
        )?;
        Ok((width, height))
    }

    pub fn build_sample_cache(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<ViewportSampleCache> {
        let (_, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        let geometry = viewport_geometry(grid, options);
        let lookup_table = ViewportLookupTable::new(grid, geometry);

        let row_builds = match &grid.storage {
            MomentStorage::U8(values) => {
                build_sample_cache_rows(height, &lookup_table, &self.row_lookup, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::U16(values) => {
                build_sample_cache_rows(height, &lookup_table, &self.row_lookup, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::F32(values) => {
                build_sample_cache_rows(height, &lookup_table, &self.row_lookup, |sample| {
                    resolve_f32_sample(values, grid, &self.row_lookup, sample)
                })
            }
        };

        Ok(viewport_sample_cache_from_rows(
            self.volume_ptr,
            self.cut_index,
            self.moment.clone(),
            width,
            height,
            row_builds,
        ))
    }

    pub fn build_geometry_cache(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<ViewportGeometryCache> {
        let (_, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        let geometry = viewport_geometry(grid, options);
        let lookup_table = ViewportLookupTable::new(grid, geometry);
        let row_builds = build_geometry_cache_rows(height, &lookup_table, &self.row_lookup);
        let (sample_count, row_spans, samples) = flatten_cached_rows(height, row_builds);

        Ok(ViewportGeometryCache {
            width,
            height,
            gate_range: grid.gate_range.clone(),
            sample_count,
            row_spans,
            samples,
        })
    }

    pub fn build_sample_cache_from_geometry_cache(
        &self,
        volume: &RadarVolume,
        geometry_cache: &ViewportGeometryCache,
    ) -> Result<ViewportSampleCache> {
        let (_, grid) = self.cut_and_grid(volume)?;
        if grid.gate_range != geometry_cache.gate_range {
            return Err(RenderError::GeometryCacheMismatch);
        }
        let geometry = geometry_cache.geometry();
        let row_builds = match &grid.storage {
            MomentStorage::U8(values) => {
                build_sample_cache_rows_from_geometry(geometry_cache.height, geometry, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::U16(values) => {
                build_sample_cache_rows_from_geometry(geometry_cache.height, geometry, |sample| {
                    resolve_compact_sample(values, grid, &self.row_lookup, sample)
                })
            }
            MomentStorage::F32(values) => {
                build_sample_cache_rows_from_geometry(geometry_cache.height, geometry, |sample| {
                    resolve_f32_sample(values, grid, &self.row_lookup, sample)
                })
            }
        };

        Ok(viewport_sample_cache_from_rows(
            self.volume_ptr,
            self.cut_index,
            self.moment.clone(),
            geometry_cache.width,
            geometry_cache.height,
            row_builds,
        ))
    }

    pub fn sample_cache_storage_upper_bound(
        &self,
        volume: &RadarVolume,
        options: ViewportRasterOptions,
    ) -> Result<usize> {
        let (_, grid) = self.cut_and_grid(volume)?;
        Ok(viewport_sample_cache_storage_upper_bound_for_grid(
            grid, options,
        ))
    }

    pub fn render_moment_rgba_with_sample_cache(
        &self,
        volume: &RadarVolume,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_moment_rgba_with_sample_cache_impl(volume, sample_cache, pixels, true)
    }

    /// Renders over an existing RGBA buffer without clearing transparent pixels first.
    ///
    /// Callers must only use this when `pixels` was last rendered with the same
    /// volume, cut, moment, and viewport sample footprint. The app worker tracks
    /// that provenance before taking this path.
    pub fn render_moment_rgba_with_sample_cache_reusing_transparency(
        &self,
        volume: &RadarVolume,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_moment_rgba_with_sample_cache_impl(volume, sample_cache, pixels, false)
    }

    fn render_moment_rgba_with_sample_cache_impl(
        &self,
        volume: &RadarVolume,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
        clear_pixels: bool,
    ) -> Result<(u32, u32)> {
        let (_, grid) = self.cut_and_grid(volume)?;
        self.ensure_sample_cache(sample_cache)?;
        ensure_rgba_buffer(pixels, sample_cache.width, sample_cache.height)?;
        render_moment_sample_cache_grid_into(
            grid,
            &self.color_lookup,
            sample_cache,
            pixels,
            clear_pixels,
        )?;
        Ok(sample_cache.dimensions())
    }

    pub fn render_storm_relative_velocity_rgba_into(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_storm_relative_velocity_rgba_into_cached(
            volume,
            storm_motion,
            None,
            options,
            pixels,
        )
    }

    pub fn build_storm_relative_velocity_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
    ) -> Result<Option<StormRelativePaletteCache>> {
        if self.moment != MomentType::Velocity {
            return Err(RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: self.moment.clone(),
            });
        }

        let (cut, grid) = self.cut_and_grid(volume)?;
        let MomentStorage::U8(_) = &grid.storage else {
            return Ok(None);
        };
        let row_motion = self
            .storm_motion_basis
            .as_ref()
            .map(|basis| basis.row_motion_components(storm_motion))
            .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
        Ok(Some(StormRelativePaletteCache {
            volume_ptr: self.volume_ptr,
            cut_index: self.cut_index,
            row_palettes: build_storm_relative_u8_row_palettes(
                grid,
                &row_motion,
                self.color_lookup.color_table(),
            ),
        }))
    }

    pub fn render_storm_relative_velocity_rgba_into_with_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: &StormRelativePaletteCache,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.ensure_storm_relative_palette_cache(palette_cache)?;
        self.render_storm_relative_velocity_rgba_into_cached(
            volume,
            storm_motion,
            Some(palette_cache),
            options,
            pixels,
        )
    }

    fn render_storm_relative_velocity_rgba_into_cached(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: Option<&StormRelativePaletteCache>,
        options: ViewportRasterOptions,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        if self.moment != MomentType::Velocity {
            return Err(RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: self.moment.clone(),
            });
        }

        let (cut, grid) = self.cut_and_grid(volume)?;
        let (width, height) = viewport_dimensions(options);
        ensure_rgba_buffer(pixels, width, height)?;
        render_storm_relative_velocity_viewport_grid_into(
            cut,
            grid,
            StormRelativeRenderCache {
                row_lookup: &self.row_lookup,
                storm_motion_basis: self.storm_motion_basis.as_ref(),
                color_table: self.color_lookup.color_table(),
                palette_cache,
            },
            storm_motion,
            options,
            pixels,
            true,
        );
        Ok((width, height))
    }

    pub fn render_storm_relative_velocity_rgba_with_sample_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            None,
            sample_cache,
            pixels,
            true,
        )
    }

    /// Renders SRV over an existing RGBA buffer without clearing transparent pixels first.
    ///
    /// This is safe only when the buffer came from the same velocity sample
    /// footprint. The storm motion may differ because every cached velocity
    /// sample is overwritten during this render.
    pub fn render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            None,
            sample_cache,
            pixels,
            false,
        )
    }

    pub fn render_storm_relative_velocity_rgba_with_sample_cache_and_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: &StormRelativePaletteCache,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.ensure_storm_relative_palette_cache(palette_cache)?;
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            Some(palette_cache),
            sample_cache,
            pixels,
            true,
        )
    }

    pub fn render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency_and_palette_cache(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: &StormRelativePaletteCache,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
    ) -> Result<(u32, u32)> {
        self.ensure_storm_relative_palette_cache(palette_cache)?;
        self.render_storm_relative_velocity_rgba_with_sample_cache_impl(
            volume,
            storm_motion,
            Some(palette_cache),
            sample_cache,
            pixels,
            false,
        )
    }

    fn render_storm_relative_velocity_rgba_with_sample_cache_impl(
        &self,
        volume: &RadarVolume,
        storm_motion: StormMotion,
        palette_cache: Option<&StormRelativePaletteCache>,
        sample_cache: &ViewportSampleCache,
        pixels: &mut [u8],
        clear_pixels: bool,
    ) -> Result<(u32, u32)> {
        if self.moment != MomentType::Velocity {
            return Err(RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: self.moment.clone(),
            });
        }

        let (cut, grid) = self.cut_and_grid(volume)?;
        self.ensure_sample_cache(sample_cache)?;
        ensure_rgba_buffer(pixels, sample_cache.width, sample_cache.height)?;
        render_storm_relative_velocity_sample_cache_grid_into(
            cut,
            grid,
            StormRelativeRenderCache {
                row_lookup: &self.row_lookup,
                storm_motion_basis: self.storm_motion_basis.as_ref(),
                color_table: self.color_lookup.color_table(),
                palette_cache,
            },
            storm_motion,
            sample_cache,
            pixels,
            clear_pixels,
        );
        Ok(sample_cache.dimensions())
    }

    fn ensure_sample_cache(&self, sample_cache: &ViewportSampleCache) -> Result<()> {
        if self.volume_ptr != sample_cache.volume_ptr {
            return Err(RenderError::CacheVolumeMismatch);
        }
        if self.cut_index != sample_cache.cut_index {
            return Err(RenderError::CacheCutMismatch {
                expected: self.cut_index,
                actual: sample_cache.cut_index,
            });
        }
        if self.moment != sample_cache.moment {
            return Err(RenderError::CacheMomentMismatch {
                expected: self.moment.clone(),
                actual: sample_cache.moment.clone(),
            });
        }
        Ok(())
    }

    fn ensure_storm_relative_palette_cache(
        &self,
        palette_cache: &StormRelativePaletteCache,
    ) -> Result<()> {
        if self.volume_ptr != palette_cache.volume_ptr {
            return Err(RenderError::CacheVolumeMismatch);
        }
        if self.cut_index != palette_cache.cut_index {
            return Err(RenderError::CacheCutMismatch {
                expected: self.cut_index,
                actual: palette_cache.cut_index,
            });
        }
        Ok(())
    }

    fn cut_and_grid<'a>(
        &'a self,
        volume: &'a RadarVolume,
    ) -> Result<(&'a ElevationCut, &'a MomentGrid)> {
        if self.volume_ptr != volume as *const RadarVolume as usize {
            return Err(RenderError::CacheVolumeMismatch);
        }

        let cut = volume
            .cuts
            .get(self.cut_index)
            .ok_or(RenderError::CutOutOfRange {
                index: self.cut_index,
                cut_count: volume.cuts.len(),
            })?;
        if let Some(grid) = &self.dealiased_grid {
            return Ok((cut, grid));
        }
        let grid = cut
            .moments
            .get(&self.moment)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index: self.cut_index,
                moment: self.moment.clone(),
            })?;
        Ok((cut, grid))
    }
}

fn render_moment_viewport_grid_into(
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    color_lookup: &CachedColorLookup,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
    clear_pixels: bool,
) -> Result<()> {
    let geometry = viewport_geometry(grid, options);
    let lookup_table = ViewportLookupTable::new(grid, geometry);

    match (&grid.storage, color_lookup) {
        (MomentStorage::U8(values), CachedColorLookup::U8 { palette, .. }) => {
            render_compact_viewport_storage(
                pixels,
                values,
                palette.as_ref(),
                grid,
                row_lookup,
                &lookup_table,
                clear_pixels,
            );
        }
        (MomentStorage::U16(values), CachedColorLookup::U16 { palette, .. }) => {
            render_compact_viewport_storage(
                pixels,
                values,
                palette,
                grid,
                row_lookup,
                &lookup_table,
                clear_pixels,
            );
        }
        (MomentStorage::F32(values), color_lookup) => {
            render_f32_viewport_storage(
                pixels,
                values,
                grid,
                row_lookup,
                color_lookup.color_table(),
                &lookup_table,
                clear_pixels,
            );
        }
        _ => return Err(RenderError::CacheStorageMismatch),
    }
    Ok(())
}

fn render_moment_sample_cache_grid_into(
    grid: &MomentGrid,
    color_lookup: &CachedColorLookup,
    sample_cache: &ViewportSampleCache,
    pixels: &mut [u8],
    clear_pixels: bool,
) -> Result<()> {
    match (&grid.storage, color_lookup) {
        (MomentStorage::U8(values), CachedColorLookup::U8 { palette, .. }) => {
            render_compact_sample_cache_storage(
                pixels,
                values,
                palette.as_ref(),
                grid,
                sample_cache,
                clear_pixels,
            );
        }
        (MomentStorage::U16(values), CachedColorLookup::U16 { palette, .. }) => {
            render_compact_sample_cache_storage(
                pixels,
                values,
                palette,
                grid,
                sample_cache,
                clear_pixels,
            );
        }
        (MomentStorage::F32(values), color_lookup) => {
            render_f32_sample_cache_storage(
                pixels,
                values,
                grid,
                color_lookup.color_table(),
                sample_cache,
                clear_pixels,
            );
        }
        _ => return Err(RenderError::CacheStorageMismatch),
    }
    Ok(())
}

pub fn render_storm_relative_velocity_image(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: RasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let cut = volume
        .cuts
        .get(cut_index)
        .ok_or(RenderError::CutOutOfRange {
            index: cut_index,
            cut_count: volume.cuts.len(),
        })?;
    let grid =
        cut.moments
            .get(&MomentType::Velocity)
            .ok_or_else(|| RenderError::MissingMoment {
                cut_index,
                moment: MomentType::Velocity,
            })?;

    if grid.radial_indices.is_empty() {
        return Err(RenderError::EmptyMoment {
            cut_index,
            moment: MomentType::Velocity,
        });
    }

    let row_lookup = AzimuthLookup::new(cut, grid);
    let row_motion = row_motion_components(cut, grid, storm_motion);
    let width = options.width.max(64);
    let height = options.height.max(64);
    let center_x = (width as f32 - 1.0) / 2.0;
    let center_y = (height as f32 - 1.0) / 2.0;
    let radius_px = center_x.min(center_y) * (f32::from(options.range_fraction) / 100.0);
    let max_range_m = max_range_m(grid).max(1.0);

    let mut pixels = vec![0; width as usize * height as usize * 4];
    let color_tables = ColorTableSet::default();
    let color_table = color_tables.for_family(ColorTableFamily::Velocity);
    let geometry = RasterGeometry {
        width,
        center_x,
        center_y,
        radius_px,
        radius_sq_px: radius_px * radius_px,
        max_range_m,
    };

    match &grid.storage {
        MomentStorage::U8(values) => {
            let row_palettes = build_storm_relative_u8_row_palettes(grid, &row_motion, color_table);
            render_storm_relative_u8_storage(
                &mut pixels,
                values,
                grid,
                &row_lookup,
                &row_palettes,
                geometry,
                false,
            );
        }
        MomentStorage::U16(values) => {
            render_storm_relative_storage(
                &mut pixels,
                values,
                grid,
                &row_lookup,
                StormRelativeValueLookup {
                    row_motion: &row_motion,
                    color_table,
                },
                geometry,
                false,
            );
        }
        MomentStorage::F32(values) => render_storm_relative_f32_storage(
            &mut pixels,
            values,
            grid,
            &row_lookup,
            StormRelativeValueLookup {
                row_motion: &row_motion,
                color_table,
            },
            geometry,
            false,
        ),
    }

    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_storm_relative_velocity_viewport_image(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
    let (width, height, pixels) =
        render_storm_relative_velocity_viewport_rgba(volume, cut_index, storm_motion, options)?;
    Ok(
        ImageBuffer::from_raw(width, height, pixels)
            .expect("RGBA buffer matches raster dimensions"),
    )
}

pub fn render_storm_relative_velocity_viewport_rgba(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
) -> Result<(u32, u32, Vec<u8>)> {
    let (width, height) = viewport_dimensions(options);
    let mut pixels = vec![0; rgba_len(width, height)];
    render_storm_relative_velocity_viewport_rgba_into(
        volume,
        cut_index,
        storm_motion,
        options,
        &mut pixels,
    )?;
    Ok((width, height, pixels))
}

pub fn render_storm_relative_velocity_viewport_rgba_into(
    volume: &RadarVolume,
    cut_index: usize,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
) -> Result<(u32, u32)> {
    let cache = ViewportMomentCache::new(volume, cut_index, MomentType::Velocity)?;
    cache.render_storm_relative_velocity_rgba_into(volume, storm_motion, options, pixels)
}

fn render_storm_relative_velocity_viewport_grid_into(
    cut: &ElevationCut,
    grid: &MomentGrid,
    render_cache: StormRelativeRenderCache<'_>,
    storm_motion: StormMotion,
    options: ViewportRasterOptions,
    pixels: &mut [u8],
    clear_pixels: bool,
) {
    let geometry = viewport_geometry(grid, options);
    let lookup_table = ViewportLookupTable::new(grid, geometry);

    match &grid.storage {
        MomentStorage::U8(values) => {
            let built_palettes;
            let row_palettes = if let Some(palette_cache) = render_cache.palette_cache {
                &palette_cache.row_palettes
            } else {
                let row_motion = render_cache
                    .storm_motion_basis
                    .map(|basis| basis.row_motion_components(storm_motion))
                    .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
                built_palettes = build_storm_relative_u8_row_palettes(
                    grid,
                    &row_motion,
                    render_cache.color_table,
                );
                &built_palettes
            };
            render_storm_relative_u8_viewport_storage(
                pixels,
                values,
                grid,
                render_cache.row_lookup,
                row_palettes,
                &lookup_table,
                clear_pixels,
            );
        }
        MomentStorage::U16(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_viewport_storage(
                pixels,
                values,
                grid,
                render_cache.row_lookup,
                StormRelativeValueLookup {
                    row_motion: &row_motion,
                    color_table: render_cache.color_table,
                },
                &lookup_table,
                clear_pixels,
            );
        }
        MomentStorage::F32(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_f32_viewport_storage(
                pixels,
                values,
                grid,
                render_cache.row_lookup,
                StormRelativeValueLookup {
                    row_motion: &row_motion,
                    color_table: render_cache.color_table,
                },
                &lookup_table,
                clear_pixels,
            );
        }
    }
}

fn render_storm_relative_velocity_sample_cache_grid_into(
    cut: &ElevationCut,
    grid: &MomentGrid,
    render_cache: StormRelativeRenderCache<'_>,
    storm_motion: StormMotion,
    sample_cache: &ViewportSampleCache,
    pixels: &mut [u8],
    clear_pixels: bool,
) {
    match &grid.storage {
        MomentStorage::U8(values) => {
            let built_palettes;
            let row_palettes = if let Some(palette_cache) = render_cache.palette_cache {
                &palette_cache.row_palettes
            } else {
                let row_motion = render_cache
                    .storm_motion_basis
                    .map(|basis| basis.row_motion_components(storm_motion))
                    .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
                built_palettes = build_storm_relative_u8_row_palettes(
                    grid,
                    &row_motion,
                    render_cache.color_table,
                );
                &built_palettes
            };
            render_storm_relative_u8_sample_cache_storage(
                pixels,
                values,
                grid,
                row_palettes,
                sample_cache,
                clear_pixels,
            );
        }
        MomentStorage::U16(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_sample_cache_storage(
                pixels,
                values,
                grid,
                &row_motion,
                render_cache.color_table,
                sample_cache,
                clear_pixels,
            );
        }
        MomentStorage::F32(values) => {
            let row_motion = render_cache
                .storm_motion_basis
                .map(|basis| basis.row_motion_components(storm_motion))
                .unwrap_or_else(|| row_motion_components(cut, grid, storm_motion));
            render_storm_relative_f32_sample_cache_storage(
                pixels,
                values,
                grid,
                &row_motion,
                render_cache.color_table,
                sample_cache,
                clear_pixels,
            );
        }
    }
}

struct StormRelativeRenderCache<'a> {
    row_lookup: &'a AzimuthLookup,
    storm_motion_basis: Option<&'a StormMotionBasis>,
    color_table: &'a ColorTable,
    palette_cache: Option<&'a StormRelativePaletteCache>,
}

#[derive(Clone, Copy)]
struct StormRelativeValueLookup<'a> {
    row_motion: &'a [f32],
    color_table: &'a ColorTable,
}

#[derive(Clone, Copy, Debug)]
struct RasterGeometry {
    width: u32,
    center_x: f32,
    center_y: f32,
    radius_px: f32,
    radius_sq_px: f32,
    max_range_m: f32,
}

#[derive(Clone, Copy, Debug)]
struct ViewportGeometry {
    width: u32,
    radar_x_px: f32,
    radar_y_px: f32,
    km_per_px_x: f32,
    km_per_px_y: f32,
    max_range_km_sq: f32,
}

fn viewport_dimensions(options: ViewportRasterOptions) -> (u32, u32) {
    (options.width.max(1), options.height.max(1))
}

fn viewport_geometry(grid: &MomentGrid, options: ViewportRasterOptions) -> ViewportGeometry {
    let (width, _) = viewport_dimensions(options);
    let max_range_km = max_range_m(grid).max(1.0) / 1000.0;
    ViewportGeometry {
        width,
        radar_x_px: options.radar_x_px,
        radar_y_px: options.radar_y_px,
        km_per_px_x: options.km_per_px_x.max(f32::EPSILON),
        km_per_px_y: options.km_per_px_y.max(f32::EPSILON),
        max_range_km_sq: max_range_km * max_range_km,
    }
}

fn rgba_len(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

fn ensure_rgba_buffer(pixels: &[u8], width: u32, height: u32) -> Result<()> {
    let expected = rgba_len(width, height);
    if pixels.len() == expected {
        Ok(())
    } else {
        Err(RenderError::BufferSizeMismatch {
            actual: pixels.len(),
            expected,
            width,
            height,
        })
    }
}

trait LookupGeometry: Copy + Sync {
    fn width(self) -> u32;
    fn x_range_for_row(self, y: u32) -> Option<Range<u32>>;
    fn lookup(
        self,
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
    ) -> Option<SampleLookup>;
}

impl LookupGeometry for RasterGeometry {
    fn width(self) -> u32 {
        self.width
    }

    fn x_range_for_row(self, _y: u32) -> Option<Range<u32>> {
        Some(0..self.width)
    }

    fn lookup(
        self,
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
    ) -> Option<SampleLookup> {
        raster_lookup(x, y, grid, row_lookup, self)
    }
}

impl LookupGeometry for ViewportGeometry {
    fn width(self) -> u32 {
        self.width
    }

    fn x_range_for_row(self, y: u32) -> Option<Range<u32>> {
        let dy_km = (self.radar_y_px - (y as f32 + 0.5)) * self.km_per_px_y;
        let dy_km_sq = dy_km * dy_km;
        if dy_km_sq > self.max_range_km_sq {
            return None;
        }

        let max_dx_km = (self.max_range_km_sq - dy_km_sq).max(0.0).sqrt();
        let max_dx_px = max_dx_km / self.km_per_px_x;
        let first = (self.radar_x_px - max_dx_px - 0.5).floor() as i64 - 1;
        let last_exclusive = (self.radar_x_px + max_dx_px - 0.5).ceil() as i64 + 2;
        let width = i64::from(self.width);
        let start = first.clamp(0, width) as u32;
        let end = last_exclusive.clamp(0, width) as u32;
        (start < end).then_some(start..end)
    }

    fn lookup(
        self,
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
    ) -> Option<SampleLookup> {
        viewport_lookup(x, y, grid, row_lookup, self)
    }
}

#[derive(Debug)]
struct ViewportLookupTable {
    geometry: ViewportGeometry,
    first_gate_m: f32,
    gate_spacing_m: f32,
    gate_count: usize,
}

impl ViewportLookupTable {
    fn new(grid: &MomentGrid, geometry: ViewportGeometry) -> Self {
        Self {
            geometry,
            first_gate_m: grid.gate_range.first_gate_m as f32,
            gate_spacing_m: grid.gate_range.gate_spacing_m.max(1) as f32,
            gate_count: grid.gate_range.gate_count,
        }
    }

    fn width(&self) -> u32 {
        self.geometry.width
    }

    fn row(&self, y: u32) -> Option<ViewportLookupRow> {
        let dy_km = (self.geometry.radar_y_px - (y as f32 + 0.5)) * self.geometry.km_per_px_y;
        let dy_km_sq = dy_km * dy_km;
        if dy_km_sq > self.geometry.max_range_km_sq {
            return None;
        }

        let max_dx_km = (self.geometry.max_range_km_sq - dy_km_sq).max(0.0).sqrt();
        let max_dx_px = max_dx_km / self.geometry.km_per_px_x;
        let first = (self.geometry.radar_x_px - max_dx_px - 0.5).floor() as i64 - 1;
        let last_exclusive = (self.geometry.radar_x_px + max_dx_px - 0.5).ceil() as i64 + 2;
        let width = i64::from(self.geometry.width);
        let start = first.clamp(0, width) as u32;
        let end = last_exclusive.clamp(0, width) as u32;
        (start < end).then_some(ViewportLookupRow {
            x_range: start..end,
            dy_km,
            dy_km_sq,
            max_range_km_sq: self.geometry.max_range_km_sq,
            radar_x_px: self.geometry.radar_x_px,
            km_per_px_x: self.geometry.km_per_px_x,
            first_gate_m: self.first_gate_m,
            gate_spacing_m: self.gate_spacing_m,
            gate_count: self.gate_count,
        })
    }
}

#[derive(Clone, Debug)]
struct ViewportLookupRow {
    x_range: Range<u32>,
    dy_km: f32,
    dy_km_sq: f32,
    max_range_km_sq: f32,
    radar_x_px: f32,
    km_per_px_x: f32,
    first_gate_m: f32,
    gate_spacing_m: f32,
    gate_count: usize,
}

impl ViewportLookupRow {
    fn lookup(&self, x: u32, row_lookup: &AzimuthLookup) -> Option<SampleLookup> {
        let dx_km = (x as f32 + 0.5 - self.radar_x_px) * self.km_per_px_x;
        let range_km_sq = dx_km.mul_add(dx_km, self.dy_km_sq);
        if range_km_sq > self.max_range_km_sq {
            return None;
        }

        let range_m = range_km_sq.sqrt() * 1000.0;
        let gate = ((range_m - self.first_gate_m) / self.gate_spacing_m).round() as isize;
        if gate < 0 || gate as usize >= self.gate_count {
            return None;
        }

        let azimuth_deg = azimuth_from_xy(dx_km, self.dy_km);
        let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
        Some(SampleLookup {
            azimuth_bin,
            gate: gate as usize,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct CachedViewportGeometry<'a> {
    row_spans: &'a [CachedRowSpan],
    samples: &'a [CachedSample],
}

impl<'a> CachedViewportGeometry<'a> {
    fn row_samples(&self, y: usize) -> Option<(u32, &'a [CachedSample])> {
        let span = self.row_spans.get(y)?;
        let range = span.range()?;
        let start = span.sample_offset;
        let end = start + (range.end - range.start) as usize;
        Some((range.start, &self.samples[start..end]))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SampleLookup {
    azimuth_bin: usize,
    gate: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedSample {
    row: usize,
    gate: usize,
}

trait RawMomentValue: Copy + Sync {
    fn to_usize(self) -> usize;
}

impl RawMomentValue for u8 {
    fn to_usize(self) -> usize {
        usize::from(self)
    }
}

impl RawMomentValue for u16 {
    fn to_usize(self) -> usize {
        usize::from(self)
    }
}

fn render_compact_storage<T: RawMomentValue, G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[T],
    palette: &[[u8; 4]],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let color = palette[raw.to_usize()];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_compact_viewport_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    palette: &[[u8; 4]],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let color = palette[raw.to_usize()];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_compact_sample_cache_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    palette: &[[u8; 4]],
    grid: &MomentGrid,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let index = cached_sample.row() * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                let color = palette[values[index].to_usize()];
                if color[3] != 0 {
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                }
                pixel += 4;
            }
        });
}

fn render_f32_storage<G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    color_table: &ColorTable,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    let sampler = color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(value) = values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let color = sampler.color_for_value(value);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_f32_viewport_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    color_table: &ColorTable,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    let sampler = color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(value) = values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let color = sampler.color_for_value(value);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_f32_sample_cache_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    color_table: &ColorTable,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    let sampler = color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let index = cached_sample.row() * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                let value = values[index];
                if value.is_finite() {
                    let color = sampler.color_for_value(value);
                    if color[3] != 0 {
                        row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    }
                }
                pixel += 4;
            }
        });
}

fn render_storm_relative_storage<T: RawMomentValue, G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[T],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    let sampler = value_lookup.color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied().map(RawMomentValue::to_usize) else {
                        continue;
                    };
                    if grid.nodata == Some(raw as u16) {
                        continue;
                    }
                    let color = if grid.range_folded == Some(raw as u16) {
                        sampler.range_folded_color()
                    } else {
                        let velocity = (raw as f32 - grid.offset) / grid.scale;
                        let relative = velocity
                            - value_lookup
                                .row_motion
                                .get(candidate.row)
                                .copied()
                                .unwrap_or(0.0);
                        sampler.color_for_value(relative)
                    };
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_viewport_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    let sampler = value_lookup.color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied().map(RawMomentValue::to_usize) else {
                        continue;
                    };
                    if grid.nodata == Some(raw as u16) {
                        continue;
                    }
                    let color = if grid.range_folded == Some(raw as u16) {
                        sampler.range_folded_color()
                    } else {
                        let velocity = (raw as f32 - grid.offset) / grid.scale;
                        let relative = velocity
                            - value_lookup
                                .row_motion
                                .get(candidate.row)
                                .copied()
                                .unwrap_or(0.0);
                        sampler.color_for_value(relative)
                    };
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn build_storm_relative_u8_row_palettes(
    grid: &MomentGrid,
    row_motion: &[f32],
    color_table: &ColorTable,
) -> Vec<[[u8; 4]; 256]> {
    let sampler = color_table.sampler();
    row_motion
        .par_iter()
        .map(|motion| {
            let mut palette = [[0, 0, 0, 0]; 256];
            for raw in 0..=u8::MAX {
                palette[usize::from(raw)] =
                    storm_relative_u8_color_for_raw(grid, &sampler, raw, *motion);
            }
            palette
        })
        .collect()
}

fn storm_relative_u8_color_for_raw(
    grid: &MomentGrid,
    sampler: &ColorSampler,
    raw: u8,
    row_motion: f32,
) -> [u8; 4] {
    let raw = u16::from(raw);
    if grid.nodata == Some(raw) {
        return [0, 0, 0, 0];
    }
    if grid.range_folded == Some(raw) {
        return sampler.range_folded_color();
    }
    let velocity = (raw as f32 - grid.offset) / grid.scale;
    sampler.color_for_value(velocity - row_motion)
}

fn render_storm_relative_u8_storage<G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[u8],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    row_palettes: &[[[u8; 4]; 256]],
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let Some(palette) = row_palettes.get(candidate.row) else {
                        continue;
                    };
                    let color = palette[usize::from(raw)];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_u8_viewport_storage(
    pixels: &mut [u8],
    values: &[u8],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    row_palettes: &[[[u8; 4]; 256]],
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(raw) = values.get(index).copied() else {
                        continue;
                    };
                    let Some(palette) = row_palettes.get(candidate.row) else {
                        continue;
                    };
                    let color = palette[usize::from(raw)];
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_u8_sample_cache_storage(
    pixels: &mut [u8],
    values: &[u8],
    grid: &MomentGrid,
    row_palettes: &[[[u8; 4]; 256]],
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let row = cached_sample.row();
                let index = row * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                debug_assert!(row < row_palettes.len());
                let color = row_palettes[row][usize::from(values[index])];
                if color[3] != 0 {
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                }
                pixel += 4;
            }
        });
}

fn render_storm_relative_sample_cache_storage<T: RawMomentValue>(
    pixels: &mut [u8],
    values: &[T],
    grid: &MomentGrid,
    row_motion: &[f32],
    color_table: &ColorTable,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    let sampler = color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let row = cached_sample.row();
                let index = row * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                debug_assert!(row < row_motion.len());
                let raw = values[index].to_usize();
                if grid.nodata == Some(raw as u16) {
                    pixel += 4;
                    continue;
                }
                let color = if grid.range_folded == Some(raw as u16) {
                    sampler.range_folded_color()
                } else {
                    let velocity = (raw as f32 - grid.offset) / grid.scale;
                    let relative = velocity - row_motion[row];
                    sampler.color_for_value(relative)
                };
                if color[3] != 0 {
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                }
                pixel += 4;
            }
        });
}

fn render_storm_relative_f32_storage<G: LookupGeometry>(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    geometry: G,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = geometry.width();
    let row_stride = width as usize * 4;
    let sampler = value_lookup.color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(x_range) = geometry.x_range_for_row(y) else {
                return;
            };
            for x in x_range {
                let Some(sample) = geometry.lookup(x, y, grid, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(velocity) =
                        values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let relative = velocity
                        - value_lookup
                            .row_motion
                            .get(candidate.row)
                            .copied()
                            .unwrap_or(0.0);
                    let color = sampler.color_for_value(relative);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_f32_viewport_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    value_lookup: StormRelativeValueLookup<'_>,
    lookup_table: &ViewportLookupTable,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let width = lookup_table.width();
    let row_stride = width as usize * 4;
    let sampler = value_lookup.color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return;
            };
            for x in row_lookup_table.x_range.clone() {
                let Some(sample) = row_lookup_table.lookup(x, row_lookup) else {
                    continue;
                };
                for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
                    let index = candidate.row * gate_count + sample.gate;
                    let Some(velocity) =
                        values.get(index).copied().filter(|value| value.is_finite())
                    else {
                        continue;
                    };
                    let relative = velocity
                        - value_lookup
                            .row_motion
                            .get(candidate.row)
                            .copied()
                            .unwrap_or(0.0);
                    let color = sampler.color_for_value(relative);
                    if color[3] == 0 {
                        continue;
                    }
                    let pixel = x as usize * 4;
                    row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    break;
                }
            }
        });
}

fn render_storm_relative_f32_sample_cache_storage(
    pixels: &mut [u8],
    values: &[f32],
    grid: &MomentGrid,
    row_motion: &[f32],
    color_table: &ColorTable,
    sample_cache: &ViewportSampleCache,
    clear_pixels: bool,
) {
    let gate_count = grid.gate_range.gate_count;
    let geometry = sample_cache.geometry();
    let width = sample_cache.width as usize;
    let row_stride = width * 4;
    let sampler = color_table.sampler();
    pixels
        .par_chunks_exact_mut(row_stride)
        .enumerate()
        .for_each(|(y, row_pixels)| {
            if clear_pixels {
                row_pixels.fill(0);
            }
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return;
            };
            let mut pixel = row_start_x as usize * 4;
            for cached_sample in row_samples {
                if let Some(skip) = cached_sample.skip_len() {
                    pixel += skip as usize * 4;
                    continue;
                }
                let row = cached_sample.row();
                let index = row * gate_count + cached_sample.gate();
                debug_assert!(index < values.len());
                debug_assert!(row < row_motion.len());
                let velocity = values[index];
                if velocity.is_finite() {
                    let relative = velocity - row_motion[row];
                    let color = sampler.color_for_value(relative);
                    if color[3] != 0 {
                        row_pixels[pixel..pixel + 4].copy_from_slice(&color);
                    }
                }
                pixel += 4;
            }
        });
}

fn build_sample_cache_rows<R>(
    height: u32,
    lookup_table: &ViewportLookupTable,
    row_lookup: &AzimuthLookup,
    resolve: R,
) -> Vec<CachedRowBuild>
where
    R: Fn(SampleLookup) -> Option<ResolvedSample> + Sync,
{
    (0..height as usize)
        .into_par_iter()
        .map(|y| {
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return CachedRowBuild::empty();
            };
            let x_range = row_lookup_table.x_range.clone();
            let x_range_len = x_range.len();
            let mut start = None;
            let mut next_x = 0u32;
            let mut samples = Vec::with_capacity(x_range_len);
            let mut count = 0;
            for x in x_range {
                if let Some(sample) = row_lookup_table.lookup(x, row_lookup).and_then(&resolve)
                    && let Some(cached_sample) = CachedSample::new(sample)
                {
                    let start_x = *start.get_or_insert(x);
                    if samples.is_empty() {
                        next_x = start_x;
                    }
                    if x > next_x {
                        push_cached_sample_skip(&mut samples, x - next_x);
                    }
                    samples.push(cached_sample);
                    count += 1;
                    next_x = x + 1;
                }
            }
            if samples.is_empty() {
                CachedRowBuild::empty()
            } else {
                CachedRowBuild {
                    start: start.expect("non-empty row has a start"),
                    samples,
                    sample_count: count,
                }
            }
        })
        .collect()
}

fn build_geometry_cache_rows(
    height: u32,
    lookup_table: &ViewportLookupTable,
    row_lookup: &AzimuthLookup,
) -> Vec<CachedRowBuild> {
    (0..height as usize)
        .into_par_iter()
        .map(|y| {
            let y = y as u32;
            let Some(row_lookup_table) = lookup_table.row(y) else {
                return CachedRowBuild::empty();
            };
            let x_range = row_lookup_table.x_range.clone();
            let mut start = None;
            let mut next_x = 0u32;
            let mut samples = Vec::with_capacity(x_range.len());
            let mut count = 0usize;
            for x in x_range {
                if let Some(sample) = row_lookup_table.lookup(x, row_lookup)
                    && let Some(cached_sample) = CachedSample::new(ResolvedSample {
                        row: sample.azimuth_bin,
                        gate: sample.gate,
                    })
                {
                    let start_x = *start.get_or_insert(x);
                    if samples.is_empty() {
                        next_x = start_x;
                    }
                    if x > next_x {
                        push_cached_sample_skip(&mut samples, x - next_x);
                    }
                    samples.push(cached_sample);
                    count += 1;
                    next_x = x + 1;
                }
            }
            if samples.is_empty() {
                CachedRowBuild::empty()
            } else {
                CachedRowBuild {
                    start: start.expect("non-empty geometry row has a start"),
                    samples,
                    sample_count: count,
                }
            }
        })
        .collect()
}

fn build_sample_cache_rows_from_geometry<R>(
    height: u32,
    geometry: CachedViewportGeometry<'_>,
    resolve: R,
) -> Vec<CachedRowBuild>
where
    R: Fn(SampleLookup) -> Option<ResolvedSample> + Sync,
{
    (0..height as usize)
        .into_par_iter()
        .map(|y| {
            let Some((row_start_x, row_samples)) = geometry.row_samples(y) else {
                return CachedRowBuild::empty();
            };
            let mut start = None;
            let mut next_x = 0u32;
            let mut x = row_start_x;
            let mut samples = Vec::with_capacity(row_samples.len());
            let mut count = 0usize;
            for cached_lookup in row_samples {
                if let Some(skip) = cached_lookup.skip_len() {
                    x += skip;
                    continue;
                }
                let sample = SampleLookup {
                    azimuth_bin: cached_lookup.row(),
                    gate: cached_lookup.gate(),
                };
                if let Some(sample) = resolve(sample)
                    && let Some(cached_sample) = CachedSample::new(sample)
                {
                    let start_x = *start.get_or_insert(x);
                    if samples.is_empty() {
                        next_x = start_x;
                    }
                    if x > next_x {
                        push_cached_sample_skip(&mut samples, x - next_x);
                    }
                    samples.push(cached_sample);
                    count += 1;
                    next_x = x + 1;
                }
                x += 1;
            }
            if samples.is_empty() {
                CachedRowBuild::empty()
            } else {
                CachedRowBuild {
                    start: start.expect("non-empty resolved geometry row has a start"),
                    samples,
                    sample_count: count,
                }
            }
        })
        .collect()
}

fn viewport_sample_cache_from_rows(
    volume_ptr: usize,
    cut_index: usize,
    moment: MomentType,
    width: u32,
    height: u32,
    row_builds: Vec<CachedRowBuild>,
) -> ViewportSampleCache {
    let (sample_count, row_spans, samples) = flatten_cached_rows(height, row_builds);
    ViewportSampleCache {
        volume_ptr,
        cut_index,
        moment,
        width,
        height,
        sample_count,
        row_spans,
        samples,
    }
}

fn flatten_cached_rows(
    height: u32,
    row_builds: Vec<CachedRowBuild>,
) -> (usize, Vec<CachedRowSpan>, Vec<CachedSample>) {
    let sample_storage_len = row_builds.iter().map(|row| row.samples.len()).sum();
    let mut row_spans = Vec::with_capacity(height as usize);
    let mut samples = Vec::with_capacity(sample_storage_len);
    let mut sample_count = 0;
    for row in row_builds {
        if row.samples.is_empty() {
            row_spans.push(CachedRowSpan::empty());
            continue;
        }
        let sample_offset = samples.len();
        let end = row.start + row.samples.len() as u32;
        sample_count += row.sample_count;
        row_spans.push(CachedRowSpan {
            start: row.start,
            end,
            sample_offset,
        });
        samples.extend(row.samples);
    }
    while row_spans.len() < height as usize {
        row_spans.push(CachedRowSpan::empty());
    }
    (sample_count, row_spans, samples)
}

fn push_cached_sample_skip(samples: &mut Vec<CachedSample>, mut pixel_count: u32) {
    while pixel_count > 0 {
        let chunk = pixel_count.min(CachedSample::SKIP_MASK);
        samples.push(CachedSample::skip(chunk).expect("positive skip chunk fits"));
        pixel_count -= chunk;
    }
}

fn resolve_compact_sample<T: RawMomentValue>(
    values: &[T],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    sample: SampleLookup,
) -> Option<ResolvedSample> {
    let gate_count = grid.gate_range.gate_count;
    for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
        let index = candidate.row * gate_count + sample.gate;
        if index >= values.len() {
            continue;
        }
        let raw = values[index].to_usize() as u16;
        if grid.nodata == Some(raw) {
            continue;
        }
        return Some(ResolvedSample {
            row: candidate.row,
            gate: sample.gate,
        });
    }
    None
}

fn resolve_f32_sample(
    values: &[f32],
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    sample: SampleLookup,
) -> Option<ResolvedSample> {
    let gate_count = grid.gate_range.gate_count;
    for candidate in row_lookup.candidates_for_bin(sample.azimuth_bin) {
        let index = candidate.row * gate_count + sample.gate;
        if index < values.len() && values[index].is_finite() {
            return Some(ResolvedSample {
                row: candidate.row,
                gate: sample.gate,
            });
        }
    }
    None
}

fn raster_lookup(
    x: u32,
    y: u32,
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    geometry: RasterGeometry,
) -> Option<SampleLookup> {
    let dx = x as f32 - geometry.center_x;
    let dy = geometry.center_y - y as f32;
    let radius_sq = dx.mul_add(dx, dy * dy);
    if radius_sq > geometry.radius_sq_px {
        return None;
    }

    let radius = radius_sq.sqrt();
    let range_m = radius / geometry.radius_px * geometry.max_range_m;
    let gate = ((range_m - grid.gate_range.first_gate_m as f32)
        / grid.gate_range.gate_spacing_m.max(1) as f32)
        .round() as isize;
    if gate < 0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }

    let azimuth_deg = azimuth_from_xy(dx, dy);
    let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
    Some(SampleLookup {
        azimuth_bin,
        gate: gate as usize,
    })
}

fn viewport_lookup(
    x: u32,
    y: u32,
    grid: &MomentGrid,
    row_lookup: &AzimuthLookup,
    geometry: ViewportGeometry,
) -> Option<SampleLookup> {
    let dx_km = (x as f32 + 0.5 - geometry.radar_x_px) * geometry.km_per_px_x;
    let dy_km = (geometry.radar_y_px - (y as f32 + 0.5)) * geometry.km_per_px_y;
    let range_km_sq = dx_km.mul_add(dx_km, dy_km * dy_km);
    if range_km_sq > geometry.max_range_km_sq {
        return None;
    }

    let range_m = range_km_sq.sqrt() * 1000.0;
    let gate = ((range_m - grid.gate_range.first_gate_m as f32)
        / grid.gate_range.gate_spacing_m.max(1) as f32)
        .round() as isize;
    if gate < 0 || gate as usize >= grid.gate_range.gate_count {
        return None;
    }

    let azimuth_deg = azimuth_from_xy(dx_km, dy_km);
    let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
    Some(SampleLookup {
        azimuth_bin,
        gate: gate as usize,
    })
}

fn build_u8_palette(grid: &MomentGrid, color_table: &ColorTable) -> [[u8; 4]; 256] {
    let sampler = color_table.sampler();
    let mut palette = [[0, 0, 0, 0]; 256];
    for raw in 0..=u8::MAX {
        palette[usize::from(raw)] = color_for_raw(grid, &sampler, u16::from(raw));
    }
    palette
}

fn build_u16_palette(grid: &MomentGrid, color_table: &ColorTable) -> Vec<[u8; 4]> {
    let sampler = color_table.sampler();
    let max_raw = match &grid.storage {
        MomentStorage::U16(values) => values.iter().copied().max().unwrap_or(0),
        _ => u16::MAX,
    };
    let mut palette = vec![[0, 0, 0, 0]; usize::from(max_raw) + 1];
    for raw in 0..=max_raw {
        palette[usize::from(raw)] = color_for_raw(grid, &sampler, raw);
    }
    palette
}

fn color_for_raw(grid: &MomentGrid, sampler: &ColorSampler, raw: u16) -> [u8; 4] {
    if grid.nodata == Some(raw) {
        return [0, 0, 0, 0];
    }
    if grid.range_folded == Some(raw) {
        return sampler.range_folded_color();
    }
    sampler.color_for_value((raw as f32 - grid.offset) / grid.scale)
}

/// Dealias (unfold) a base velocity moment.
///
/// This is a **region-based** unfolder, not a gate-by-gate radial walk. A
/// radial/gate-sequential continuity scheme (the previous implementation)
/// propagates a single bad fold down an entire ray, producing the radial
/// "spokes" that plague high-shear convection (derechos, mesocyclones). The
/// region-based approach decides whole coherent regions at once and lets
/// genuine discontinuities remain at region boundaries, so an error cannot
/// run down a radial. See Feldmann et al. (2020, *R2D2*, JTECH-D-20-0054.1),
/// Jing & Wiener (1993), and Py-ART's `dealias_region_based`
/// (Helmus & Collis 2016).
///
/// Steps: (1) flood-fill connected regions whose neighbouring gates differ by
/// less than half a Nyquist (so no fold occurs *within* a region); (2) build a
/// region-adjacency graph whose edges carry the integer Nyquist fold between
/// the two regions (the consensus over all shared boundary gate-pairs);
/// (3) resolve folds strongest-boundary-first via a union-find with per-node
/// fold offset; (4) anchor each connected group so its largest region is
/// unfolded (fold 0); (5) apply and despeckle.
pub fn dealias_velocity_grid(cut: &ElevationCut, source: &MomentGrid) -> MomentGrid {
    let rows = source.radial_count();
    let gate_count = source.gate_range.gate_count;
    let total = rows.saturating_mul(gate_count);
    let fallback_nyquist = median_nyquist_mps(cut, source);

    // Per-row Nyquist (NaN where unknown) and observed velocities (NaN for
    // no-data / range-folded gates, which never join a region).
    let mut nyq = vec![f32::NAN; rows.max(1)];
    for (row, slot) in nyq.iter_mut().enumerate().take(rows) {
        *slot = row_nyquist_mps(cut, source, row)
            .or(fallback_nyquist)
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(f32::NAN);
    }

    let mut observed = vec![f32::NAN; total];
    if total > 0 {
        let mut row_buf = vec![f32::NAN; gate_count];
        for row in 0..rows {
            copy_scaled_velocity_row(source, row, &mut row_buf);
            observed[row * gate_count..(row + 1) * gate_count].copy_from_slice(&row_buf);
        }
    }

    let azimuths = radial_azimuths(cut, source);
    let folds = region_based_dealias_folds(&observed, &nyq, rows, gate_count, &azimuths);

    let mut corrected = vec![DEALIASED_VELOCITY_NODATA; total];
    #[allow(clippy::needless_range_loop)]
    for row in 0..rows {
        let n = nyq[row];
        for gate in 0..gate_count {
            let idx = row * gate_count + gate;
            let v = observed[idx];
            if !v.is_finite() {
                continue;
            }
            let value = if n.is_finite() {
                v + 2.0 * n * folds[idx] as f32
            } else {
                v
            };
            corrected[idx] = encode_dealiased_velocity(value);
        }
    }

    despeckle_dealiased_velocity(&mut corrected, &nyq, rows, gate_count);

    MomentGrid {
        moment: MomentType::Velocity,
        gate_range: source.gate_range.clone(),
        scale: DEALIASED_VELOCITY_SCALE,
        offset: DEALIASED_VELOCITY_OFFSET,
        nodata: Some(DEALIASED_VELOCITY_NODATA),
        range_folded: None,
        radial_indices: source.radial_indices.clone(),
        storage: MomentStorage::U16(corrected),
    }
}

/// Fraction of the Nyquist interval below which two adjacent gates are assumed
/// to belong to the same (unfolded) region. Comfortably below 1.0 so a true
/// fold (a jump of ~2·Nyquist) always lands on a region boundary.
const REGION_JOIN_FRAC: f32 = 0.5;
/// Hard cap on the integer fold count applied to any region.
const REGION_MAX_FOLD: i32 = 5;
/// Minimum shared-boundary support for an inter-region fold to be trusted.
/// One is enough: region interiors are already coherent, so a single boundary
/// pair is a valid estimate, and because edges resolve strongest-first a lone
/// spurious contact only ever resolves last (as a no-op cycle).
const REGION_EDGE_MIN_SUPPORT: u32 = 1;

/// Plain union-find for connected-component region labelling.
struct UnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let parent = self.parent[x as usize];
            self.parent[x as usize] = self.parent[parent as usize]; // path halving
            x = self.parent[x as usize];
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (ra, rb) = if self.rank[ra as usize] < self.rank[rb as usize] {
            (rb, ra)
        } else {
            (ra, rb)
        };
        self.parent[rb as usize] = ra;
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] += 1;
        }
    }
}

/// Union-find that also tracks an integer fold offset to the group root, so we
/// can accumulate "region B is k Nyquist intervals above region A" relations
/// and solve them all consistently (a weighted/potential DSU).
struct FoldUnionFind {
    parent: Vec<u32>,
    rank: Vec<u8>,
    /// fold[x] = k[x] - k[parent[x]]
    offset: Vec<i32>,
}

impl FoldUnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
            offset: vec![0; n],
        }
    }

    /// Returns (root, k[x] - k[root]).
    fn find(&mut self, x: u32) -> (u32, i32) {
        let mut node = x;
        let mut total = 0;
        while self.parent[node as usize] != node {
            total += self.offset[node as usize];
            node = self.parent[node as usize];
        }
        (node, total)
    }

    /// Enforce k[b] - k[a] = rel by merging the two groups.
    fn union(&mut self, a: u32, b: u32, rel: i32) {
        let (ra, oa) = self.find(a);
        let (rb, ob) = self.find(b);
        if ra == rb {
            return;
        }
        // k[rb] - k[ra] = rel + oa - ob
        let delta = rel + oa - ob;
        if self.rank[ra as usize] < self.rank[rb as usize] {
            self.parent[ra as usize] = rb;
            self.offset[ra as usize] = -delta;
        } else {
            self.parent[rb as usize] = ra;
            self.offset[rb as usize] = delta;
            if self.rank[ra as usize] == self.rank[rb as usize] {
                self.rank[ra as usize] += 1;
            }
        }
    }
}

fn radial_azimuths(cut: &ElevationCut, grid: &MomentGrid) -> Vec<f32> {
    grid.radial_indices
        .iter()
        .map(|ri| {
            cut.radials
                .get(*ri)
                .map(|r| r.azimuth_deg.rem_euclid(360.0))
                .unwrap_or(f32::NAN)
        })
        .collect()
}

/// Whether row order closes a full 360° sweep (so the last radial is azimuthally
/// adjacent to the first). True for any normal NEXRAD PPI.
fn sweep_wraps(azimuths: &[f32]) -> bool {
    let rows = azimuths.len();
    if rows < 8 {
        return false;
    }
    let (Some(first), Some(last)) = (azimuths.first(), azimuths.last()) else {
        return false;
    };
    if !first.is_finite() || !last.is_finite() {
        return false;
    }
    let gap = (first - last)
        .rem_euclid(360.0)
        .min((last - first).rem_euclid(360.0));
    let typical = 360.0 / rows as f32;
    gap <= 3.0 * typical
}

/// Core region-based fold solver. Returns the integer Nyquist fold for every
/// gate (0 where unknown / no data).
fn region_based_dealias_folds(
    observed: &[f32],
    nyq: &[f32],
    rows: usize,
    gates: usize,
    azimuths: &[f32],
) -> Vec<i32> {
    let total = rows.saturating_mul(gates);
    let mut folds = vec![0i32; total];
    // Union-find nodes are indexed by `idx as u32`; bail (no unfolding) rather
    // than truncate if a grid were ever absurdly large. Real sweeps are ~1.3M
    // gates, far under u32::MAX.
    if total == 0 || observed.len() != total || total > u32::MAX as usize {
        return folds;
    }

    let same_region = |a: usize, b: usize, n: f32| -> bool {
        n.is_finite()
            && observed[a].is_finite()
            && observed[b].is_finite()
            && (observed[a] - observed[b]).abs() <= REGION_JOIN_FRAC * n
    };

    // ---- 1. label connected regions ----
    let mut labels = UnionFind::new(total);
    let wrap = sweep_wraps(azimuths);
    for row in 0..rows {
        let row_n = nyq[row];
        for gate in 0..gates {
            let idx = row * gates + gate;
            if !observed[idx].is_finite() {
                continue;
            }
            if gate + 1 < gates && same_region(idx, idx + 1, row_n) {
                labels.union(idx as u32, (idx + 1) as u32);
            }
            if row + 1 < rows {
                let down = (row + 1) * gates + gate;
                let n = row_n.min(nyq[row + 1]);
                if same_region(idx, down, n) {
                    labels.union(idx as u32, down as u32);
                }
            }
        }
    }
    if wrap {
        let n = nyq[rows - 1].min(nyq[0]);
        for gate in 0..gates {
            let a = (rows - 1) * gates + gate;
            let b = gate;
            if same_region(a, b, n) {
                labels.union(a as u32, b as u32);
            }
        }
    }

    // Compact region ids + sizes.
    let mut region_of = vec![u32::MAX; total];
    let mut region_size: Vec<u32> = Vec::new();
    for idx in 0..total {
        if !observed[idx].is_finite() {
            continue;
        }
        let root = labels.find(idx as u32);
        let rid = &mut region_of[root as usize];
        if *rid == u32::MAX {
            *rid = region_size.len() as u32;
            region_size.push(0);
        }
        let rid = *rid;
        region_of[idx] = rid;
        region_size[rid as usize] += 1;
    }
    let region_count = region_size.len();
    if region_count == 0 {
        return folds;
    }

    // ---- 2. accumulate inter-region fold votes over shared boundaries ----
    // key: (lo_region, hi_region) -> map fold -> count, where fold f means
    // k[hi] = k[lo] + f, f = round((v_lo - v_hi) / (2·Nyquist)).
    let mut edges: std::collections::HashMap<(u32, u32), std::collections::HashMap<i32, u32>> =
        std::collections::HashMap::new();
    let mut vote = |ra: u32, va: f32, rb: u32, vb: f32, n: f32| {
        if ra == rb || !n.is_finite() {
            return;
        }
        let (lo, vlo, hi, vhi) = if ra < rb {
            (ra, va, rb, vb)
        } else {
            (rb, vb, ra, va)
        };
        let f = ((vlo - vhi) / (2.0 * n)).round() as i32;
        if f.abs() > 2 * REGION_MAX_FOLD {
            return;
        }
        *edges.entry((lo, hi)).or_default().entry(f).or_insert(0) += 1;
    };
    for row in 0..rows {
        let row_n = nyq[row];
        for gate in 0..gates {
            let idx = row * gates + gate;
            let ra = region_of[idx];
            if ra == u32::MAX {
                continue;
            }
            if gate + 1 < gates {
                let rb = region_of[idx + 1];
                if rb != u32::MAX {
                    vote(ra, observed[idx], rb, observed[idx + 1], row_n);
                }
            }
            if row + 1 < rows {
                let down = (row + 1) * gates + gate;
                let rb = region_of[down];
                if rb != u32::MAX {
                    vote(
                        ra,
                        observed[idx],
                        rb,
                        observed[down],
                        row_n.min(nyq[row + 1]),
                    );
                }
            }
        }
    }
    if wrap {
        let n = nyq[rows - 1].min(nyq[0]);
        for gate in 0..gates {
            let a = (rows - 1) * gates + gate;
            let b = gate;
            let (ra, rb) = (region_of[a], region_of[b]);
            if ra != u32::MAX && rb != u32::MAX {
                vote(ra, observed[a], rb, observed[b], n);
            }
        }
    }

    // ---- 3. resolve folds, strongest shared boundary first ----
    let mut resolved: Vec<((u32, u32), i32, u32)> = edges
        .into_iter()
        .filter_map(|(key, votes)| {
            let total_support: u32 = votes.values().sum();
            // Tied vote counts must resolve deterministically (HashMap
            // iteration order varies per instance): prefer the smaller
            // |fold|, then the smaller fold.
            let (fold, support) = votes.into_iter().max_by_key(|(fold, count)| {
                (
                    *count,
                    std::cmp::Reverse(fold.abs()),
                    std::cmp::Reverse(*fold),
                )
            })?;
            (support >= REGION_EDGE_MIN_SUPPORT).then_some((key, fold, total_support))
        })
        .collect();
    // Strongest boundary first; tie-break on the (unique) region pair so the
    // union order — and therefore the unfolded field — is reproducible.
    resolved.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

    let mut dsu = FoldUnionFind::new(region_count);
    for ((lo, hi), fold, _) in resolved {
        dsu.union(lo, hi, fold);
    }

    // ---- 4. anchor each connected group so its largest region is unfolded ----
    // For every group root, remember the offset of the biggest region in it.
    let mut anchor_offset: std::collections::HashMap<u32, (u32, i32)> =
        std::collections::HashMap::new();
    for rid in 0..region_count as u32 {
        let (root, off) = dsu.find(rid);
        let size = region_size[rid as usize];
        anchor_offset
            .entry(root)
            .and_modify(|(best_size, best_off)| {
                if size > *best_size {
                    *best_size = size;
                    *best_off = off;
                }
            })
            .or_insert((size, off));
    }

    // ---- 5. per-gate fold = (region offset) - (anchor offset of its group) ----
    let mut region_fold = vec![0i32; region_count];
    for rid in 0..region_count as u32 {
        let (root, off) = dsu.find(rid);
        let anchor = anchor_offset.get(&root).map(|(_, o)| *o).unwrap_or(0);
        region_fold[rid as usize] = (off - anchor).clamp(-REGION_MAX_FOLD, REGION_MAX_FOLD);
    }
    for idx in 0..total {
        let rid = region_of[idx];
        if rid != u32::MAX {
            folds[idx] = region_fold[rid as usize];
        }
    }
    folds
}

/// Remove isolated single-gate outliers from the unfolded field: a gate whose
/// decoded velocity differs from the median of its finite 4-neighbours by more
/// than a Nyquist is snapped toward that median (dual-PRF/processor speckle;
/// Holleman & Beekhuis 2003, Altube et al. 2017).
fn despeckle_dealiased_velocity(corrected: &mut [u16], nyq: &[f32], rows: usize, gates: usize) {
    if rows < 3 || gates < 3 || corrected.len() != rows.saturating_mul(gates) {
        return;
    }
    let snapshot = corrected.to_vec();
    let decode = |raw: u16| decode_dealiased_velocity(raw);
    #[allow(clippy::needless_range_loop)]
    for row in 0..rows {
        let n = nyq[row];
        if !n.is_finite() {
            continue;
        }
        for gate in 0..gates {
            let idx = row * gates + gate;
            let Some(v) = decode(snapshot[idx]) else {
                continue;
            };
            let mut neigh = [0.0f32; 4];
            let mut count = 0;
            for (nr, ng) in [
                (row.wrapping_sub(1), gate),
                (row + 1, gate),
                (row, gate.wrapping_sub(1)),
                (row, gate + 1),
            ] {
                if nr >= rows || ng >= gates {
                    continue;
                }
                if let Some(nv) = decode(snapshot[nr * gates + ng]) {
                    neigh[count] = nv;
                    count += 1;
                }
            }
            if count < 3 {
                continue;
            }
            let median = median_small_f32(&mut neigh, count);
            if (v - median).abs() > n {
                // collapse the outlier onto the nearest Nyquist multiple of the
                // local consensus.
                let fold = ((median - v) / (2.0 * n)).round();
                corrected[idx] = encode_dealiased_velocity(v + 2.0 * n * fold);
            }
        }
    }
}

const DEALIASED_VELOCITY_SCALE: f32 = 10.0;
const DEALIASED_VELOCITY_OFFSET: f32 = 32_768.0;
const DEALIASED_VELOCITY_NODATA: u16 = 0;

fn encode_dealiased_velocity(value: f32) -> u16 {
    if !value.is_finite() {
        return DEALIASED_VELOCITY_NODATA;
    }
    (value * DEALIASED_VELOCITY_SCALE + DEALIASED_VELOCITY_OFFSET)
        .round()
        .clamp(1.0, u16::MAX as f32) as u16
}

fn decode_dealiased_velocity(raw: u16) -> Option<f32> {
    if raw == DEALIASED_VELOCITY_NODATA {
        return None;
    }
    Some((raw as f32 - DEALIASED_VELOCITY_OFFSET) / DEALIASED_VELOCITY_SCALE)
}

fn copy_scaled_velocity_row(source: &MomentGrid, row: usize, row_values: &mut [f32]) {
    row_values.fill(f32::NAN);
    let gate_count = source.gate_range.gate_count;
    if gate_count == 0 || row_values.len() != gate_count {
        return;
    }
    let Some(row_start) = row.checked_mul(gate_count) else {
        return;
    };
    let row_end = row_start + gate_count;
    match &source.storage {
        MomentStorage::U8(values) => {
            let Some(raw_row) = values.get(row_start..row_end) else {
                return;
            };
            for (raw, value) in raw_row.iter().zip(row_values.iter_mut()) {
                let raw = u16::from(*raw);
                if source.nodata == Some(raw) || source.range_folded == Some(raw) {
                    continue;
                }
                *value = (raw as f32 - source.offset) / source.scale;
            }
        }
        MomentStorage::U16(values) => {
            let Some(raw_row) = values.get(row_start..row_end) else {
                return;
            };
            for (raw, value) in raw_row.iter().zip(row_values.iter_mut()) {
                if source.nodata == Some(*raw) || source.range_folded == Some(*raw) {
                    continue;
                }
                *value = (*raw as f32 - source.offset) / source.scale;
            }
        }
        MomentStorage::F32(values) => {
            let Some(source_row) = values.get(row_start..row_end) else {
                return;
            };
            row_values.copy_from_slice(source_row);
        }
    }
}

fn median_nyquist_mps(cut: &ElevationCut, grid: &MomentGrid) -> Option<f32> {
    let mut values = grid
        .radial_indices
        .iter()
        .filter_map(|radial_index| cut.radials.get(*radial_index)?.nyquist_velocity_mps)
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_by(f32::total_cmp);
    Some(values[values.len() / 2])
}

fn row_nyquist_mps(cut: &ElevationCut, grid: &MomentGrid, row: usize) -> Option<f32> {
    let radial_index = *grid.radial_indices.get(row)?;
    cut.radials.get(radial_index)?.nyquist_velocity_mps
}

fn median_small_f32(values: &mut [f32], count: usize) -> f32 {
    debug_assert!(count > 0 && count <= values.len());
    values[..count].sort_by(f32::total_cmp);
    values[count / 2]
}

fn max_range_m(grid: &MomentGrid) -> f32 {
    grid.gate_range.first_gate_m as f32
        + grid.gate_range.gate_spacing_m as f32 * grid.gate_range.gate_count as f32
}

fn azimuth_from_xy(dx: f32, dy: f32) -> f32 {
    let mut degrees = dx.atan2(dy) * 180.0 / PI;
    if degrees < 0.0 {
        degrees += 360.0;
    }
    degrees
}

struct AzimuthLookup {
    bins: Vec<AzimuthBin>,
}

impl AzimuthLookup {
    fn new(cut: &ElevationCut, grid: &MomentGrid) -> Self {
        let mut groups = vec![None; AZIMUTH_BINS];
        for (row, radial_index) in grid.radial_indices.iter().enumerate() {
            let Some(radial) = cut.radials.get(*radial_index) else {
                continue;
            };
            let azimuth = radial.azimuth_deg.rem_euclid(360.0);
            let bin = azimuth_bin(azimuth);
            let group = groups[bin].get_or_insert_with(|| AzimuthGroup {
                azimuth: bin as f32 * AZIMUTH_BIN_WIDTH_DEG,
                candidates: Vec::new(),
            });
            group.candidates.push(RowCandidate {
                row,
                valid_extent: row_valid_extent(grid, row),
            });
        }

        let mut groups = groups.into_iter().flatten().collect::<Vec<_>>();
        for group in &mut groups {
            group
                .candidates
                .sort_by_key(|candidate| std::cmp::Reverse(candidate.rank()));
        }
        groups.sort_by(|left, right| left.azimuth.total_cmp(&right.azimuth));

        let mut bins = vec![AzimuthBin::default(); AZIMUTH_BINS];
        if groups.is_empty() {
            return Self { bins };
        }
        if groups.len() == 1 {
            fill_azimuth_bins(&mut bins, 0.0, 360.0, &groups[0].candidates);
            return Self { bins };
        }

        for index in 0..groups.len() {
            let group = &groups[index];
            let prev_azimuth = groups
                .get(index.wrapping_sub(1))
                .or_else(|| groups.last())
                .map(|group| group.azimuth)
                .unwrap_or(group.azimuth);
            let next_azimuth = groups
                .get(index + 1)
                .or_else(|| groups.first())
                .map(|group| group.azimuth)
                .unwrap_or(group.azimuth);
            let left_width = (clockwise_delta_deg(prev_azimuth, group.azimuth) * 0.5)
                .min(MAX_AZIMUTH_HALF_WIDTH_DEG);
            let right_width = (clockwise_delta_deg(group.azimuth, next_azimuth) * 0.5)
                .min(MAX_AZIMUTH_HALF_WIDTH_DEG);
            fill_azimuth_bins(
                &mut bins,
                group.azimuth - left_width,
                group.azimuth + right_width,
                &group.candidates,
            );
        }

        Self { bins }
    }

    #[cfg(test)]
    fn row_for_azimuth(&self, azimuth_deg: f32) -> Option<usize> {
        self.candidates_for_bin(self.filled_bin_for_azimuth(azimuth_deg)?)
            .first()
            .map(|candidate| candidate.row)
    }

    fn filled_bin_for_azimuth(&self, azimuth_deg: f32) -> Option<usize> {
        let bin = azimuth_bin(azimuth_deg);
        (!self.bins[bin].is_empty()).then_some(bin)
    }

    fn candidates_for_bin(&self, bin: usize) -> &[RowCandidate] {
        self.bins[bin].candidates()
    }
}

#[derive(Clone, Copy, Debug)]
struct RowCandidate {
    row: usize,
    valid_extent: usize,
}

impl RowCandidate {
    fn rank(self) -> (usize, usize) {
        (self.valid_extent, self.row)
    }
}

impl Default for RowCandidate {
    fn default() -> Self {
        Self {
            row: usize::MAX,
            valid_extent: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct AzimuthGroup {
    azimuth: f32,
    candidates: Vec<RowCandidate>,
}

#[derive(Clone, Copy, Debug)]
struct AzimuthBin {
    candidates: [RowCandidate; MAX_AZIMUTH_CANDIDATES],
    len: usize,
}

impl Default for AzimuthBin {
    fn default() -> Self {
        Self {
            candidates: [RowCandidate::default(); MAX_AZIMUTH_CANDIDATES],
            len: 0,
        }
    }
}

impl AzimuthBin {
    fn is_empty(self) -> bool {
        self.len == 0
    }

    fn candidates(&self) -> &[RowCandidate] {
        &self.candidates[..self.len]
    }

    fn push_candidate(&mut self, candidate: RowCandidate) {
        if self
            .candidates()
            .iter()
            .any(|existing| existing.row == candidate.row)
        {
            return;
        }

        let insert_at = self
            .candidates()
            .iter()
            .position(|existing| candidate.rank() > existing.rank())
            .unwrap_or(self.len);
        if self.len < MAX_AZIMUTH_CANDIDATES {
            for index in (insert_at..self.len).rev() {
                self.candidates[index + 1] = self.candidates[index];
            }
            self.candidates[insert_at] = candidate;
            self.len += 1;
        } else if insert_at < MAX_AZIMUTH_CANDIDATES {
            for index in (insert_at..MAX_AZIMUTH_CANDIDATES - 1).rev() {
                self.candidates[index + 1] = self.candidates[index];
            }
            self.candidates[insert_at] = candidate;
        }
    }
}

fn azimuth_bin(azimuth_deg: f32) -> usize {
    ((azimuth_deg.rem_euclid(360.0) / AZIMUTH_BIN_WIDTH_DEG).round() as usize) % AZIMUTH_BINS
}

fn row_valid_extent(grid: &MomentGrid, row: usize) -> usize {
    let gate_count = grid.gate_range.gate_count;
    let start = row.saturating_mul(gate_count);
    let Some(end) = start.checked_add(gate_count) else {
        return 0;
    };
    match &grid.storage {
        MomentStorage::U8(values) => values
            .get(start..end)
            .and_then(|row| {
                row.iter().rposition(|raw| {
                    let raw = u16::from(*raw);
                    grid.nodata != Some(raw)
                })
            })
            .map(|gate| gate + 1)
            .unwrap_or(0),
        MomentStorage::U16(values) => values
            .get(start..end)
            .and_then(|row| row.iter().rposition(|raw| grid.nodata != Some(*raw)))
            .map(|gate| gate + 1)
            .unwrap_or(0),
        MomentStorage::F32(values) => values
            .get(start..end)
            .and_then(|row| row.iter().rposition(|value| value.is_finite()))
            .map(|gate| gate + 1)
            .unwrap_or(0),
    }
}

fn fill_azimuth_bins(bins: &mut [AzimuthBin], start_deg: f32, end_deg: f32, rows: &[RowCandidate]) {
    let start_bin = (start_deg / AZIMUTH_BIN_WIDTH_DEG).floor() as i32;
    let end_bin = (end_deg / AZIMUTH_BIN_WIDTH_DEG).ceil() as i32;
    for bin in start_bin..=end_bin {
        let target = &mut bins[bin.rem_euclid(AZIMUTH_BINS as i32) as usize];
        for row in rows {
            target.push_candidate(*row);
        }
    }
}

fn clockwise_delta_deg(from_deg: f32, to_deg: f32) -> f32 {
    (to_deg - from_deg).rem_euclid(360.0)
}

fn row_motion_components(
    cut: &ElevationCut,
    grid: &MomentGrid,
    storm_motion: StormMotion,
) -> Vec<f32> {
    grid.radial_indices
        .iter()
        .map(|radial_index| {
            cut.radials
                .get(*radial_index)
                .map(|radial| motion_component_away_mps(storm_motion, radial.azimuth_deg))
                .unwrap_or(0.0)
        })
        .collect()
}

pub fn storm_relative_velocity_mps(
    radar_velocity_mps: f32,
    beam_azimuth_deg: f32,
    storm_motion: StormMotion,
) -> f32 {
    radar_velocity_mps - motion_component_away_mps(storm_motion, beam_azimuth_deg)
}

fn motion_component_away_mps(storm_motion: StormMotion, beam_azimuth_deg: f32) -> f32 {
    let delta = (storm_motion.direction_deg - beam_azimuth_deg).to_radians();
    storm_motion.speed_mps * delta.cos()
}

pub fn color_family_for_moment(moment: &MomentType) -> ColorTableFamily {
    match moment {
        MomentType::Reflectivity => ColorTableFamily::Reflectivity,
        MomentType::Velocity => ColorTableFamily::Velocity,
        MomentType::SpectrumWidth => ColorTableFamily::SpectrumWidth,
        MomentType::CorrelationCoefficient => ColorTableFamily::CorrelationCoefficient,
        MomentType::DifferentialReflectivity => ColorTableFamily::DifferentialReflectivity,
        MomentType::DifferentialPhase => ColorTableFamily::DifferentialPhase,
        MomentType::SpecificDifferentialPhase => ColorTableFamily::SpecificDifferentialPhase,
        _ => ColorTableFamily::Generic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radar_core::{GateRange, MomentRow, RadarSite, RadarVolume, Radial};

    #[test]
    fn base_layer_starts_visible() {
        assert!(RenderLayer::base(MomentType::Reflectivity).visible);
    }

    #[test]
    fn azimuth_places_north_at_zero_degrees() {
        assert_eq!(azimuth_from_xy(0.0, 1.0).round(), 0.0);
        assert_eq!(azimuth_from_xy(1.0, 0.0).round(), 90.0);
        assert_eq!(azimuth_from_xy(0.0, -1.0).round(), 180.0);
        assert_eq!(azimuth_from_xy(-1.0, 0.0).round(), 270.0);
    }

    #[test]
    fn velocity_table_has_a_hard_zero_boundary() {
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Velocity);
        let inbound = table.color_for_value(-2.0);
        let outbound = table.color_for_value(2.0);
        let neutral = table.color_for_value(0.0);

        assert_ne!(inbound, outbound);
        assert_ne!(neutral, inbound);
        assert_ne!(neutral, outbound);
    }

    #[test]
    fn range_folded_gates_are_visible() {
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Velocity);

        assert_eq!(table.range_folded_color()[3], 245);
    }

    #[test]
    fn velocity_range_folded_bins_render_table_rf_color() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Velocity);

        assert_eq!(
            color_for_raw(grid, &table.sampler(), 1),
            table.range_folded_color()
        );
    }

    #[test]
    fn reflectivity_range_folded_bins_render_table_rf_color() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let tables = ColorTableSet::default();
        let table = tables.for_family(ColorTableFamily::Reflectivity);

        assert_eq!(
            color_for_raw(grid, &table.sampler(), 1),
            table.range_folded_color()
        );
    }

    #[test]
    fn lightweight_velocity_dealias_unfolds_radial_continuity() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 5,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.radials.push(Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: Some(10.0),
            radial_status: None,
        });
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: vec![0],
            storage: MomentStorage::F32(vec![0.0, 5.0, 9.0, -9.0, -7.0]),
        };

        let corrected = dealias_velocity_grid(&cut, &grid);
        assert!(matches!(corrected.storage, MomentStorage::U16(_)));

        let values = (0..corrected.gate_range.gate_count)
            .map(|gate| corrected.scaled_value(0, gate).expect("corrected gate"))
            .collect::<Vec<_>>();
        assert_eq!(values, vec![0.0, 5.0, 9.0, 11.0, 13.0]);
    }

    #[test]
    fn region_dealias_recovers_smooth_folded_ramp() {
        // A smooth radial velocity ramp from -34 to +34 m/s with Nyquist 20 is
        // aliased into [-20, 20]; the region-based unfolder must recover the
        // smooth field (up to a global 2·Nyquist constant from anchoring).
        let nyq = 20.0f32;
        let gates = 24usize;
        let rows = 12usize;
        let truth: Vec<f32> = (0..gates)
            .map(|g| -34.0 + 68.0 * g as f32 / (gates as f32 - 1.0))
            .collect();
        let alias = |v: f32| -> f32 {
            let mut a = v;
            while a > nyq {
                a -= 2.0 * nyq;
            }
            while a < -nyq {
                a += 2.0 * nyq;
            }
            a
        };
        let observed_row: Vec<f32> = truth.iter().map(|v| alias(*v)).collect();
        // The raw row genuinely folds (large gate-to-gate jumps present).
        let raw_jumps = observed_row
            .windows(2)
            .filter(|w| (w[0] - w[1]).abs() > nyq)
            .count();
        assert!(raw_jumps >= 1, "test fixture must actually alias");

        let rows_data: Vec<Vec<f32>> = (0..rows).map(|_| observed_row.clone()).collect();
        let (mut cut, grid) = test_velocity_grid_rows(rows_data);
        for radial in &mut cut.radials {
            radial.nyquist_velocity_mps = Some(nyq);
        }

        let corrected = dealias_velocity_grid(&cut, &grid);
        let recovered: Vec<f32> = (0..gates)
            .map(|g| corrected.scaled_value(rows / 2, g).expect("gate"))
            .collect();

        // 1) the unfolded field is smooth: no gate-to-gate jump exceeds Nyquist.
        for w in recovered.windows(2) {
            assert!(
                (w[0] - w[1]).abs() <= nyq,
                "residual fold in dealiased ramp: {w:?}"
            );
        }
        // 2) it matches the truth up to a single constant multiple of 2·Nyquist.
        let offset = recovered[0] - truth[0];
        let folds = (offset / (2.0 * nyq)).round();
        assert!(
            (offset - folds * 2.0 * nyq).abs() < 1.0,
            "offset not a fold multiple: {offset}"
        );
        for (r, t) in recovered.iter().zip(truth.iter()) {
            assert!(
                (r - (t + folds * 2.0 * nyq)).abs() < 1.0,
                "recovered {r} != truth {t} (+{folds} folds)"
            );
        }
    }

    #[test]
    fn region_dealias_does_not_propagate_errors_down_a_radial() {
        // The classic spoke failure: one ambiguous gate near the radar must not
        // flip the entire downrange radial. Two radials of identical, coherent,
        // sub-Nyquist data should come back essentially unchanged (no fold).
        let nyq = 20.0f32;
        let coherent = vec![2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0, 16.0];
        let rows_data: Vec<Vec<f32>> = (0..16).map(|_| coherent.clone()).collect();
        let (mut cut, grid) = test_velocity_grid_rows(rows_data);
        for radial in &mut cut.radials {
            radial.nyquist_velocity_mps = Some(nyq);
        }

        let corrected = dealias_velocity_grid(&cut, &grid);
        for g in 0..coherent.len() {
            assert_eq!(
                corrected.scaled_value(5, g),
                Some(coherent[g]),
                "coherent gate {g} should be untouched"
            );
        }
    }

    #[test]
    fn region_dealias_is_deterministic_across_runs() {
        // Same input must always produce the same unfolded field: edge
        // resolution order and tied fold votes must not depend on HashMap
        // iteration order (which differs per HashMap instance).
        let nyq = 20.0f32;
        let rows = 24usize;
        let gates = 40usize;
        let mut seed = 0x2468_ace1_u32;
        let mut lcg = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as f32 / 65_536.0
        };
        let rows_data: Vec<Vec<f32>> = (0..rows)
            .map(|row| {
                (0..gates)
                    .map(|gate| {
                        let patch = (row / 3) * 7 + gate / 5;
                        let base = match patch % 4 {
                            0 => -38.0,
                            1 => -2.0,
                            2 => 18.5,
                            _ => 39.0,
                        };
                        let v = base + (lcg() - 0.5) * 6.0;
                        let mut aliased = v;
                        while aliased > nyq {
                            aliased -= 2.0 * nyq;
                        }
                        while aliased < -nyq {
                            aliased += 2.0 * nyq;
                        }
                        aliased
                    })
                    .collect()
            })
            .collect();
        let (mut cut, grid) = test_velocity_grid_rows(rows_data);
        for radial in &mut cut.radials {
            radial.nyquist_velocity_mps = Some(nyq);
        }

        let reference = dealias_velocity_grid(&cut, &grid);
        for run in 0..16 {
            let corrected = dealias_velocity_grid(&cut, &grid);
            assert_eq!(
                corrected.storage, reference.storage,
                "dealias output changed between identical runs (run {run})"
            );
        }
    }

    #[test]
    fn region_dealias_unfolds_geometrically_supported_fold() {
        // A folded 2-gate segment surrounded on three sides by data that
        // consistently implies one fold is unfolded (the old radial-walk code
        // wrongly "suppressed" this as a spike and left the alias in place).
        let quiet = vec![0.0, 3.0, 5.0, 7.0, 8.0];
        let folded = vec![0.0, 5.0, 9.0, -9.0, -7.0];
        let (cut, grid) = test_velocity_grid_rows(vec![
            quiet.clone(),
            quiet.clone(),
            folded,
            quiet.clone(),
            quiet,
        ]);

        let corrected = dealias_velocity_grid(&cut, &grid);

        assert_eq!(corrected.scaled_value(2, 3), Some(11.0));
        assert_eq!(corrected.scaled_value(2, 4), Some(13.0));
    }

    #[test]
    fn velocity_dealias_preserves_supported_adjacent_folds() {
        let quiet = vec![0.0, 3.0, 5.0, 7.0, 8.0];
        let folded = vec![0.0, 5.0, 9.0, -9.0, -7.0];
        let (cut, grid) = test_velocity_grid_rows(vec![
            quiet.clone(),
            folded.clone(),
            folded.clone(),
            folded,
            quiet,
        ]);

        let corrected = dealias_velocity_grid(&cut, &grid);

        assert_eq!(corrected.scaled_value(2, 3), Some(11.0));
        assert_eq!(corrected.scaled_value(2, 4), Some(13.0));
    }

    #[test]
    fn storm_relative_u8_row_palette_matches_direct_color_math() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let tables = ColorTableSet::default();
        let color_table = tables.for_family(ColorTableFamily::Velocity);
        let row_motion = [3.25];
        let palettes = build_storm_relative_u8_row_palettes(grid, &row_motion, color_table);

        for raw in [0, 1, 119, 129, 139] {
            assert_eq!(
                palettes[0][usize::from(raw)],
                storm_relative_u8_color_for_raw(grid, &color_table.sampler(), raw, row_motion[0])
            );
        }
    }

    #[test]
    fn custom_color_table_feeds_precomputed_u8_palette() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let table = ColorTable::parse(
            "unit test velocity",
            "units: m/s\ncolor: -20 1 2 3\ncolor: 0 10 20 30\ncolor: 20 40 50 60",
        )
        .expect("custom color table");

        let palette = build_u8_palette(grid, &table);

        assert_eq!(palette[64], [10, 20, 30, 255]);
        assert_eq!(palette[74], [25, 35, 45, 255]);
    }

    #[test]
    fn storm_relative_velocity_subtracts_motion_along_beam() {
        let storm_motion = StormMotion {
            direction_deg: 0.0,
            speed_mps: 10.0,
        };

        assert_eq!(
            storm_relative_velocity_mps(10.0, 0.0, storm_motion).round(),
            0.0
        );
        assert_eq!(
            storm_relative_velocity_mps(10.0, 180.0, storm_motion).round(),
            20.0
        );
        assert_eq!(
            storm_relative_velocity_mps(10.0, 90.0, storm_motion).round(),
            10.0
        );
    }

    #[test]
    fn storm_motion_basis_matches_direct_projection() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Velocity)
            .expect("velocity grid");
        let basis = StormMotionBasis::new(cut, grid);
        let storm_motion = StormMotion {
            direction_deg: 225.0,
            speed_mps: 18.0,
        };
        let row_motion = basis.row_motion_components(storm_motion);

        for (row, radial_index) in grid.radial_indices.iter().enumerate() {
            let radial = &cut.radials[*radial_index];
            let direct = motion_component_away_mps(storm_motion, radial.azimuth_deg);
            assert!((row_motion[row] - direct).abs() < 0.000_01);
        }
    }

    #[test]
    fn cached_sample_packs_lookup_into_four_bytes() {
        assert_eq!(std::mem::size_of::<CachedSample>(), 4);

        let sample = ResolvedSample {
            row: 3_599,
            gate: 1_832,
        };
        let cached = CachedSample::new(sample).expect("sample fits packed cache entry");

        assert_eq!(cached.sample(), Some(sample));
        let skip = CachedSample::skip(37).expect("skip fits packed cache entry");
        assert_eq!(skip.skip_len(), Some(37));
        assert_eq!(skip.sample(), None);
        assert_eq!(
            CachedSample::new(ResolvedSample {
                row: CachedSample::ROW_LIMIT,
                gate: 0
            }),
            None
        );
    }

    #[test]
    fn sample_cache_storage_upper_bound_scales_with_viewport_pixels() {
        let options = ViewportRasterOptions {
            width: 1_920,
            height: 1_080,
            radar_x_px: 960.0,
            radar_y_px: 540.0,
            km_per_px_x: 1.0,
            km_per_px_y: 1.0,
        };

        assert_eq!(
            viewport_sample_cache_storage_upper_bound(options),
            1_920 * 1_080 * std::mem::size_of::<CachedSample>()
                + 1_080 * std::mem::size_of::<CachedRowSpan>()
        );
    }

    #[test]
    fn grid_sample_cache_upper_bound_tracks_actual_radar_footprint() {
        let volume = test_volume();
        let grid = volume.cuts[0]
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let options = ViewportRasterOptions {
            width: 1_920,
            height: 1_080,
            radar_x_px: 960.0,
            radar_y_px: 540.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let full_viewport = viewport_sample_cache_storage_upper_bound(options);
        let radar_footprint = viewport_sample_cache_storage_upper_bound_for_grid(grid, options);

        assert!(radar_footprint < full_viewport);
        assert!(radar_footprint > 1_080 * std::mem::size_of::<CachedRowSpan>());
    }

    #[test]
    fn viewport_lookup_matches_reference_hypot_formula() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let row_lookup = AzimuthLookup::new(cut, grid);
        let max_range_m = max_range_m(grid).max(1.0);
        let max_range_km = max_range_m / 1000.0;
        let geometry = ViewportGeometry {
            width: 333,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            max_range_km_sq: max_range_km * max_range_km,
        };

        for (x, y) in [(0, 0), (166, 108), (180, 110), (220, 70), (332, 216)] {
            assert_eq!(
                viewport_lookup(x, y, grid, &row_lookup, geometry),
                viewport_lookup_reference(x, y, grid, &row_lookup, geometry)
            );
        }
    }

    #[test]
    fn viewport_lookup_table_matches_reference_hypot_formula() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let row_lookup = AzimuthLookup::new(cut, grid);
        let geometry = viewport_geometry(
            grid,
            ViewportRasterOptions {
                width: 333,
                height: 217,
                radar_x_px: 166.5,
                radar_y_px: 108.5,
                km_per_px_x: 0.5,
                km_per_px_y: 0.5,
            },
        );
        let lookup_table = ViewportLookupTable::new(grid, geometry);

        for y in [0, 10, 70, 108, 140, 216] {
            for x in [0, 20, 120, 166, 180, 260, 332] {
                let table_sample = lookup_table.row(y).and_then(|row| {
                    row.x_range
                        .contains(&x)
                        .then(|| row.lookup(x, &row_lookup))
                        .flatten()
                });
                assert_eq!(
                    table_sample,
                    viewport_lookup_reference(x, y, grid, &row_lookup, geometry),
                    "lookup mismatch at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn viewport_row_span_covers_reference_samples() {
        let volume = test_volume();
        let cut = &volume.cuts[0];
        let grid = cut
            .moments
            .get(&MomentType::Reflectivity)
            .expect("reflectivity grid");
        let row_lookup = AzimuthLookup::new(cut, grid);
        let max_range_m = max_range_m(grid).max(1.0);
        let max_range_km = max_range_m / 1000.0;
        let geometry = ViewportGeometry {
            width: 96,
            radar_x_px: 48.0,
            radar_y_px: 48.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
            max_range_km_sq: max_range_km * max_range_km,
        };

        for y in 0..96 {
            let span = geometry.x_range_for_row(y);
            for x in 0..96 {
                if viewport_lookup_reference(x, y, grid, &row_lookup, geometry).is_some() {
                    assert!(
                        span.as_ref().is_some_and(|range| range.contains(&x)),
                        "row span missed reference sample at ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn azimuth_lookup_fills_wider_native_radial_sectors() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 1,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );

        for index in 0..180 {
            cut.radials.push(Radial {
                azimuth_deg: index as f32 * 2.0,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
            grid.push_u8_row_slice(index, &[20]).expect("radial row");
        }

        let lookup = AzimuthLookup::new(&cut, &grid);
        assert!(lookup.row_for_azimuth(1.0).is_some());
        assert!(lookup.row_for_azimuth(181.0).is_some());
    }

    #[test]
    fn azimuth_lookup_prefers_duplicate_row_with_longer_valid_extent() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 4,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        for azimuth_deg in [0.0, 0.0, 2.0, 4.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }
        grid.push_u8_row_slice(0, &[20, 0, 0, 0])
            .expect("short duplicate row");
        grid.push_u8_row_slice(1, &[20, 30, 40, 50])
            .expect("long duplicate row");
        grid.push_u8_row_slice(2, &[20, 30, 40, 50])
            .expect("neighbor row");
        grid.push_u8_row_slice(3, &[20, 30, 40, 50])
            .expect("neighbor row");

        let lookup = AzimuthLookup::new(&cut, &grid);
        assert_eq!(lookup.row_for_azimuth(0.0), Some(1));
        assert_eq!(row_valid_extent(&grid, 0), 1);
        assert_eq!(row_valid_extent(&grid, 1), 4);

        let sample = SampleLookup {
            azimuth_bin: azimuth_bin(0.0),
            gate: 3,
        };
        let MomentStorage::U8(values) = &grid.storage else {
            panic!("test grid should use u8 storage");
        };
        let resolved =
            resolve_compact_sample(values, &grid, &lookup, sample).expect("sample should resolve");
        assert_eq!(resolved.row, 1);
        assert_eq!(resolved.gate, 3);
    }

    #[test]
    fn compact_sample_resolution_keeps_visible_range_folded_candidates() {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 4,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        cut.radials.push(Radial {
            azimuth_deg: 0.0,
            elevation_deg: 0.5,
            time_offset_ms: 0,
            gate_range: gate_range.clone(),
            nyquist_velocity_mps: None,
            radial_status: None,
        });
        grid.push_u8_row_slice(0, &[1, 1, 1, 1])
            .expect("range-folded row");

        let lookup = AzimuthLookup::new(&cut, &grid);
        assert_eq!(row_valid_extent(&grid, 0), 4);

        let MomentStorage::U8(values) = &grid.storage else {
            panic!("test grid should use u8 storage");
        };
        let resolved = resolve_compact_sample(
            values,
            &grid,
            &lookup,
            SampleLookup {
                azimuth_bin: azimuth_bin(0.0),
                gate: 3,
            },
        )
        .expect("range-folded sample should resolve");

        assert_eq!(resolved.row, 0);
        assert_eq!(resolved.gate, 3);
    }

    #[test]
    fn viewport_render_uses_requested_screen_resolution() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let reflectivity =
            render_moment_viewport_image(&volume, 0, MomentType::Reflectivity, options)
                .expect("viewport reflectivity");
        assert_eq!(reflectivity.dimensions(), (333, 217));
        assert!(has_visible_pixel(reflectivity.as_raw()));

        let mut reusable_pixels = vec![255; viewport_rgba_buffer_len(options)];
        let dimensions = render_moment_viewport_rgba_into(
            &volume,
            0,
            MomentType::Reflectivity,
            options,
            &mut reusable_pixels,
        )
        .expect("viewport reflectivity into reusable buffer");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&reusable_pixels));
        assert!(has_transparent_pixel(&reusable_pixels));

        let reflectivity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport reflectivity cache");
        reusable_pixels.fill(255);
        let dimensions = reflectivity_cache
            .render_moment_rgba_into(&volume, options, &mut reusable_pixels)
            .expect("cached viewport reflectivity");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&reusable_pixels));
        assert!(has_transparent_pixel(&reusable_pixels));

        let storm_relative = render_storm_relative_velocity_viewport_image(
            &volume,
            0,
            StormMotion {
                direction_deg: 45.0,
                speed_mps: 10.0,
            },
            options,
        )
        .expect("viewport storm-relative velocity");
        assert_eq!(storm_relative.dimensions(), (333, 217));
        assert!(has_visible_pixel(storm_relative.as_raw()));

        let mut storm_relative_pixels = vec![255; viewport_rgba_buffer_len(options)];
        let dimensions = render_storm_relative_velocity_viewport_rgba_into(
            &volume,
            0,
            StormMotion {
                direction_deg: 45.0,
                speed_mps: 10.0,
            },
            options,
            &mut storm_relative_pixels,
        )
        .expect("viewport storm-relative velocity into reusable buffer");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&storm_relative_pixels));
        assert!(has_transparent_pixel(&storm_relative_pixels));

        let velocity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Velocity)
            .expect("viewport velocity cache");
        storm_relative_pixels.fill(255);
        let dimensions = velocity_cache
            .render_storm_relative_velocity_rgba_into(
                &volume,
                StormMotion {
                    direction_deg: 45.0,
                    speed_mps: 10.0,
                },
                options,
                &mut storm_relative_pixels,
            )
            .expect("cached viewport storm-relative velocity");
        assert_eq!(dimensions, (333, 217));
        assert!(has_visible_pixel(&storm_relative_pixels));
        assert!(has_transparent_pixel(&storm_relative_pixels));
    }

    #[test]
    fn viewport_sample_cache_matches_direct_moment_render() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport reflectivity cache");
        let sample_cache = cache
            .build_sample_cache(&volume, options)
            .expect("viewport sample cache");
        let mut direct_pixels = vec![0; viewport_rgba_buffer_len(options)];
        let mut sample_cache_pixels = vec![255; viewport_rgba_buffer_len(options)];

        cache
            .render_moment_rgba_into(&volume, options, &mut direct_pixels)
            .expect("direct viewport render");
        let dimensions = cache
            .render_moment_rgba_with_sample_cache(&volume, &sample_cache, &mut sample_cache_pixels)
            .expect("sample-cache viewport render");

        assert_eq!(dimensions, (333, 217));
        assert_eq!(sample_cache.dimensions(), (333, 217));
        assert!(sample_cache.sample_count() > 0);
        assert!(sample_cache.storage_bytes() < viewport_rgba_buffer_len(options));
        assert_eq!(sample_cache_pixels, direct_pixels);

        let mut reused_pixels = direct_pixels.clone();
        cache
            .render_moment_rgba_with_sample_cache_reusing_transparency(
                &volume,
                &sample_cache,
                &mut reused_pixels,
            )
            .expect("sample-cache reuse viewport render");
        assert_eq!(reused_pixels, sample_cache_pixels);
    }

    #[test]
    fn viewport_geometry_cache_resolves_across_compatible_products() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let reflectivity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        let velocity_cache =
            ViewportMomentCache::new(&volume, 0, MomentType::Velocity).expect("velocity cache");
        let geometry_cache = reflectivity_cache
            .build_geometry_cache(&volume, options)
            .expect("geometry cache");
        let geometry_sample_cache = velocity_cache
            .build_sample_cache_from_geometry_cache(&volume, &geometry_cache)
            .expect("velocity sample cache from geometry");
        let direct_sample_cache = velocity_cache
            .build_sample_cache(&volume, options)
            .expect("direct velocity sample cache");
        let mut geometry_pixels = vec![255; viewport_rgba_buffer_len(options)];
        let mut direct_pixels = vec![255; viewport_rgba_buffer_len(options)];

        velocity_cache
            .render_moment_rgba_with_sample_cache(
                &volume,
                &geometry_sample_cache,
                &mut geometry_pixels,
            )
            .expect("geometry-derived sample render");
        velocity_cache
            .render_moment_rgba_with_sample_cache(&volume, &direct_sample_cache, &mut direct_pixels)
            .expect("direct sample render");

        assert_eq!(geometry_cache.dimensions(), (333, 217));
        assert!(geometry_cache.sample_count() >= geometry_sample_cache.sample_count());
        assert_eq!(geometry_pixels, direct_pixels);
    }

    #[test]
    fn viewport_sample_cache_matches_direct_storm_relative_render() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let storm_motion = StormMotion {
            direction_deg: 45.0,
            speed_mps: 10.0,
        };
        let cache =
            ViewportMomentCache::new(&volume, 0, MomentType::Velocity).expect("velocity cache");
        let sample_cache = cache
            .build_sample_cache(&volume, options)
            .expect("velocity sample cache");
        let mut direct_pixels = vec![0; viewport_rgba_buffer_len(options)];
        let mut sample_cache_pixels = vec![255; viewport_rgba_buffer_len(options)];

        cache
            .render_storm_relative_velocity_rgba_into(
                &volume,
                storm_motion,
                options,
                &mut direct_pixels,
            )
            .expect("direct SRV viewport render");
        let dimensions = cache
            .render_storm_relative_velocity_rgba_with_sample_cache(
                &volume,
                storm_motion,
                &sample_cache,
                &mut sample_cache_pixels,
            )
            .expect("sample-cache SRV viewport render");

        assert_eq!(dimensions, (333, 217));
        assert_eq!(sample_cache_pixels, direct_pixels);

        let next_storm_motion = StormMotion {
            direction_deg: 220.0,
            speed_mps: 18.0,
        };
        let mut cleared_next_pixels = vec![255; viewport_rgba_buffer_len(options)];
        cache
            .render_storm_relative_velocity_rgba_with_sample_cache(
                &volume,
                next_storm_motion,
                &sample_cache,
                &mut cleared_next_pixels,
            )
            .expect("cleared next SRV viewport render");

        let mut reused_next_pixels = sample_cache_pixels;
        cache
            .render_storm_relative_velocity_rgba_with_sample_cache_reusing_transparency(
                &volume,
                next_storm_motion,
                &sample_cache,
                &mut reused_next_pixels,
            )
            .expect("reused next SRV viewport render");
        assert_eq!(reused_next_pixels, cleared_next_pixels);
    }

    #[test]
    fn viewport_sample_cache_rejects_mismatched_cache() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 64,
            height: 64,
            radar_x_px: 32.0,
            radar_y_px: 32.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let reflectivity_cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("reflectivity cache");
        let velocity_cache =
            ViewportMomentCache::new(&volume, 0, MomentType::Velocity).expect("velocity cache");
        let sample_cache = reflectivity_cache
            .build_sample_cache(&volume, options)
            .expect("reflectivity sample cache");
        let mut pixels = vec![0; viewport_rgba_buffer_len(options)];

        let err = velocity_cache
            .render_moment_rgba_with_sample_cache(&volume, &sample_cache, &mut pixels)
            .expect_err("sample cache should be moment-bound");

        assert!(matches!(
            err,
            RenderError::CacheMomentMismatch {
                expected: MomentType::Velocity,
                actual: MomentType::Reflectivity
            }
        ));
    }

    #[test]
    fn viewport_render_rejects_wrong_sized_reusable_buffer() {
        let volume = test_volume();
        let options = ViewportRasterOptions {
            width: 333,
            height: 217,
            radar_x_px: 166.5,
            radar_y_px: 108.5,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };

        let mut pixels = vec![0; viewport_rgba_buffer_len(options) - 4];
        let err = render_moment_viewport_rgba_into(
            &volume,
            0,
            MomentType::Reflectivity,
            options,
            &mut pixels,
        )
        .expect_err("wrong buffer size should be rejected");

        assert!(matches!(err, RenderError::BufferSizeMismatch { .. }));
    }

    #[test]
    fn viewport_cache_rejects_different_volume() {
        let volume = test_volume();
        let other_volume = test_volume();
        let options = ViewportRasterOptions {
            width: 64,
            height: 64,
            radar_x_px: 32.0,
            radar_y_px: 32.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport reflectivity cache");
        let mut pixels = vec![0; viewport_rgba_buffer_len(options)];

        let err = cache
            .render_moment_rgba_into(&other_volume, options, &mut pixels)
            .expect_err("cache should be bound to its source volume");

        assert!(matches!(err, RenderError::CacheVolumeMismatch));
    }

    #[test]
    fn viewport_cache_renders_u16_palette_moments() {
        let volume = test_u16_volume();
        let options = ViewportRasterOptions {
            width: 96,
            height: 96,
            radar_x_px: 48.0,
            radar_y_px: 48.0,
            km_per_px_x: 0.5,
            km_per_px_y: 0.5,
        };
        let cache = ViewportMomentCache::new(&volume, 0, MomentType::Reflectivity)
            .expect("viewport u16 reflectivity cache");
        let mut pixels = vec![255; viewport_rgba_buffer_len(options)];

        let dimensions = cache
            .render_moment_rgba_into(&volume, options, &mut pixels)
            .expect("cached u16 viewport reflectivity");

        assert_eq!(dimensions, (96, 96));
        assert!(has_visible_pixel(&pixels));
        assert!(has_transparent_pixel(&pixels));
    }

    fn has_visible_pixel(pixels: &[u8]) -> bool {
        pixels.chunks_exact(4).any(|pixel| pixel[3] != 0)
    }

    fn has_transparent_pixel(pixels: &[u8]) -> bool {
        pixels.chunks_exact(4).any(|pixel| pixel[3] == 0)
    }

    fn viewport_lookup_reference(
        x: u32,
        y: u32,
        grid: &MomentGrid,
        row_lookup: &AzimuthLookup,
        geometry: ViewportGeometry,
    ) -> Option<SampleLookup> {
        let dx_km = (x as f32 + 0.5 - geometry.radar_x_px) * geometry.km_per_px_x;
        let dy_km = (geometry.radar_y_px - (y as f32 + 0.5)) * geometry.km_per_px_y;
        let range_m = dx_km.hypot(dy_km) * 1000.0;
        let max_range_m = geometry.max_range_km_sq.sqrt() * 1000.0;
        if range_m > max_range_m {
            return None;
        }

        let gate = ((range_m - grid.gate_range.first_gate_m as f32)
            / grid.gate_range.gate_spacing_m.max(1) as f32)
            .round() as isize;
        if gate < 0 || gate as usize >= grid.gate_range.gate_count {
            return None;
        }

        let azimuth_deg = azimuth_from_xy(dx_km, dy_km);
        let azimuth_bin = row_lookup.filled_bin_for_azimuth(azimuth_deg)?;
        Some(SampleLookup {
            azimuth_bin,
            gate: gate as usize,
        })
    }

    fn test_velocity_grid_rows(rows: Vec<Vec<f32>>) -> (ElevationCut, MomentGrid) {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: rows.first().map(Vec::len).unwrap_or(0),
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for index in 0..rows.len() {
            cut.radials.push(Radial {
                azimuth_deg: index as f32,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(10.0),
                radial_status: None,
            });
        }
        let grid = MomentGrid {
            moment: MomentType::Velocity,
            gate_range,
            scale: 1.0,
            offset: 0.0,
            nodata: None,
            range_folded: None,
            radial_indices: (0..cut.radials.len()).collect(),
            storage: MomentStorage::F32(rows.into_iter().flatten().collect()),
        };
        (cut, grid)
    }

    fn test_volume() -> RadarVolume {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 6,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for azimuth_deg in [0.0, 90.0, 180.0, 270.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: Some(32.0),
                radial_status: None,
            });
        }

        let mut reflectivity = MomentGrid::new_u8(
            MomentType::Reflectivity,
            gate_range.clone(),
            1.0,
            0.0,
            Some(0),
            Some(1),
        );
        let mut velocity = MomentGrid::new_u8(
            MomentType::Velocity,
            gate_range,
            1.0,
            64.0,
            Some(0),
            Some(1),
        );
        for radial_index in 0..4 {
            reflectivity
                .push_u8_row_slice(radial_index, &[20, 30, 40, 50, 60, 70])
                .expect("reflectivity row");
            velocity
                .push_u8_row_slice(radial_index, &[44, 54, 64, 74, 84, 94])
                .expect("velocity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);
        cut.moments.insert(MomentType::Velocity, velocity);

        let mut volume = RadarVolume::new(RadarSite::new("TST"), chrono::Utc::now());
        volume.cuts.push(cut);
        volume
    }

    fn test_u16_volume() -> RadarVolume {
        let gate_range = GateRange {
            first_gate_m: 0,
            gate_spacing_m: 1_000,
            gate_count: 6,
        };
        let mut cut = ElevationCut::new(0.5, Some(1));
        for azimuth_deg in [0.0, 90.0, 180.0, 270.0] {
            cut.radials.push(Radial {
                azimuth_deg,
                elevation_deg: 0.5,
                time_offset_ms: 0,
                gate_range: gate_range.clone(),
                nyquist_velocity_mps: None,
                radial_status: None,
            });
        }

        let mut reflectivity = MomentGrid::new_u16(
            MomentType::Reflectivity,
            gate_range,
            2.0,
            64.0,
            Some(0),
            Some(1),
        );
        for radial_index in 0..4 {
            reflectivity
                .push_row(
                    radial_index,
                    MomentRow::U16(vec![80, 100, 120, 140, 160, 180]),
                )
                .expect("u16 reflectivity row");
        }
        cut.moments.insert(MomentType::Reflectivity, reflectivity);

        let mut volume = RadarVolume::new(RadarSite::new("U16"), chrono::Utc::now());
        volume.cuts.push(cut);
        volume
    }
}
