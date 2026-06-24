//! 3D Volume Explorer: GPU direct-volume rendering of the reflectivity
//! volume plus a radar underlay anchored to the true z=0 ground plane.
//!
//! `render2d::volume_box_resample` builds a Cartesian reflectivity box around
//! the current map center on a background thread. The box is uploaded as an
//! R8 3D texture. A second R8 texture carries the best complete low-level PPI
//! over the same horizontal footprint. The fragment shader can render that
//! PPI or a column-maximum projection on the floor, then ray-march the volume
//! in front of it.
//!
//! Camera parameters are shared by the shader and the CPU annotation
//! projection. Keeping one dynamic z-span and one perspective scale fixes the
//! old apparent "floating floor" caused by the wireframe using viewport width
//! for its vertical projection while the shader used viewport height.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use radar_core::{MomentGrid, MomentType, RadarVolume};
use rayon::prelude::*;
use std::sync::{Arc, Mutex, mpsc};

pub const BOX_N: usize = 192;
pub const BOX_NZ: usize = 48;
/// A sharper floor than the volume lattice, while retaining a 512-byte row
/// pitch that is friendly to texture uploads.
pub const FLOOR_N: usize = 512;
pub const BOX_HALF_KM: f32 = 60.0;
pub const BOX_TOP_M: f32 = 18_000.0;
const MAX_SHADER_STEPS: usize = 256;
const UNIFORM_FLOATS: usize = 28;
const UNIFORM_BYTES: u64 = (UNIFORM_FLOATS * std::mem::size_of::<f32>()) as u64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloorMode {
    Off,
    LowestTilt,
    ColumnMax,
}

impl FloorMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::LowestTilt, Self::ColumnMax];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::LowestTilt => "Lowest tilt PPI",
            Self::ColumnMax => "Column max",
        }
    }

    pub fn shader_value(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::LowestTilt => 1.0,
            Self::ColumnMax => 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dThresholdMode {
    Above,
    Below,
    Outside,
}

impl Vol3dThresholdMode {
    pub fn shader_value(self) -> f32 {
        match self {
            Self::Above => 0.0,
            Self::Below => 1.0,
            Self::Outside => 2.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dQuality {
    Draft,
    Balanced,
    High,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Vol3dCameraMode {
    Orbit,
    Fly,
}

impl Vol3dCameraMode {
    pub fn shader_value(self) -> f32 {
        match self {
            Self::Orbit => 0.0,
            Self::Fly => 1.0,
        }
    }
}

impl Vol3dQuality {
    pub const ALL: [Self; 3] = [Self::Draft, Self::Balanced, Self::High];

    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Balanced => "Balanced",
            Self::High => "High",
        }
    }

    pub fn steps(self) -> usize {
        match self {
            Self::Draft => 96,
            Self::Balanced => 160,
            Self::High => 240,
        }
    }
}

/// UI-thread state.
pub struct Vol3d {
    pub open: bool,
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub camera_mode: Vol3dCameraMode,
    pub fly_x: f32,
    pub fly_y: f32,
    pub fly_z: f32,
    pub fly_speed: f32,
    pub threshold_dbz: f32,
    pub threshold_mode: Vol3dThresholdMode,
    pub opacity: f32,
    /// Optical-depth multiplier. Opacity controls each sample; density controls
    /// how quickly repeated samples accumulate into a solid echo body.
    pub density: f32,
    /// Gradient lighting strength, 0 = flat palette color, 1 = full shading.
    pub shading: f32,
    pub quality: Vol3dQuality,
    pub floor_mode: FloorMode,
    pub floor_opacity: f32,
    pub floor_threshold_dbz: f32,
    pub floor_threshold_mode: Vol3dThresholdMode,
    /// Purely visual vertical exaggeration. 1x preserves physical proportions
    /// for every box size; the default 2x matches the useful operational look
    /// of the original 120 km box without making larger boxes progressively
    /// more exaggerated.
    pub vertical_exaggeration: f32,
    /// Perspective coefficient used by the shader and CPU projection.
    pub fov_scale: f32,
    /// Camera look-at height, km above the floor.
    pub focus_height_km: f32,
    /// Visible vertical slab, km above the radar.
    pub clip_bottom_km: f32,
    pub clip_top_km: f32,
    pub show_grid: bool,
    pub show_box: bool,
    pub show_labels: bool,
    pub resample_rx: Option<mpsc::Receiver<Option<VolumeBox>>>,
    /// (site id, product label, volume_time_ms, source volume pointer, top
    /// product elevation in tenths of a degree, center east/10km, center
    /// north/10km, box half km). Top-tilt and pointer inclusion make
    /// completing/replaced live volumes re-trigger the resample.
    pub volume_key: Option<(String, String, i64, usize, i32, i32, i32, i32)>,
    /// Top product elevation of the last fully-built box. Live partial volumes
    /// do not replace a complete box with a one-sector fragment.
    pub last_top_deg: f32,
    /// Box half-width, km (60/120/180 = 120/240/360 km boxes).
    pub box_half_km: f32,
    /// Optional fixed map target chosen from the radar canvas. When absent,
    /// the 3D box follows the current map center as before.
    pub box_target_lonlat: Option<(f32, f32)>,
    /// Product label used by the currently selected 3D volume.
    pub product_label: String,
    /// Current box center relative to the radar; used for the radar marker.
    pub box_center_east_km: f32,
    pub box_center_north_km: f32,
    pub status: String,
    /// Last uploaded reflectivity LUT signature.
    pub lut_signature: u64,
    /// Uploads waiting for the GPU, drained in `prepare`.
    pub pending: Arc<Mutex<PendingUploads>>,
    /// Inline advanced control groups. These are not popup menus because egui
    /// combo boxes close immediately when nested inside a parent popup.
    pub show_volume_controls: bool,
    pub show_floor_controls: bool,
}

#[derive(Default)]
pub struct PendingUploads {
    pub volume: Option<VolumeBox>,
    pub lut: Option<Vec<u8>>,
}

pub struct VolumeBox {
    pub data: Vec<u8>,
    pub n: usize,
    pub nz: usize,
    /// Raw normalized dBZ for the low-level floor. Rows run south-to-north,
    /// exactly like the y dimension of `volume_box_resample`.
    pub floor_data: Option<Vec<u8>>,
    pub floor_elevation_deg: Option<f32>,
}

type CameraBasis = ([f32; 3], [f32; 3], [f32; 3], [f32; 3]);

impl Default for Vol3d {
    fn default() -> Self {
        Self {
            open: false,
            yaw: 0.6,
            pitch: 0.45,
            dist: 2.4,
            camera_mode: Vol3dCameraMode::Orbit,
            fly_x: 0.0,
            fly_y: -2.4,
            fly_z: 1.0,
            fly_speed: 1.2,
            threshold_dbz: 35.0,
            threshold_mode: Vol3dThresholdMode::Above,
            opacity: 0.55,
            density: 1.0,
            shading: 0.65,
            quality: Vol3dQuality::Balanced,
            floor_mode: FloorMode::LowestTilt,
            floor_opacity: 0.82,
            floor_threshold_dbz: 0.0,
            floor_threshold_mode: Vol3dThresholdMode::Above,
            vertical_exaggeration: 2.0,
            fov_scale: 0.7,
            focus_height_km: 6.0,
            clip_bottom_km: 0.0,
            clip_top_km: BOX_TOP_M / 1000.0,
            show_grid: true,
            show_box: true,
            show_labels: true,
            resample_rx: None,
            volume_key: None,
            last_top_deg: 0.0,
            box_half_km: BOX_HALF_KM,
            box_target_lonlat: None,
            product_label: "REF".to_owned(),
            box_center_east_km: 0.0,
            box_center_north_km: 0.0,
            status: String::new(),
            lut_signature: 0,
            pending: Arc::new(Mutex::new(PendingUploads::default())),
            show_volume_controls: false,
            show_floor_controls: false,
        }
    }
}

impl Vol3d {
    pub fn top_km(&self) -> f32 {
        BOX_TOP_M / 1000.0
    }

    /// Display-space box height. One horizontal world unit equals
    /// `box_half_km`; therefore top_km / half_km is the physically correct
    /// height and the user multiplier is a stable exaggeration across sizes.
    pub fn zspan(&self) -> f32 {
        (self.top_km() / self.box_half_km.max(1.0) * self.vertical_exaggeration).clamp(0.06, 24.0)
    }

    pub fn focus_height_fraction(&self) -> f32 {
        (self.focus_height_km / self.top_km().max(0.1)).clamp(0.0, 1.0)
    }

    pub fn normalized_clip(&self) -> (f32, f32) {
        let top = self.top_km().max(0.1);
        let low = (self.clip_bottom_km / top).clamp(0.0, 0.98);
        let high = (self.clip_top_km / top).clamp(low + 0.01, 1.0);
        (low, high)
    }

    pub fn orbit_distance(&self) -> f32 {
        self.dist.max(self.zspan() * 0.45 + 1.25)
    }

    fn orbit_center(&self) -> [f32; 3] {
        [0.0, 0.0, self.zspan() * self.focus_height_fraction()]
    }

    pub fn orbit_eye(&self) -> [f32; 3] {
        let center = self.orbit_center();
        let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        let dist = self.orbit_distance();
        [
            center[0] + dist * cy * cp,
            center[1] + dist * sy * cp,
            center[2] + dist * sp,
        ]
    }

    fn fly_forward(&self) -> [f32; 3] {
        let (cy, sy) = (self.yaw.cos(), self.yaw.sin());
        let (cp, sp) = (self.pitch.cos(), self.pitch.sin());
        [-cy * cp, -sy * cp, -sp]
    }

    fn camera_basis(&self) -> Option<CameraBasis> {
        let (eye, mut fwd) = match self.camera_mode {
            Vol3dCameraMode::Orbit => {
                let center = self.orbit_center();
                let eye = self.orbit_eye();
                (
                    eye,
                    [center[0] - eye[0], center[1] - eye[1], center[2] - eye[2]],
                )
            }
            Vol3dCameraMode::Fly => ([self.fly_x, self.fly_y, self.fly_z], self.fly_forward()),
        };
        let fwd_len = (fwd[0] * fwd[0] + fwd[1] * fwd[1] + fwd[2] * fwd[2]).sqrt();
        if !fwd_len.is_finite() || fwd_len <= 1.0e-6 {
            return None;
        }
        for component in &mut fwd {
            *component /= fwd_len;
        }
        let mut right = [fwd[1], -fwd[0], 0.0];
        let right_len = (right[0] * right[0] + right[1] * right[1]).sqrt();
        if !right_len.is_finite() || right_len <= 1.0e-6 {
            return None;
        }
        right[0] /= right_len;
        right[1] /= right_len;
        let up = [
            right[1] * fwd[2] - right[2] * fwd[1],
            right[2] * fwd[0] - right[0] * fwd[2],
            right[0] * fwd[1] - right[1] * fwd[0],
        ];
        Some((eye, fwd, right, up))
    }

    pub fn enter_orbit_mode(&mut self) {
        self.camera_mode = Vol3dCameraMode::Orbit;
        self.pitch = self.pitch.clamp(0.03, 1.50);
    }

    pub fn enter_fly_mode(&mut self) {
        if self.camera_mode != Vol3dCameraMode::Fly {
            let eye = self.orbit_eye();
            self.fly_x = eye[0];
            self.fly_y = eye[1];
            self.fly_z = eye[2];
        }
        self.camera_mode = Vol3dCameraMode::Fly;
        self.pitch = self.pitch.clamp(-1.45, 1.45);
    }

    pub fn reset_fly_eye_from_orbit(&mut self) {
        let eye = self.orbit_eye();
        self.fly_x = eye[0];
        self.fly_y = eye[1];
        self.fly_z = eye[2];
    }

    pub fn fly_dolly(&mut self, amount: f32) {
        let fwd = self.fly_forward();
        self.fly_x += fwd[0] * amount;
        self.fly_y += fwd[1] * amount;
        self.fly_z = (self.fly_z + fwd[2] * amount).clamp(-1.0, self.zspan() + 1.0);
    }

    pub fn apply_fly_movement(&mut self, strafe: f32, forward: f32, vertical: f32, dt: f32) {
        if strafe == 0.0 && forward == 0.0 && vertical == 0.0 {
            return;
        }
        let fwd = self.fly_forward();
        let mut right = [fwd[1], -fwd[0], 0.0];
        let right_len = (right[0] * right[0] + right[1] * right[1]).sqrt();
        if right_len > 1.0e-6 {
            right[0] /= right_len;
            right[1] /= right_len;
        }
        let speed = self.fly_speed.max(0.05) * dt.max(0.0);
        self.fly_x += (right[0] * strafe + fwd[0] * forward) * speed;
        self.fly_y += (right[1] * strafe + fwd[1] * forward) * speed;
        self.fly_z =
            (self.fly_z + (fwd[2] * forward + vertical) * speed).clamp(-1.0, self.zspan() + 1.0);
    }

    pub fn reset_camera(&mut self) {
        self.yaw = 0.6;
        self.pitch = 0.45;
        self.dist = 2.4;
        self.fov_scale = 0.7;
        self.focus_height_km = 6.0;
        self.enter_orbit_mode();
        self.reset_fly_eye_from_orbit();
    }

    pub fn top_view(&mut self) {
        self.yaw = 0.0;
        self.pitch = 1.50;
        self.dist = 2.45;
        self.focus_height_km = 0.0;
        self.enter_orbit_mode();
    }

    pub fn low_view(&mut self) {
        self.yaw = 0.6;
        self.pitch = 0.20;
        self.dist = 2.7;
        self.focus_height_km = 4.0;
        self.enter_orbit_mode();
    }

    pub fn apply_balanced_preset(&mut self) {
        self.threshold_dbz = 25.0;
        self.opacity = 0.48;
        self.density = 1.0;
        self.shading = 0.65;
        self.quality = Vol3dQuality::Balanced;
    }

    pub fn apply_structure_preset(&mut self) {
        self.threshold_dbz = 12.0;
        self.opacity = 0.28;
        self.density = 0.78;
        self.shading = 0.90;
        self.quality = Vol3dQuality::High;
    }

    pub fn apply_core_preset(&mut self) {
        self.threshold_dbz = 40.0;
        self.opacity = 0.72;
        self.density = 1.35;
        self.shading = 0.48;
        self.quality = Vol3dQuality::Balanced;
    }

    /// Exact CPU companion to the WGSL camera projection. Returning `None`
    /// avoids drawing annotation lines through the camera or behind it.
    pub fn project_point(&self, rect: egui::Rect, point: [f32; 3]) -> Option<egui::Pos2> {
        let (eye, fwd, right, up) = self.camera_basis()?;
        let delta = [point[0] - eye[0], point[1] - eye[1], point[2] - eye[2]];
        let depth = delta[0] * fwd[0] + delta[1] * fwd[1] + delta[2] * fwd[2];
        if !depth.is_finite() || depth <= 1.0e-4 {
            return None;
        }
        let x = (delta[0] * right[0] + delta[1] * right[1] + delta[2] * right[2])
            / depth
            / self.fov_scale.max(0.05);
        let y = (delta[0] * up[0] + delta[1] * up[1] + delta[2] * up[2])
            / depth
            / self.fov_scale.max(0.05);
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        // The shader's horizontal aspect correction makes both axes use the
        // viewport HEIGHT in pixel space. The old width-based y scale was the
        // source of the apparent floating ground plane on wide panes.
        let scale = rect.height() * 0.5;
        Some(rect.center() + egui::vec2(x, -y) * scale)
    }
}

pub fn normalize_box_with_range(
    values: &[f32],
    n: usize,
    nz: usize,
    value_min: f32,
    value_max: f32,
) -> VolumeBox {
    let data = values
        .iter()
        .map(|value| normalize_value(*value, value_min, value_max))
        .collect::<Vec<_>>();
    VolumeBox {
        data,
        n,
        nz,
        // Upload an empty floor with every new volume so a failed low-tilt
        // raster can never leave the prior scan painted underneath.
        floor_data: Some(vec![0; FLOOR_N.saturating_mul(FLOOR_N)]),
        floor_elevation_deg: None,
    }
}

pub fn empty_box() -> VolumeBox {
    VolumeBox {
        data: vec![0; BOX_N.saturating_mul(BOX_N).saturating_mul(BOX_NZ)],
        n: BOX_N,
        nz: BOX_NZ,
        floor_data: Some(vec![0; FLOOR_N.saturating_mul(FLOOR_N)]),
        floor_elevation_deg: None,
    }
}

fn normalize_value(value: f32, value_min: f32, value_max: f32) -> u8 {
    if value.is_finite() {
        let span = (value_max - value_min).abs().max(f32::EPSILON);
        (((value - value_min) / span).clamp(0.0, 1.0) * 255.0).round() as u8
    } else {
        0
    }
}

#[derive(Clone, Copy)]
struct FloorAzimuthSample {
    azimuth_deg: f32,
    row: usize,
    left_half_width_deg: f32,
    right_half_width_deg: f32,
}

fn angle_distance_deg(a: f32, b: f32) -> f32 {
    let delta = (a - b).abs().rem_euclid(360.0);
    delta.min(360.0 - delta)
}

fn signed_angle_delta_deg(from: f32, to: f32) -> f32 {
    (to - from + 540.0).rem_euclid(360.0) - 180.0
}

fn floor_azimuth_samples(
    volume: &RadarVolume,
    cut_index: usize,
    grid: &MomentGrid,
) -> Vec<FloorAzimuthSample> {
    let Some(cut) = volume.cuts.get(cut_index) else {
        return Vec::new();
    };
    let mut base = grid
        .radial_indices
        .iter()
        .enumerate()
        .filter_map(|(row, radial_index)| {
            let azimuth_deg = cut
                .radials
                .get(*radial_index)?
                .azimuth_deg
                .rem_euclid(360.0);
            azimuth_deg.is_finite().then_some((azimuth_deg, row))
        })
        .collect::<Vec<_>>();
    base.sort_by(|left, right| left.0.total_cmp(&right.0));
    if base.is_empty() {
        return Vec::new();
    }

    (0..base.len())
        .map(|index| {
            let (azimuth_deg, row) = base[index];
            let previous = base[(index + base.len() - 1) % base.len()].0;
            let next = base[(index + 1) % base.len()].0;
            let left_gap = (azimuth_deg - previous).rem_euclid(360.0);
            let right_gap = (next - azimuth_deg).rem_euclid(360.0);
            FloorAzimuthSample {
                azimuth_deg,
                row,
                // A 3 degree ceiling keeps sector-scan edges from smearing
                // across their unobserved azimuth gap.
                left_half_width_deg: (0.5 * left_gap).clamp(0.05, 3.0),
                right_half_width_deg: (0.5 * right_gap).clamp(0.05, 3.0),
            }
        })
        .collect()
}

fn nearest_floor_row(samples: &[FloorAzimuthSample], azimuth_deg: f32) -> Option<usize> {
    if samples.is_empty() {
        return None;
    }
    let index = samples.partition_point(|sample| sample.azimuth_deg < azimuth_deg);
    let low = if index == 0 {
        samples.len() - 1
    } else {
        index - 1
    };
    let high = if index >= samples.len() { 0 } else { index };
    let candidate = if angle_distance_deg(samples[low].azimuth_deg, azimuth_deg)
        <= angle_distance_deg(samples[high].azimuth_deg, azimuth_deg)
    {
        samples[low]
    } else {
        samples[high]
    };
    let signed = signed_angle_delta_deg(candidate.azimuth_deg, azimuth_deg);
    let allowed = if signed < 0.0 {
        candidate.left_half_width_deg
    } else {
        candidate.right_half_width_deg
    };
    (signed.abs() <= allowed + 0.02).then_some(candidate.row)
}

pub fn lowest_moment_floor(
    volume: &RadarVolume,
    moment: &MomentType,
    center_east_km: f32,
    center_north_km: f32,
    half_km: f32,
    value_min: f32,
    value_max: f32,
) -> Option<(Vec<u8>, f32)> {
    if !half_km.is_finite() || half_km <= 0.0 {
        return None;
    }
    let candidates = volume
        .cuts
        .iter()
        .enumerate()
        .filter_map(|(index, cut)| {
            let grid = cut.moments.get(moment)?;
            (cut.elevation_deg.is_finite() && grid.radial_count() > 0).then_some((
                index,
                cut.elevation_deg,
                grid.radial_count(),
                grid,
            ))
        })
        .collect::<Vec<_>>();
    let max_rows = candidates.iter().map(|candidate| candidate.2).max()?;
    let minimum_usable_rows = (max_rows / 2).max(8).min(max_rows);
    let (cut_index, elevation_deg, _, grid) = candidates
        .into_iter()
        .filter(|candidate| candidate.2 >= minimum_usable_rows)
        .min_by(|left, right| {
            left.1
                .total_cmp(&right.1)
                .then_with(|| right.2.cmp(&left.2))
        })?;
    let azimuth_samples = floor_azimuth_samples(volume, cut_index, grid);
    if azimuth_samples.is_empty() || grid.gate_range.gate_count == 0 {
        return None;
    }
    let first_gate_m = grid.gate_range.first_gate_m as f32;
    let gate_spacing_m = grid.gate_range.gate_spacing_m.max(1) as f32;
    let gate_count = grid.gate_range.gate_count;
    let span_km = half_km * 2.0;
    let denominator = (FLOOR_N - 1).max(1) as f32;
    let mut data = vec![0u8; FLOOR_N * FLOOR_N];
    data.par_chunks_mut(FLOOR_N)
        .enumerate()
        .for_each(|(row_index, output_row)| {
            // row 0 = south, matching the volume texture's +Y convention.
            let north_km = center_north_km - half_km + span_km * row_index as f32 / denominator;
            for (column_index, output) in output_row.iter_mut().enumerate() {
                let east_km =
                    center_east_km - half_km + span_km * column_index as f32 / denominator;
                let range_m = east_km.hypot(north_km) * 1000.0;
                let gate_float = (range_m - first_gate_m) / gate_spacing_m;
                if !gate_float.is_finite() {
                    continue;
                }
                let gate = gate_float.round() as isize;
                if gate < 0 || gate as usize >= gate_count {
                    continue;
                }
                let azimuth_deg = east_km.atan2(north_km).to_degrees().rem_euclid(360.0);
                let Some(row) = nearest_floor_row(&azimuth_samples, azimuth_deg) else {
                    continue;
                };
                let Some(value) = grid
                    .scaled_value(row, gate as usize)
                    .filter(|value| value.is_finite())
                else {
                    continue;
                };
                *output = normalize_value(value, value_min, value_max);
            }
        });
    Some((data, elevation_deg))
}

const SHADER: &str = r#"
struct Uniforms {
    yaw: f32,
    pitch: f32,
    dist: f32,
    threshold: f32,

    opacity: f32,
    aspect: f32,
    floor_opacity: f32,
    floor_mode: f32,

    zspan: f32,
    fov_scale: f32,
    steps: f32,
    density: f32,

    shading: f32,
    clip_low: f32,
    clip_high: f32,
    floor_threshold: f32,

    focus_height: f32,
    camera_mode: f32,
    fly_x: f32,
    fly_y: f32,

    fly_z: f32,
    threshold_high: f32,
    threshold_mode: f32,
    floor_threshold_high: f32,

    floor_threshold_mode: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var t_volume: texture_3d<f32>;
@group(0) @binding(2) var s_volume: sampler;
@group(0) @binding(3) var t_lut: texture_2d<f32>;
@group(0) @binding(4) var s_lut: sampler;
@group(0) @binding(5) var t_floor: texture_2d<f32>;
@group(0) @binding(6) var s_floor: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let p = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.uv = p;
    out.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    return out;
}

const MAX_STEPS: i32 = 256;
const FLOOR_COLUMN_STEPS: i32 = 48;
const VOLUME_DX: f32 = 1.0 / 192.0;
const VOLUME_DZ: f32 = 1.0 / 48.0;

fn box_intersect(
    ro: vec3<f32>,
    rd: vec3<f32>,
    bmin: vec3<f32>,
    bmax: vec3<f32>
) -> vec2<f32> {
    let inv = 1.0 / rd;
    let t0 = (bmin - ro) * inv;
    let t1 = (bmax - ro) * inv;
    let tmin = min(t0, t1);
    let tmax = max(t0, t1);
    return vec2<f32>(
        max(max(tmin.x, tmin.y), tmin.z),
        min(min(tmax.x, tmax.y), tmax.z)
    );
}

fn shaded_rgb(uvw: vec3<f32>, ray_dir: vec3<f32>, base: vec3<f32>) -> vec3<f32> {
    if (u.shading <= 0.001) {
        return base;
    }
    let gx = textureSampleLevel(
        t_volume,
        s_volume,
        uvw + vec3<f32>(VOLUME_DX, 0.0, 0.0),
        0.0
    ).r - textureSampleLevel(
        t_volume,
        s_volume,
        uvw - vec3<f32>(VOLUME_DX, 0.0, 0.0),
        0.0
    ).r;
    let gy = textureSampleLevel(
        t_volume,
        s_volume,
        uvw + vec3<f32>(0.0, VOLUME_DX, 0.0),
        0.0
    ).r - textureSampleLevel(
        t_volume,
        s_volume,
        uvw - vec3<f32>(0.0, VOLUME_DX, 0.0),
        0.0
    ).r;
    let gz = textureSampleLevel(
        t_volume,
        s_volume,
        uvw + vec3<f32>(0.0, 0.0, VOLUME_DZ),
        0.0
    ).r - textureSampleLevel(
        t_volume,
        s_volume,
        uvw - vec3<f32>(0.0, 0.0, VOLUME_DZ),
        0.0
    ).r;
    let gradient = vec3<f32>(gx, gy, gz * 0.55);
    if (dot(gradient, gradient) < 0.000001) {
        return base;
    }
    let normal = normalize(gradient);
    let light_dir = normalize(vec3<f32>(0.40, -0.48, 0.78));
    let diffuse = 0.45 + 0.55 * abs(dot(normal, light_dir));
    let rim = 0.18 * pow(1.0 - abs(dot(normal, -ray_dir)), 2.0);
    let lighting = clamp(diffuse + rim, 0.35, 1.25);
    return base * mix(1.0, lighting, clamp(u.shading, 0.0, 1.0));
}

fn column_max(uv: vec2<f32>) -> f32 {
    var maximum = 0.0;
    let low = clamp(u.clip_low, 0.0, 0.99);
    let high = clamp(u.clip_high, low + 0.01, 1.0);
    for (var i = 0; i < FLOOR_COLUMN_STEPS; i = i + 1) {
        let fraction = (f32(i) + 0.5) / f32(FLOOR_COLUMN_STEPS);
        let z = mix(low, high, fraction);
        maximum = max(
            maximum,
            textureSampleLevel(t_volume, s_volume, vec3<f32>(uv, z), 0.0).r
        );
    }
    return maximum;
}

fn threshold_strength(value: f32, low: f32, high: f32, mode: f32, width: f32) -> f32 {
    if (mode > 1.5) {
        if (value <= low) {
            return smoothstep(0.0, width, low - value);
        }
        if (value >= high) {
            return smoothstep(0.0, width, value - high);
        }
        return -1.0;
    }
    if (mode > 0.5) {
        if (value >= low) {
            return -1.0;
        }
        return smoothstep(0.0, width, low - value);
    }
    if (value <= low) {
        return -1.0;
    }
    return smoothstep(low, low + width, value);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let cy = cos(u.yaw);
    let sy = sin(u.yaw);
    let cp = cos(u.pitch);
    let sp = sin(u.pitch);
    let center = vec3<f32>(0.0, 0.0, u.zspan * clamp(u.focus_height, 0.0, 1.0));
    var eye = center + u.dist * vec3<f32>(cy * cp, sy * cp, sp);
    var fwd = normalize(center - eye);
    if (u.camera_mode > 0.5) {
        eye = vec3<f32>(u.fly_x, u.fly_y, u.fly_z);
        fwd = normalize(vec3<f32>(-cy * cp, -sy * cp, -sp));
    }
    let right = normalize(cross(fwd, vec3<f32>(0.0, 0.0, 1.0)));
    let up = cross(right, fwd);
    let ndc = (in.uv * 2.0 - 1.0) * vec2<f32>(u.aspect, 1.0);
    let rd = normalize(fwd + u.fov_scale * (ndc.x * right + ndc.y * up));

    let clip_low_z = clamp(u.clip_low, 0.0, 0.99) * u.zspan;
    let clip_high_z = clamp(u.clip_high, u.clip_low + 0.01, 1.0) * u.zspan;
    let hit = box_intersect(
        eye,
        rd,
        vec3<f32>(-1.0, -1.0, clip_low_z),
        vec3<f32>(1.0, 1.0, clip_high_z)
    );

    var color = vec3<f32>(0.0);
    var accumulated = 0.0;
    if (hit.y > max(hit.x, 0.0)) {
        let t0 = max(hit.x, 0.0);
        let step_count = i32(clamp(u.steps, 32.0, 256.0));
        let dt = (hit.y - t0) / f32(step_count);
        for (var i = 0; i < MAX_STEPS; i = i + 1) {
            if (i >= step_count) {
                break;
            }
            let point = eye + rd * (t0 + (f32(i) + 0.5) * dt);
            let uvw = vec3<f32>(
                (point.x + 1.0) * 0.5,
                (point.y + 1.0) * 0.5,
                point.z / u.zspan
            );
            let value = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
            let transfer = threshold_strength(
                value,
                u.threshold,
                u.threshold_high,
                u.threshold_mode,
                0.08
            );
            if (transfer <= 0.0) {
                continue;
            }
            let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(value, 0.5), 0.0);
            var alpha = palette.a * u.opacity * transfer;
            alpha = 1.0 - pow(
                max(1.0 - alpha, 0.0001),
                dt * 28.0 * max(u.density, 0.05)
            );
            let sample_rgb = shaded_rgb(uvw, rd, palette.rgb);
            color = color + (1.0 - accumulated) * alpha * sample_rgb;
            accumulated = accumulated + (1.0 - accumulated) * alpha;
            if (accumulated > 0.985) {
                break;
            }
        }
    }

    // Ground underlay at the exact z=0 box floor. With the camera constrained
    // above the box this intersection lies behind all volume samples, so
    // ordinary front-to-back compositing gives correct occlusion.
    if (u.floor_mode > 0.5 && abs(rd.z) > 0.00001) {
        let floor_t = -eye.z / rd.z;
        if (floor_t > 0.0) {
            let point = eye + rd * floor_t;
            if (abs(point.x) <= 1.0 && abs(point.y) <= 1.0) {
                let floor_uv = vec2<f32>(
                    (point.x + 1.0) * 0.5,
                    (point.y + 1.0) * 0.5
                );
                var value = textureSampleLevel(t_floor, s_floor, floor_uv, 0.0).r;
                if (u.floor_mode > 1.5) {
                    value = column_max(floor_uv);
                }
                let floor_transfer = threshold_strength(
                    value,
                    u.floor_threshold,
                    u.floor_threshold_high,
                    u.floor_threshold_mode,
                    0.04
                );
                if (floor_transfer > 0.0) {
                    let palette = textureSampleLevel(
                        t_lut,
                        s_lut,
                        vec2<f32>(value, 0.5),
                        0.0
                    );
                    let alpha = palette.a * u.floor_opacity * floor_transfer;
                    color = color + (1.0 - accumulated) * alpha * palette.rgb;
                    accumulated = accumulated + (1.0 - accumulated) * alpha;
                }
            }
        }
    }

    if (accumulated <= 0.00001) {
        return vec4<f32>(0.0);
    }
    // The render target uses straight-alpha blending. The ray marcher builds
    // a premultiplied front-to-back color, so unpremultiply once here rather
    // than letting the fixed-function blend multiply alpha a second time.
    return vec4<f32>(color / accumulated, accumulated);
}
"#;

/// GPU resources, created once at startup and stored in egui-wgpu's callback
/// resource typemap.
pub struct Vol3dResources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    volume_tex: wgpu::Texture,
    lut_tex: wgpu::Texture,
    floor_tex: wgpu::Texture,
}

/// One-time GPU setup (eframe custom-3D pattern: call from the app
/// constructor with `cc.wgpu_render_state`).
pub fn init_gpu(render_state: &egui_wgpu::RenderState) {
    let device = &render_state.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vol3d"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vol3d-uniforms"),
        size: UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let volume_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-volume"),
        size: wgpu::Extent3d {
            width: BOX_N as u32,
            height: BOX_N as u32,
            depth_or_array_layers: BOX_NZ as u32,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let lut_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-lut"),
        size: wgpu::Extent3d {
            width: 256,
            height: 1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let floor_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-floor"),
        size: wgpu::Extent3d {
            width: FLOOR_N as u32,
            height: FLOOR_N as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("vol3d-sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vol3d-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 6,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vol3d-bg"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(
                    &volume_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::TextureView(
                    &lut_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            wgpu::BindGroupEntry {
                binding: 5,
                resource: wgpu::BindingResource::TextureView(
                    &floor_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 6,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vol3d-pl"),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("vol3d-pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
                format: render_state.target_format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(Vol3dResources {
            pipeline,
            bind_group,
            uniforms,
            volume_tex,
            lut_tex,
            floor_tex,
        });
}

/// Per-frame paint callback: uniforms + pending texture uploads in `prepare`,
/// one fullscreen-triangle draw in `paint`.
pub struct Vol3dCallback {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub camera_mode: Vol3dCameraMode,
    pub fly_x: f32,
    pub fly_y: f32,
    pub fly_z: f32,
    pub threshold01: f32,
    pub threshold_high01: f32,
    pub threshold_mode: Vol3dThresholdMode,
    pub opacity: f32,
    pub aspect: f32,
    pub floor_opacity: f32,
    pub floor_mode: FloorMode,
    pub zspan: f32,
    pub fov_scale: f32,
    pub quality: Vol3dQuality,
    pub density: f32,
    pub shading: f32,
    pub clip_low: f32,
    pub clip_high: f32,
    pub floor_threshold01: f32,
    pub floor_threshold_high01: f32,
    pub floor_threshold_mode: Vol3dThresholdMode,
    pub focus_height: f32,
    pub pending: Arc<Mutex<PendingUploads>>,
}

impl egui_wgpu::CallbackTrait for Vol3dCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(resources) = resources.get::<Vol3dResources>() else {
            return Vec::new();
        };
        let uniforms: [f32; UNIFORM_FLOATS] = [
            self.yaw,
            self.pitch,
            self.dist,
            self.threshold01,
            self.opacity,
            self.aspect,
            self.floor_opacity,
            self.floor_mode.shader_value(),
            self.zspan,
            self.fov_scale,
            self.quality.steps().min(MAX_SHADER_STEPS) as f32,
            self.density,
            self.shading,
            self.clip_low,
            self.clip_high,
            self.floor_threshold01,
            self.focus_height,
            self.camera_mode.shader_value(),
            self.fly_x,
            self.fly_y,
            self.fly_z,
            self.threshold_high01,
            self.threshold_mode.shader_value(),
            self.floor_threshold_high01,
            self.floor_threshold_mode.shader_value(),
            0.0,
            0.0,
            0.0,
        ];
        let mut bytes = [0u8; UNIFORM_FLOATS * std::mem::size_of::<f32>()];
        for (index, value) in uniforms.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        queue.write_buffer(&resources.uniforms, 0, &bytes);

        if let Ok(mut pending) = self.pending.lock() {
            if let Some(volume) = pending.volume.take() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.volume_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &volume.data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(volume.n as u32),
                        rows_per_image: Some(volume.n as u32),
                    },
                    wgpu::Extent3d {
                        width: volume.n as u32,
                        height: volume.n as u32,
                        depth_or_array_layers: volume.nz as u32,
                    },
                );
                if let Some(floor_data) = volume.floor_data {
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &resources.floor_tex,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &floor_data,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(FLOOR_N as u32),
                            rows_per_image: Some(FLOOR_N as u32),
                        },
                        wgpu::Extent3d {
                            width: FLOOR_N as u32,
                            height: FLOOR_N as u32,
                            depth_or_array_layers: 1,
                        },
                    );
                }
            }
            if let Some(lut) = pending.lut.take() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.lut_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &lut,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(256 * 4),
                        rows_per_image: Some(1),
                    },
                    wgpu::Extent3d {
                        width: 256,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(resources) = resources.get::<Vol3dResources>() else {
            return;
        };
        render_pass.set_pipeline(&resources.pipeline);
        render_pass.set_bind_group(0, &resources.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_projection_is_aspect_stable() {
        let explorer = Vol3d::default();
        let square = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(640.0, 640.0));
        let wide = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1_200.0, 640.0));
        let point = [0.35, -0.40, 0.0];
        let square_position = explorer
            .project_point(square, point)
            .expect("visible point");
        let wide_position = explorer.project_point(wide, point).expect("visible point");
        let square_offset = square_position - square.center();
        let wide_offset = wide_position - wide.center();
        assert!((square_offset.x - wide_offset.x).abs() < 1.0e-3);
        assert!((square_offset.y - wide_offset.y).abs() < 1.0e-3);
    }

    #[test]
    fn vertical_exaggeration_is_independent_of_box_size() {
        let mut explorer = Vol3d {
            vertical_exaggeration: 2.5,
            ..Default::default()
        };
        for half_km in [10.0, 20.0, 60.0, 120.0, 180.0] {
            explorer.box_half_km = half_km;
            let recovered = explorer.zspan() * half_km / explorer.top_km();
            assert!((recovered - 2.5).abs() < 1.0e-4);
        }
    }

    #[test]
    fn orbit_distance_backs_out_for_tall_selected_cell_boxes() {
        let explorer = Vol3d {
            box_half_km: 6.0,
            vertical_exaggeration: 2.5,
            dist: 1.2,
            ..Default::default()
        };

        assert!(explorer.zspan() > 1.6);
        assert!(explorer.orbit_distance() > explorer.dist);
    }

    #[test]
    fn fly_camera_starts_from_orbit_eye_and_sees_box_center() {
        let mut explorer = Vol3d::default();
        let orbit_eye = explorer.orbit_eye();
        explorer.enter_fly_mode();

        assert_eq!(explorer.camera_mode, Vol3dCameraMode::Fly);
        assert!((explorer.fly_x - orbit_eye[0]).abs() < 1.0e-5);
        assert!((explorer.fly_y - orbit_eye[1]).abs() < 1.0e-5);
        assert!((explorer.fly_z - orbit_eye[2]).abs() < 1.0e-5);

        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        assert!(
            explorer
                .project_point(rect, explorer.orbit_center())
                .is_some()
        );
    }
}
