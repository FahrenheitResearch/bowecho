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

mod lighting;

pub use lighting::{Vol3dLightingPreset, Vol3dLightingSettings};

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use lighting::{
    LIGHT_VOLUME_N, LIGHT_VOLUME_NZ, LIGHT_WORKGROUP_SIZE, LIGHTING_ENCODE_MAX, MAX_SHADOW_STEPS,
    combine_cache_revision,
};
use radar_core::{MomentGrid, MomentType, RadarVolume};
use rayon::prelude::*;
use std::sync::{Arc, Mutex, mpsc};

#[path = "vol3d/advanced.rs"]
mod advanced;
pub use advanced::{SupportMode, Vol3dRenderMode};

pub const BOX_N: usize = 192;
pub const BOX_NZ: usize = 48;
/// A sharper floor than the volume lattice. RG value/mask texels retain a
/// 1024-byte row pitch that is friendly to texture uploads.
pub const FLOOR_N: usize = 512;
pub const BOX_HALF_KM: f32 = 60.0;
pub const BOX_TOP_M: f32 = 18_000.0;
const MAX_SHADER_STEPS: usize = 256;
const UNIFORM_FLOATS: usize = 48;
const UNIFORM_BYTES: u64 = (UNIFORM_FLOATS * std::mem::size_of::<f32>()) as u64;
const LIGHTING_UNIFORM_FLOATS: usize = 32;
const LIGHTING_UNIFORM_BYTES: u64 = (LIGHTING_UNIFORM_FLOATS * std::mem::size_of::<f32>()) as u64;
const LIGHT_VOLUME_SHADER: &str = include_str!("vol3d/light_volume.wgsl");
pub const LIGHTING_MAX_SHADOW_STEPS: u32 = MAX_SHADOW_STEPS;

pub type Vol3dVolumeKey = (String, String, i64, usize, i32, i32, i32, i32);

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
    /// Velocity mode only: reflectivity structure gate (dBZ). The volume body
    /// takes its shape from where reflectivity exceeds this, and the signed
    /// velocity is painted only inside that body.
    pub vel_ref_gate_dbz: f32,
    /// Velocity mode only: 0 shows all flow at the reflectivity opacity (broad
    /// flow), 1 fades near-zero flow and lets strong inbound/outbound couplets
    /// dominate.
    pub vel_couplet_emphasis: f32,
    /// True when the currently uploaded box is a velocity two-box pair
    /// (reflectivity structure + velocity color). Drives the shader mode flag.
    pub velocity_color_active: bool,
    pub opacity: f32,
    /// Optical-depth multiplier. Opacity controls each sample; density controls
    /// how quickly repeated samples accumulate into a solid echo body.
    pub density: f32,
    /// Blend from untouched palette color (0) to cached SH lighting (1).
    pub shading: f32,
    /// Positive-energy environment, key-light, and pseudo-shadow controls.
    pub lighting: Vol3dLightingSettings,
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
    pub volume_key: Option<Vol3dVolumeKey>,
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
    /// Second-generation renderer mode. All modes use the same source data and
    /// palette; only integration/traversal changes.
    pub render_mode: Vol3dRenderMode,
    /// How the renderer exposes the beam-stack support field.
    pub support_mode: SupportMode,
    /// Support value below which the honest-fade path becomes transparent.
    pub support_floor: f32,
    pub support_fade: f32,
    /// Physical field value used by isosurface and hybrid-shell modes.
    pub iso_value: f32,
    pub iso_width: f32,
    /// Stable sub-voxel ray offset. It reduces banding without temporal shimmer.
    pub jitter_strength: f32,
    /// Segment-preintegrated transfer lookup for single-field DVR/hybrid.
    pub preintegration: bool,
    /// Horizontal crop box in normalized volume coordinates.
    pub crop_x_min: f32,
    pub crop_x_max: f32,
    pub crop_y_min: f32,
    pub crop_y_max: f32,
    /// Orthogonal slice positions in normalized volume coordinates.
    pub slice_x: f32,
    pub slice_y: f32,
    pub slice_z: f32,
    /// 0 = fixed reference sampling inside occupied bricks; 1 = full adaptive.
    pub adaptive_strength: f32,
    pub show_analysis_controls: bool,
    pub show_volume_controls: bool,
    pub show_floor_controls: bool,
    pub show_lighting_controls: bool,
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
    /// Optional co-located color box, same `n`/`nz` lattice as `data`. Present
    /// only in velocity mode: `data` then carries the REFLECTIVITY structure
    /// (opacity/threshold/shading) while `color_data` carries the signed
    /// velocity that drives the LUT color. `None` = single-box (reflectivity
    /// and every other moment): color comes from `data` itself, exactly as
    /// before.
    pub color_data: Option<Vec<u8>>,
    /// Beam-stack interpolation support; zero is no data.
    pub support_data: Vec<u8>,
    /// Conservative 8x8x8 brick min/max/support hierarchy.
    pub hierarchy_fine: Vec<u8>,
    /// Conservative macro hierarchy aggregated from fine bricks.
    pub hierarchy_coarse: Vec<u8>,
    pub acceleration_empty_fraction: f32,
    /// Packed RG texels for the low-level floor: normalized value followed by
    /// an observed-data mask. Rows run south-to-north, exactly like the y
    /// dimension of `volume_box_resample`.
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
            vel_ref_gate_dbz: 15.0,
            vel_couplet_emphasis: 0.35,
            velocity_color_active: false,
            opacity: 0.55,
            density: 1.0,
            shading: 0.65,
            lighting: Vol3dLightingSettings::default(),
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
            render_mode: Vol3dRenderMode::DirectVolume,
            support_mode: SupportMode::HonestFade,
            support_floor: 0.14,
            support_fade: 1.35,
            iso_value: 40.0,
            iso_width: 2.0,
            jitter_strength: 0.72,
            preintegration: true,
            crop_x_min: 0.0,
            crop_x_max: 1.0,
            crop_y_min: 0.0,
            crop_y_max: 1.0,
            slice_x: 0.5,
            slice_y: 0.5,
            slice_z: 0.35,
            adaptive_strength: 0.85,
            show_analysis_controls: false,
            show_volume_controls: false,
            show_floor_controls: false,
            show_lighting_controls: false,
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

    /// Velocity two-box preset: fill the whole precipitation body with its
    /// signed velocity color (no couplet emphasis, low reflectivity gate).
    /// Velocity structure is fine-grained, so all velocity presets ray-march at
    /// High quality to keep the volume smooth.
    pub fn apply_velocity_broad_preset(&mut self) {
        self.vel_ref_gate_dbz = 12.0;
        self.vel_couplet_emphasis = 0.0;
        self.opacity = 0.50;
        self.density = 1.00;
        self.shading = 0.55;
        self.quality = Vol3dQuality::High;
    }

    /// Velocity two-box preset: emphasize the couplet — fade weak flow, keep
    /// the color inside the storm cores.
    pub fn apply_velocity_couplet_preset(&mut self) {
        self.vel_ref_gate_dbz = 20.0;
        self.vel_couplet_emphasis = 0.60;
        self.opacity = 0.70;
        self.density = 1.10;
        self.shading = 0.60;
        self.quality = Vol3dQuality::High;
    }

    /// Velocity two-box preset: strongest reflectivity cores + strongest flow
    /// only, for picking out mesocyclone/shear signatures.
    pub fn apply_velocity_shear_preset(&mut self) {
        self.vel_ref_gate_dbz = 25.0;
        self.vel_couplet_emphasis = 0.85;
        self.opacity = 0.85;
        self.density = 1.30;
        self.shading = 0.55;
        self.quality = Vol3dQuality::High;
    }

    pub fn apply_sota_volume_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::DirectVolume;
        self.support_mode = SupportMode::HonestFade;
        self.preintegration = true;
        self.adaptive_strength = 0.85;
        self.jitter_strength = 0.72;
    }

    pub fn apply_sota_hybrid_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::HybridShell;
        self.support_mode = SupportMode::HonestFade;
        self.iso_value = 40.0;
        self.iso_width = 2.0;
        self.preintegration = true;
        self.adaptive_strength = 0.9;
    }

    pub fn apply_sota_surface_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::Isosurface;
        self.support_mode = SupportMode::HonestFade;
        self.iso_value = 45.0;
        self.preintegration = false;
        self.adaptive_strength = 1.0;
    }

    pub fn apply_sota_support_preset(&mut self) {
        self.render_mode = Vol3dRenderMode::SupportInspection;
        self.support_mode = SupportMode::Inspect;
        self.preintegration = false;
        self.adaptive_strength = 1.0;
    }

    pub fn normalized_horizontal_crop(&self) -> (f32, f32, f32, f32) {
        let x0 = self.crop_x_min.clamp(0.0, 0.99);
        let x1 = self.crop_x_max.clamp(x0 + 0.01, 1.0);
        let y0 = self.crop_y_min.clamp(0.0, 0.99);
        let y1 = self.crop_y_max.clamp(y0 + 0.01, 1.0);
        (x0, x1, y0, y1)
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

/// Normalize a slab of physical values into 0..255 texels against an explicit
/// range. Non-finite (no-data) maps to 0. Shared by the structure box and the
/// velocity color box so both lattices are normalized the same way.
pub fn normalize_values(values: &[f32], value_min: f32, value_max: f32) -> Vec<u8> {
    values
        .iter()
        .map(|value| normalize_value(*value, value_min, value_max))
        .collect()
}

/// Build a box from values alone, treating every finite value as fully
/// supported. Test-only since stage 2: every production path now carries the
/// beam-stack support field and goes through `normalize_box_with_support`, and
/// `empty_box` hand-writes its result instead of computing it. It survives as
/// the reference those tests pin the shortcut against, so `#[cfg(test)]` rather
/// than `#[allow(dead_code)]` - the compiler should keep telling us if a
/// production caller ever reappears without support metadata.
#[cfg(test)]
pub fn normalize_box_with_range(
    values: &[f32],
    n: usize,
    nz: usize,
    value_min: f32,
    value_max: f32,
) -> VolumeBox {
    let support = values
        .iter()
        .map(|value| if value.is_finite() { 255 } else { 0 })
        .collect::<Vec<_>>();
    normalize_box_with_support(values, &support, n, nz, value_min, value_max)
}

pub fn normalize_box_with_support(
    values: &[f32],
    support: &[u8],
    n: usize,
    nz: usize,
    value_min: f32,
    value_max: f32,
) -> VolumeBox {
    let data = normalize_values(values, value_min, value_max);
    let acceleration = advanced::build_acceleration(&data, support, n, nz);
    VolumeBox {
        data,
        n,
        nz,
        color_data: None,
        support_data: acceleration.support,
        hierarchy_fine: acceleration.fine_minmax,
        hierarchy_coarse: acceleration.coarse_minmax,
        acceleration_empty_fraction: acceleration.empty_fine_fraction,
        // Upload an empty floor with every new volume so a failed low-tilt
        // raster can never leave the prior scan painted underneath.
        // RG texels: normalized value plus an explicit observed-data mask.
        // Keeping validity separate is essential for Below/Outside threshold
        // modes because a missing gate and a valid value at the palette
        // minimum both normalize to zero.
        floor_data: Some(vec![0; FLOOR_N.saturating_mul(FLOOR_N).saturating_mul(2)]),
        floor_elevation_deg: None,
    }
}

/// All-no-data box used to blank the render between products and while a live
/// volume is still building.
///
/// It is called from the UI thread on every repaint of those waiting states, so
/// it must not take the general path: `normalize_box_with_range` allocates a
/// 1.7 M-voxel NaN slab and then walks the 3.4 M-texel dilated hierarchy gather
/// only to produce zeros. Every field below is exactly what that path yields
/// for an all-NaN input (no-data normalizes to 0, support is 0 everywhere, so
/// every brick is unsupported and publishes an all-zero range), and
/// `empty_box_matches_general_path` pins that equality.
pub fn empty_box() -> VolumeBox {
    let voxels = BOX_N.saturating_mul(BOX_N).saturating_mul(BOX_NZ);
    VolumeBox {
        data: vec![0; voxels],
        n: BOX_N,
        nz: BOX_NZ,
        color_data: None,
        support_data: vec![0; voxels],
        hierarchy_fine: vec![0; advanced::FINE_X * advanced::FINE_Y * advanced::FINE_Z * 4],
        hierarchy_coarse: vec![0; advanced::COARSE_X * advanced::COARSE_Y * advanced::COARSE_Z * 4],
        acceleration_empty_fraction: 1.0,
        // Upload an empty floor with every new volume so a failed low-tilt
        // raster can never leave the prior scan painted underneath.
        floor_data: Some(vec![0; FLOOR_N.saturating_mul(FLOOR_N).saturating_mul(2)]),
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
    // Pack each floor texel as RG: normalized value, then an explicit
    // validity mask. A value-only texture cannot distinguish no-data from a
    // legitimate value at `value_min`; that ambiguity painted every missing
    // pixel in Below/Outside threshold modes.
    let mut data = vec![0u8; FLOOR_N * FLOOR_N * 2];
    data.par_chunks_mut(FLOOR_N * 2)
        .enumerate()
        .for_each(|(row_index, output_row)| {
            // row 0 = south, matching the volume texture's +Y convention.
            let north_km = center_north_km - half_km + span_km * row_index as f32 / denominator;
            for (column_index, output) in output_row.chunks_exact_mut(2).enumerate() {
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
                output[0] = normalize_value(value, value_min, value_max);
                output[1] = 255;
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
    // Velocity two-box mode: t_volume carries the REFLECTIVITY structure and
    // t_color carries the signed velocity. velocity_mode > 0.5 switches on the
    // reflectivity-driven opacity + velocity-driven color path.
    velocity_mode: f32,
    // Normalized reflectivity structure gate (0..1). Below this, transparent.
    ref_gate: f32,
    // 0 = show all flow at the reflectivity opacity, 1 = fade weak flow so
    // strong inbound/outbound couplets dominate.
    couplet_emphasis: f32,

    // Fragment-only controls for the precomputed lighting texture.
    rim_strength: f32,
    light_enabled: f32,
    light_decode_scale: f32,
    _lighting_pad: f32,

    render_mode: f32,
    support_mode: f32,
    support_floor: f32,
    support_fade: f32,

    iso_value: f32,
    iso_width: f32,
    jitter_strength: f32,
    preintegration: f32,

    crop_x_min: f32,
    crop_x_max: f32,
    crop_y_min: f32,
    crop_y_max: f32,

    slice_x: f32,
    slice_y: f32,
    slice_z: f32,
    adaptive_strength: f32,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var t_volume: texture_3d<f32>;
@group(0) @binding(2) var s_volume: sampler;
@group(0) @binding(3) var t_lut: texture_2d<f32>;
@group(0) @binding(4) var s_lut: sampler;
@group(0) @binding(5) var t_floor: texture_2d<f32>;
@group(0) @binding(6) var s_floor: sampler;
@group(0) @binding(7) var t_color: texture_3d<f32>;
@group(0) @binding(8) var t_light: texture_3d<f32>;


@group(0) @binding(9) var t_support: texture_3d<f32>;
@group(0) @binding(10) var t_hierarchy_fine: texture_3d<f32>;
@group(0) @binding(11) var t_hierarchy_coarse: texture_3d<f32>;
@group(0) @binding(12) var t_preintegrated: texture_2d<f32>;

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
    if (u.shading <= 0.001 || u.light_enabled < 0.5) {
        return base;
    }
    let cached = textureSampleLevel(t_light, s_volume, uvw, 0.0);
    let decoded_normal = cached.rgb * 2.0 - vec3<f32>(1.0);
    var rim = 0.0;
    if (dot(decoded_normal, decoded_normal) > 0.01) {
        let normal = normalize(decoded_normal);
        let rim_facing = 1.0 - abs(dot(normal, -ray_dir));
        rim = max(u.rim_strength, 0.0) * rim_facing * rim_facing;
    }
    let lighting = clamp(cached.a * u.light_decode_scale + rim, 0.20, 2.0);
    // Scalar illumination preserves the exact reflectivity/velocity palette hue.
    return base * mix(1.0, lighting, clamp(u.shading, 0.0, 1.0));
}

fn column_max(uv: vec2<f32>) -> vec2<f32> {
    var maximum = 0.0;
    var observed = 0.0;
    let low = clamp(u.clip_low, 0.0, 0.99);
    let high = clamp(u.clip_high, low + 0.01, 1.0);
    for (var i = 0; i < FLOOR_COLUMN_STEPS; i = i + 1) {
        let fraction = (f32(i) + 0.5) / f32(FLOOR_COLUMN_STEPS);
        let z = mix(low, high, fraction);
        let point = vec3<f32>(uv, z);
        let support = textureSampleLevel(t_support, s_volume, point, 0.0).r;
        if (support > 0.0001) {
            maximum = max(maximum, textureSampleLevel(t_volume, s_volume, point, 0.0).r);
            observed = 1.0;
        }
    }
    return vec2<f32>(maximum, observed);
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


const MAX_TRAVERSAL_STEPS: i32 = 1024;
const FINE_DIMS: vec3<i32> = vec3<i32>(24, 24, 6);
const COARSE_DIMS: vec3<i32> = vec3<i32>(6, 6, 2);
const HUGE_T: f32 = 1.0e20;

fn hash12(p: vec2<f32>) -> f32 {
    let h = dot(p, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453123);
}

fn point_to_uvw(point: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        (point.x + 1.0) * 0.5,
        (point.y + 1.0) * 0.5,
        point.z / max(u.zspan, 0.0001)
    );
}

fn hierarchy_coord(uvw: vec3<f32>, dims: vec3<i32>) -> vec3<i32> {
    let scaled = vec3<i32>(floor(clamp(uvw, vec3<f32>(0.0), vec3<f32>(0.999999)) * vec3<f32>(dims)));
    return clamp(scaled, vec3<i32>(0), dims - vec3<i32>(1));
}

fn fine_range(uvw: vec3<f32>) -> vec4<f32> {
    return textureLoad(t_hierarchy_fine, hierarchy_coord(uvw, FINE_DIMS), 0);
}

fn coarse_range(uvw: vec3<f32>) -> vec4<f32> {
    return textureLoad(t_hierarchy_coarse, hierarchy_coord(uvw, COARSE_DIMS), 0);
}

fn range_can_contribute(range: vec4<f32>) -> bool {
    if (range.a <= 0.0001) {
        return false;
    }
    if (u.render_mode > 1.5 && u.render_mode < 2.5) {
        return range.g >= u.iso_value - max(u.iso_width, 0.002);
    }
    if (u.render_mode > 0.5 && u.render_mode < 1.5) {
        if (range.g >= u.iso_value - max(u.iso_width, 0.002)) {
            return true;
        }
    }
    if (u.velocity_mode > 0.5) {
        return range.g > u.ref_gate;
    }
    if (u.threshold_mode > 1.5) {
        return range.r < u.threshold || range.g > u.threshold_high;
    }
    if (u.threshold_mode > 0.5) {
        return range.r < u.threshold;
    }
    return range.g > u.threshold;
}

fn axis_cell_exit(
    p: f32,
    d: f32,
    cell: i32,
    dim: i32,
    world_min: f32,
    world_max: f32
) -> f32 {
    if (abs(d) < 0.0000001) {
        return HUGE_T;
    }
    let width = (world_max - world_min) / f32(dim);
    var boundary = world_min + f32(cell) * width;
    if (d > 0.0) {
        boundary = boundary + width;
    }
    let distance = (boundary - p) / d;
    if (distance <= 0.000001) {
        return HUGE_T;
    }
    return distance;
}

fn next_cell_exit(point: vec3<f32>, rd: vec3<f32>, dims: vec3<i32>) -> f32 {
    let uvw = point_to_uvw(point);
    let cell = hierarchy_coord(uvw, dims);
    let tx = axis_cell_exit(point.x, rd.x, cell.x, dims.x, -1.0, 1.0);
    let ty = axis_cell_exit(point.y, rd.y, cell.y, dims.y, -1.0, 1.0);
    let tz = axis_cell_exit(point.z, rd.z, cell.z, dims.z, 0.0, u.zspan);
    return max(min(tx, min(ty, tz)), 0.00001);
}

fn support_value(uvw: vec3<f32>) -> f32 {
    return textureSampleLevel(t_support, s_volume, uvw, 0.0).r;
}

fn support_weight(value: f32) -> f32 {
    if (value <= 0.0001) {
        return 0.0;
    }
    if (u.support_mode > 0.5 && u.support_mode < 1.5) {
        return 1.0;
    }
    let floor_value = clamp(u.support_floor, 0.0, 0.95);
    let normalized = smoothstep(floor_value, 1.0, value);
    return pow(max(normalized, 0.0001), max(u.support_fade, 0.05));
}

fn support_color(value: f32) -> vec3<f32> {
    let low = vec3<f32>(0.78, 0.16, 0.15);
    let middle = vec3<f32>(0.95, 0.69, 0.16);
    let high = vec3<f32>(0.20, 0.82, 0.90);
    if (value < 0.5) {
        return mix(low, middle, value * 2.0);
    }
    return mix(middle, high, (value - 0.5) * 2.0);
}

fn refined_iso_t(ro: vec3<f32>, rd: vec3<f32>, ta_in: f32, tb_in: f32) -> f32 {
    var ta = ta_in;
    var tb = tb_in;
    var va = textureSampleLevel(t_volume, s_volume, point_to_uvw(ro + rd * ta), 0.0).r;
    let vb0 = textureSampleLevel(t_volume, s_volume, point_to_uvw(ro + rd * tb), 0.0).r;
    let rising = vb0 >= va;
    for (var iteration = 0; iteration < 6; iteration = iteration + 1) {
        let tm = 0.5 * (ta + tb);
        let vm = textureSampleLevel(t_volume, s_volume, point_to_uvw(ro + rd * tm), 0.0).r;
        if (rising) {
            if (vm < u.iso_value) {
                ta = tm;
                va = vm;
            } else {
                tb = tm;
            }
        } else {
            if (vm > u.iso_value) {
                ta = tm;
                va = vm;
            } else {
                tb = tm;
            }
        }
    }
    return 0.5 * (ta + tb);
}

fn crop_contains(point: vec3<f32>, low_z: f32, high_z: f32) -> bool {
    let x0 = mix(-1.0, 1.0, clamp(u.crop_x_min, 0.0, 0.99));
    let x1 = mix(-1.0, 1.0, clamp(u.crop_x_max, u.crop_x_min + 0.01, 1.0));
    let y0 = mix(-1.0, 1.0, clamp(u.crop_y_min, 0.0, 0.99));
    let y1 = mix(-1.0, 1.0, clamp(u.crop_y_max, u.crop_y_min + 0.01, 1.0));
    return point.x >= x0 && point.x <= x1 && point.y >= y0 && point.y <= y1
        && point.z >= low_z && point.z <= high_z;
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
    let crop_min = vec3<f32>(
        mix(-1.0, 1.0, clamp(u.crop_x_min, 0.0, 0.99)),
        mix(-1.0, 1.0, clamp(u.crop_y_min, 0.0, 0.99)),
        clip_low_z
    );
    let crop_max = vec3<f32>(
        mix(-1.0, 1.0, clamp(u.crop_x_max, u.crop_x_min + 0.01, 1.0)),
        mix(-1.0, 1.0, clamp(u.crop_y_max, u.crop_y_min + 0.01, 1.0)),
        clip_high_z
    );
    let hit = box_intersect(eye, rd, crop_min, crop_max);

    var color = vec3<f32>(0.0);
    var accumulated = 0.0;

    if (hit.y > max(hit.x, 0.0)) {
        let t0 = max(hit.x, 0.0);
        let step_count = i32(clamp(u.steps, 32.0, 256.0));
        let base_dt = (hit.y - t0) / f32(step_count);

        // Orthogonal slices are analytic plane intersections rather than a
        // sampled volume. Sort the three hit distances and composite front to back.
        if (u.render_mode > 3.5 && u.render_mode < 4.5) {
            var slice_t = array<f32, 3>(HUGE_T, HUGE_T, HUGE_T);
            if (abs(rd.x) > 0.000001) {
                slice_t[0] = (mix(-1.0, 1.0, clamp(u.slice_x, 0.0, 1.0)) - eye.x) / rd.x;
            }
            if (abs(rd.y) > 0.000001) {
                slice_t[1] = (mix(-1.0, 1.0, clamp(u.slice_y, 0.0, 1.0)) - eye.y) / rd.y;
            }
            if (abs(rd.z) > 0.000001) {
                slice_t[2] = (clamp(u.slice_z, 0.0, 1.0) * u.zspan - eye.z) / rd.z;
            }
            if (slice_t[1] < slice_t[0]) { let temp = slice_t[0]; slice_t[0] = slice_t[1]; slice_t[1] = temp; }
            if (slice_t[2] < slice_t[1]) { let temp = slice_t[1]; slice_t[1] = slice_t[2]; slice_t[2] = temp; }
            if (slice_t[1] < slice_t[0]) { let temp = slice_t[0]; slice_t[0] = slice_t[1]; slice_t[1] = temp; }
            for (var slice_index = 0; slice_index < 3; slice_index = slice_index + 1) {
                let sample_t = slice_t[slice_index];
                if (sample_t < t0 || sample_t > hit.y) { continue; }
                let point = eye + rd * sample_t;
                if (!crop_contains(point, clip_low_z, clip_high_z)) { continue; }
                let uvw = point_to_uvw(point);
                let structure = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
                let support = support_value(uvw);
                if (support <= 0.0001) { continue; }
                var lut_coord = structure;
                var transfer = threshold_strength(structure, u.threshold, u.threshold_high, u.threshold_mode, 0.08);
                if (u.velocity_mode > 0.5) {
                    transfer = smoothstep(u.ref_gate, u.ref_gate + 0.08, structure);
                    lut_coord = textureSampleLevel(t_color, s_volume, uvw, 0.0).r;
                }
                if (transfer <= 0.0) { continue; }
                let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(lut_coord, 0.5), 0.0);
                let weight = support_weight(support);
                let alpha = palette.a * u.opacity * transfer * max(weight, 0.15) * 0.72;
                var rgb = shaded_rgb(uvw, rd, palette.rgb);
                if (u.support_mode > 1.5) { rgb = support_color(support); }
                color = color + (1.0 - accumulated) * alpha * rgb;
                accumulated = accumulated + (1.0 - accumulated) * alpha;
            }
        } else {
            let jitter = (hash12(in.pos.xy) - 0.5) * clamp(u.jitter_strength, 0.0, 1.0);
            var t = t0 + max(jitter * base_dt, 0.0);
            var previous_t = t;
            var previous_structure = textureSampleLevel(
                t_volume, s_volume, point_to_uvw(eye + rd * t), 0.0
            ).r;
            var have_previous = false;
            var shell_drawn = false;
            var maximum_value = -1.0;
            var maximum_lut = 0.0;
            var maximum_support = 0.0;
            var maximum_uvw = vec3<f32>(0.5);

            for (var iteration = 0; iteration < MAX_TRAVERSAL_STEPS; iteration = iteration + 1) {
                if (t > hit.y || accumulated > 0.992) { break; }
                let point = eye + rd * t;
                let uvw = point_to_uvw(point);
                let coarse = coarse_range(uvw);
                if (!range_can_contribute(coarse)) {
                    t = t + next_cell_exit(point, rd, COARSE_DIMS) + 0.00002;
                    have_previous = false;
                    continue;
                }
                let fine = fine_range(uvw);
                if (!range_can_contribute(fine)) {
                    t = t + next_cell_exit(point, rd, FINE_DIMS) + 0.00002;
                    have_previous = false;
                    continue;
                }

                let structure = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
                let support = support_value(uvw);
                if (support <= 0.0001) {
                    t = t + base_dt;
                    have_previous = false;
                    continue;
                }

                var transfer = 0.0;
                var lut_coord = structure;
                var emphasis = 1.0;
                if (u.velocity_mode > 0.5) {
                    transfer = smoothstep(u.ref_gate, u.ref_gate + 0.08, structure);
                    let velocity = textureSampleLevel(t_color, s_volume, uvw, 0.0).r;
                    let magnitude = clamp(abs(velocity - 0.5) * 2.0, 0.0, 1.0);
                    emphasis = mix(1.0, magnitude, clamp(u.couplet_emphasis, 0.0, 1.0));
                    lut_coord = velocity;
                } else {
                    transfer = threshold_strength(
                        structure, u.threshold, u.threshold_high, u.threshold_mode, 0.08
                    );
                }

                // Maximum projection keeps the strongest contributing structure
                // while preserving velocity color at that same voxel.
                if (u.render_mode > 2.5 && u.render_mode < 3.5) {
                    let candidate = transfer * emphasis;
                    if (candidate > 0.0 && structure > maximum_value) {
                        maximum_value = structure;
                        maximum_lut = lut_coord;
                        maximum_support = support;
                        maximum_uvw = uvw;
                    }
                } else {
                    let crossed_iso = have_previous
                        && ((previous_structure < u.iso_value && structure >= u.iso_value)
                            || (previous_structure > u.iso_value && structure <= u.iso_value));
                    if (crossed_iso && (u.render_mode > 0.5 && u.render_mode < 2.5)) {
                        let surface_t = refined_iso_t(eye, rd, previous_t, t);
                        let surface_uvw = point_to_uvw(eye + rd * surface_t);
                        let surface_support = support_value(surface_uvw);
                        if (surface_support > 0.0001) {
                            var surface_lut = textureSampleLevel(t_volume, s_volume, surface_uvw, 0.0).r;
                            if (u.velocity_mode > 0.5) {
                                surface_lut = textureSampleLevel(t_color, s_volume, surface_uvw, 0.0).r;
                            }
                            let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(surface_lut, 0.5), 0.0);
                            let support_scale = support_weight(surface_support);
                            let shell_alpha = mix(0.78, 0.38, select(0.0, 1.0, u.render_mode < 1.5))
                                * max(support_scale, 0.1);
                            var shell_rgb = shaded_rgb(surface_uvw, rd, palette.rgb);
                            if (u.support_mode > 1.5 || u.render_mode > 4.5) {
                                shell_rgb = support_color(surface_support);
                            }
                            color = color + (1.0 - accumulated) * shell_alpha * shell_rgb;
                            accumulated = accumulated + (1.0 - accumulated) * shell_alpha;
                            shell_drawn = true;
                            if (u.render_mode > 1.5 && u.render_mode < 2.5) {
                                break;
                            }
                        }
                    }

                    if (transfer > 0.0 && (u.render_mode < 1.5 || u.render_mode > 4.5)) {
                        let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(lut_coord, 0.5), 0.0);
                        let support_scale = support_weight(support);
                        var sample_rgb = palette.rgb;
                        var alpha = 0.0;
                        let use_preintegration = u.preintegration > 0.5
                            && u.velocity_mode < 0.5
                            && u.render_mode < 1.5
                            && have_previous;
                        if (use_preintegration) {
                            let segment = textureSampleLevel(
                                t_preintegrated,
                                s_lut,
                                vec2<f32>(previous_structure, structure),
                                0.0
                            );
                            sample_rgb = segment.rgb;
                            alpha = 1.0 - pow(
                                max(1.0 - segment.a, 0.0001),
                                max((t - previous_t) * 28.0 * u.density, 0.01)
                            );
                        } else {
                            let raw_alpha = palette.a * u.opacity * transfer * emphasis;
                            alpha = 1.0 - pow(
                                max(1.0 - raw_alpha, 0.0001),
                                base_dt * 28.0 * max(u.density, 0.05)
                            );
                        }
                        alpha = alpha * support_scale;
                        sample_rgb = shaded_rgb(uvw, rd, sample_rgb);
                        if (u.support_mode > 1.5 || u.render_mode > 4.5) {
                            sample_rgb = support_color(support);
                        }
                        color = color + (1.0 - accumulated) * alpha * sample_rgb;
                        accumulated = accumulated + (1.0 - accumulated) * alpha;
                    }
                }

                previous_t = t;
                previous_structure = structure;
                have_previous = true;

                let interval_width = fine.g - fine.r;
                let edge_value = select(u.threshold, u.iso_value, u.render_mode > 0.5 && u.render_mode < 2.5);
                let proximity = 1.0 - clamp(abs(structure - edge_value) / 0.12, 0.0, 1.0);
                let detail = clamp(interval_width * 4.5, 0.0, 1.0);
                let adaptive = max(detail, proximity);
                let target_factor = mix(2.25, 0.55, adaptive);
                let factor = mix(1.0, target_factor, clamp(u.adaptive_strength, 0.0, 1.0));
                var step_dt = base_dt * factor;
                if (u.render_mode > 2.5 && u.render_mode < 3.5) {
                    step_dt = min(step_dt, base_dt * 0.85);
                }
                step_dt = min(step_dt, next_cell_exit(point, rd, FINE_DIMS) + 0.00002);
                t = t + max(step_dt, base_dt * 0.35);
            }

            if (u.render_mode > 2.5 && u.render_mode < 3.5 && maximum_value >= 0.0) {
                let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(maximum_lut, 0.5), 0.0);
                let support_scale = support_weight(maximum_support);
                let alpha = clamp(u.opacity * max(support_scale, 0.1), 0.08, 1.0);
                var rgb = shaded_rgb(maximum_uvw, rd, palette.rgb);
                if (u.support_mode > 1.5) { rgb = support_color(maximum_support); }
                color = rgb * alpha;
                accumulated = alpha;
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
                let floor_uv = vec2<f32>((point.x + 1.0) * 0.5, (point.y + 1.0) * 0.5);
                let floor_sample = textureSampleLevel(t_floor, s_floor, floor_uv, 0.0).rg;
                var value = floor_sample.r;
                // Requiring a fully valid filtered footprint keeps the edge
                // of the observed PPI honest instead of blending no-data zero
                // into the last valid gate.
                var floor_observed = floor_sample.g > 0.999;
                // Column-max projects the reflectivity structure column onto the
                // floor; in velocity mode that column lives in t_volume and
                // would be miscolored by the velocity LUT, so fall back to the
                // lowest-tilt velocity PPI there.
                if (u.floor_mode > 1.5 && u.velocity_mode < 0.5) {
                    let projected = column_max(floor_uv);
                    value = projected.r;
                    floor_observed = projected.g > 0.5;
                }
                if (floor_observed) {
                    let floor_transfer = threshold_strength(
                        value, u.floor_threshold, u.floor_threshold_high,
                        u.floor_threshold_mode, 0.04
                    );
                    if (floor_transfer > 0.0) {
                        let palette = textureSampleLevel(t_lut, s_lut, vec2<f32>(value, 0.5), 0.0);
                        let alpha = palette.a * u.floor_opacity * floor_transfer;
                        color = color + (1.0 - accumulated) * alpha * palette.rgb;
                        accumulated = accumulated + (1.0 - accumulated) * alpha;
                    }
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
    color_tex: wgpu::Texture,
    light_pipeline: wgpu::ComputePipeline,
    light_bind_group: wgpu::BindGroup,
    light_uniforms: wgpu::Buffer,
    volume_generation: u64,
    last_lighting_revision: Option<u64>,
    support_tex: wgpu::Texture,
    hierarchy_fine_tex: wgpu::Texture,
    hierarchy_coarse_tex: wgpu::Texture,
    preintegrated_tex: wgpu::Texture,
    lut_cpu: Mutex<Vec<u8>>,
    preintegration_signature: Mutex<u64>,
}

/// One-time GPU setup (eframe custom-3D pattern: call from the app
/// constructor with `cc.wgpu_render_state`).
pub fn init_gpu(render_state: &egui_wgpu::RenderState) {
    let device = &render_state.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vol3d"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let light_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("vol3d-light-volume"),
        source: wgpu::ShaderSource::Wgsl(LIGHT_VOLUME_SHADER.into()),
    });
    let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vol3d-uniforms"),
        size: UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let light_uniforms = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vol3d-light-uniforms"),
        size: LIGHTING_UNIFORM_BYTES,
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
        format: wgpu::TextureFormat::Rg8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // Velocity color box: same lattice as the volume, sampled only when the
    // shader is in velocity mode. Zeroed until a velocity two-box arrives.
    let color_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-color"),
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
    let light_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-light-volume"),
        size: wgpu::Extent3d {
            width: LIGHT_VOLUME_N,
            height: LIGHT_VOLUME_N,
            depth_or_array_layers: LIGHT_VOLUME_NZ,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::STORAGE_BINDING,
        view_formats: &[],
    });

    let support_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-support"),
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
    let hierarchy_fine_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-hierarchy-fine"),
        size: wgpu::Extent3d {
            width: advanced::FINE_X as u32,
            height: advanced::FINE_Y as u32,
            depth_or_array_layers: advanced::FINE_Z as u32,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let hierarchy_coarse_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-hierarchy-coarse"),
        size: wgpu::Extent3d {
            width: advanced::COARSE_X as u32,
            height: advanced::COARSE_Y as u32,
            depth_or_array_layers: advanced::COARSE_Z as u32,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D3,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let preintegrated_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vol3d-preintegrated-transfer"),
        size: wgpu::Extent3d {
            width: 256,
            height: 256,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
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
            wgpu::BindGroupLayoutEntry {
                binding: 7,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 8,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 9,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 10,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 11,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 12,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
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
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::TextureView(
                    &color_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 8,
                resource: wgpu::BindingResource::TextureView(
                    &light_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 9,
                resource: wgpu::BindingResource::TextureView(
                    &support_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 10,
                resource: wgpu::BindingResource::TextureView(
                    &hierarchy_fine_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 11,
                resource: wgpu::BindingResource::TextureView(
                    &hierarchy_coarse_tex.create_view(&Default::default()),
                ),
            },
            wgpu::BindGroupEntry {
                binding: 12,
                resource: wgpu::BindingResource::TextureView(
                    &preintegrated_tex.create_view(&Default::default()),
                ),
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

    let light_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("vol3d-light-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D3,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D3,
                },
                count: None,
            },
        ],
    });
    let light_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vol3d-light-bg"),
        layout: &light_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: light_uniforms.as_entire_binding(),
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
                    &light_tex.create_view(&Default::default()),
                ),
            },
        ],
    });
    let light_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vol3d-light-pl"),
        bind_group_layouts: &[Some(&light_layout)],
        immediate_size: 0,
    });
    let light_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("vol3d-light-pipeline"),
        layout: Some(&light_pipeline_layout),
        module: &light_shader,
        entry_point: Some("cs_main"),
        compilation_options: Default::default(),
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
            color_tex,
            light_pipeline,
            light_bind_group,
            light_uniforms,
            volume_generation: 0,
            last_lighting_revision: None,
            support_tex,
            hierarchy_fine_tex,
            hierarchy_coarse_tex,
            preintegrated_tex,
            lut_cpu: Mutex::new(vec![0; 256 * 4]),
            preintegration_signature: Mutex::new(u64::MAX),
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
    pub lighting: Vol3dLightingSettings,
    pub clip_low: f32,
    pub clip_high: f32,
    pub floor_threshold01: f32,
    pub floor_threshold_high01: f32,
    pub floor_threshold_mode: Vol3dThresholdMode,
    pub focus_height: f32,
    /// 0 = single-box (reflectivity and every other moment), 1 = velocity
    /// two-box (reflectivity structure + velocity color).
    pub velocity_mode: f32,
    /// Normalized reflectivity structure gate for velocity mode.
    pub ref_gate: f32,
    /// Couplet emphasis 0..1 for velocity mode.
    pub couplet_emphasis: f32,
    // Second-generation analysis parameters. These mirror the matching fields
    // on `Vol3d`, except where the shader needs a different domain than the UI
    // (see `iso_value`/`iso_width`/`preintegration` below). The two enums are
    // carried as enums, not floats, so `shader_value()` stays the single place
    // that defines the uniform encoding; a float here would let a UI change
    // silently disagree with the WGSL branch numbering.
    pub render_mode: Vol3dRenderMode,
    pub support_mode: SupportMode,
    /// Support value below which the honest-fade path becomes transparent.
    pub support_floor: f32,
    pub support_fade: f32,
    /// Isosurface level, NORMALIZED to the active palette range. The UI holds
    /// the physical value (dBZ, m/s, dB, ...) because that is what a forecaster
    /// reasons about; the shader compares against the normalized R8 texel.
    pub iso_value: f32,
    /// Shell half-width in the same normalized units as `iso_value`.
    pub iso_width: f32,
    /// Stable sub-voxel ray offset. It reduces banding without temporal shimmer.
    pub jitter_strength: f32,
    /// Segment-preintegrated transfer flag, encoded 0/1 for the float uniform
    /// block. The shader, not this struct, applies contract 7 (preintegration
    /// is force-disabled for the velocity two-box path).
    pub preintegration: f32,
    /// Horizontal crop box in normalized volume coordinates.
    pub crop_x_min: f32,
    pub crop_x_max: f32,
    pub crop_y_min: f32,
    pub crop_y_max: f32,
    /// Orthogonal slice positions in normalized volume coordinates.
    pub slice_x: f32,
    pub slice_y: f32,
    pub slice_z: f32,
    /// 0 = fixed reference sampling inside occupied bricks; 1 = full adaptive.
    /// The 0 end is the A/B reference path required by contract 8.
    pub adaptive_strength: f32,
    pub pending: Arc<Mutex<PendingUploads>>,
}

impl egui_wgpu::CallbackTrait for Vol3dCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(resources) = resources.get_mut::<Vol3dResources>() else {
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
            self.velocity_mode,
            self.ref_gate,
            self.couplet_emphasis,
            self.lighting.rim_strength,
            if self.lighting.enabled { 1.0 } else { 0.0 },
            LIGHTING_ENCODE_MAX,
            0.0,
            self.render_mode.shader_value(),
            self.support_mode.shader_value(),
            self.support_floor,
            self.support_fade,
            self.iso_value,
            self.iso_width,
            self.jitter_strength,
            self.preintegration,
            self.crop_x_min,
            self.crop_x_max,
            self.crop_y_min,
            self.crop_y_max,
            self.slice_x,
            self.slice_y,
            self.slice_z,
            self.adaptive_strength,
        ];
        let mut bytes = [0u8; UNIFORM_FLOATS * std::mem::size_of::<f32>()];
        for (index, value) in uniforms.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        queue.write_buffer(&resources.uniforms, 0, &bytes);

        let mut uploaded_volume = false;
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(volume) = pending.volume.take() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.support_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &volume.support_data,
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
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.hierarchy_fine_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &volume.hierarchy_fine,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some((advanced::FINE_X * 4) as u32),
                        rows_per_image: Some(advanced::FINE_Y as u32),
                    },
                    wgpu::Extent3d {
                        width: advanced::FINE_X as u32,
                        height: advanced::FINE_Y as u32,
                        depth_or_array_layers: advanced::FINE_Z as u32,
                    },
                );
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &resources.hierarchy_coarse_tex,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &volume.hierarchy_coarse,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some((advanced::COARSE_X * 4) as u32),
                        rows_per_image: Some(advanced::COARSE_Y as u32),
                    },
                    wgpu::Extent3d {
                        width: advanced::COARSE_X as u32,
                        height: advanced::COARSE_Y as u32,
                        depth_or_array_layers: advanced::COARSE_Z as u32,
                    },
                );

                uploaded_volume = true;
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
                if let Some(color_data) = volume.color_data {
                    queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &resources.color_tex,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &color_data,
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
                }
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
                            bytes_per_row: Some((FLOOR_N * 2) as u32),
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
                if let Ok(mut cached) = resources.lut_cpu.lock() {
                    *cached = lut.clone();
                }
                if let Ok(mut signature) = resources.preintegration_signature.lock() {
                    *signature = u64::MAX;
                }
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

        let cached_lut = resources
            .lut_cpu
            .lock()
            .map(|lut| lut.clone())
            .unwrap_or_else(|_| vec![0; 256 * 4]);
        let signature = advanced::preintegration_signature(
            &cached_lut,
            self.threshold01,
            self.threshold_high01,
            self.threshold_mode.shader_value(),
            self.opacity,
        );
        // Exact CPU mirror of the shader's `use_preintegration` gate: the table
        // is sampled only by the direct-volume path, only with preintegration
        // on, and never in velocity two-box (contract 7). Building it costs
        // 256 x 256 x 16 substeps with a `powf` each, on the render thread, and
        // `opacity` is part of the signature - so without this gate, dragging
        // the opacity slider rebuilt a million-substep table every frame even
        // in the modes that never read it. Skipping deliberately leaves the
        // stored signature stale, which is exactly what makes the table rebuild
        // on the first frame after the user turns preintegration back on.
        let preintegration_table_is_read = self.preintegration > 0.5
            && self.velocity_mode < 0.5
            && self.render_mode.shader_value() < 1.5;
        let rebuild_preintegration = preintegration_table_is_read
            && resources
                .preintegration_signature
                .lock()
                .map(|stored| *stored != signature)
                .unwrap_or(true);
        if rebuild_preintegration {
            let table = advanced::build_preintegrated_lut(
                &cached_lut,
                self.threshold01,
                self.threshold_high01,
                self.threshold_mode.shader_value(),
                self.opacity,
            );
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &resources.preintegrated_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &table,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256 * 4),
                    rows_per_image: Some(256),
                },
                wgpu::Extent3d {
                    width: 256,
                    height: 256,
                    depth_or_array_layers: 1,
                },
            );
            if let Ok(mut stored) = resources.preintegration_signature.lock() {
                *stored = signature;
            }
        }

        if uploaded_volume {
            resources.volume_generation = resources.volume_generation.wrapping_add(1);
        }

        if self.lighting.enabled && self.shading > 0.001 {
            let settings_revision = self.lighting.cache_revision(
                self.threshold01,
                self.threshold_high01,
                self.threshold_mode.shader_value(),
                self.velocity_mode,
                self.ref_gate,
                self.zspan,
            );
            let desired_revision =
                combine_cache_revision(settings_revision, resources.volume_generation);
            if resources.last_lighting_revision != Some(desired_revision) {
                let light = self.lighting.light_direction();
                let coefficients = self.lighting.log_sh_coefficients();
                let lighting_uniforms: [f32; LIGHTING_UNIFORM_FLOATS] = [
                    light[0],
                    light[1],
                    light[2],
                    0.0,
                    self.lighting.ambient_strength,
                    self.lighting.key_strength,
                    self.lighting.shadow_strength,
                    self.lighting.shadow_density,
                    self.threshold01,
                    self.threshold_high01,
                    self.threshold_mode.shader_value(),
                    self.ref_gate,
                    self.zspan,
                    self.lighting.shadow_steps.clamp(1, MAX_SHADOW_STEPS) as f32,
                    self.velocity_mode,
                    LIGHTING_ENCODE_MAX,
                    coefficients[0],
                    coefficients[1],
                    coefficients[2],
                    coefficients[3],
                    coefficients[4],
                    coefficients[5],
                    coefficients[6],
                    coefficients[7],
                    coefficients[8],
                    coefficients[9],
                    coefficients[10],
                    coefficients[11],
                    coefficients[12],
                    coefficients[13],
                    coefficients[14],
                    coefficients[15],
                ];
                let mut lighting_bytes =
                    [0u8; LIGHTING_UNIFORM_FLOATS * std::mem::size_of::<f32>()];
                for (index, value) in lighting_uniforms.iter().enumerate() {
                    lighting_bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                queue.write_buffer(&resources.light_uniforms, 0, &lighting_bytes);

                {
                    let mut compute_pass =
                        encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("vol3d-light-volume"),
                            timestamp_writes: None,
                        });
                    compute_pass.set_pipeline(&resources.light_pipeline);
                    compute_pass.set_bind_group(0, &resources.light_bind_group, &[]);
                    compute_pass.dispatch_workgroups(
                        LIGHT_VOLUME_N.div_ceil(LIGHT_WORKGROUP_SIZE),
                        LIGHT_VOLUME_N.div_ceil(LIGHT_WORKGROUP_SIZE),
                        LIGHT_VOLUME_NZ.div_ceil(LIGHT_WORKGROUP_SIZE),
                    );
                }
                resources.last_lighting_revision = Some(desired_revision);
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

    /// `empty_box` hand-writes the zeros the general normalize + hierarchy path
    /// produces for an all-no-data volume, because it runs on the UI thread on
    /// every repaint while a live volume is still building. Pin the two against
    /// each other so the shortcut cannot silently drift from the real path.
    #[test]
    fn empty_box_matches_general_path() {
        let voxels = BOX_N * BOX_N * BOX_NZ;
        let no_data = vec![f32::NAN; voxels];
        let reference = normalize_box_with_range(&no_data, BOX_N, BOX_NZ, 0.0, 1.0);
        let fast = empty_box();
        assert_eq!(fast.n, reference.n);
        assert_eq!(fast.nz, reference.nz);
        assert_eq!(fast.data, reference.data);
        assert_eq!(fast.support_data, reference.support_data);
        assert_eq!(fast.hierarchy_fine, reference.hierarchy_fine);
        assert_eq!(fast.hierarchy_coarse, reference.hierarchy_coarse);
        assert_eq!(fast.floor_data, reference.floor_data);
        assert!(fast.color_data.is_none() && reference.color_data.is_none());
        assert!(
            (fast.acceleration_empty_fraction - reference.acceleration_empty_fraction).abs()
                < 1.0e-6
        );
    }

    #[test]
    fn empty_floor_is_explicitly_masked_instead_of_looking_like_a_low_value() {
        let floor = empty_box().floor_data.expect("empty floor upload");
        assert_eq!(floor.len(), FLOOR_N * FLOOR_N * 2);
        assert!(
            floor
                .chunks_exact(2)
                .all(|texel| texel == [0, 0]),
            "no-data floor texels must carry a zero validity channel"
        );
    }

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

    #[test]
    fn normalize_values_maps_range_and_rejects_nan() {
        // Signed velocity range: min -> 0, zero -> mid, max -> 255.
        let values = [-100.0, 0.0, 100.0, f32::NAN, -250.0, 999.0];
        let out = normalize_values(&values, -100.0, 100.0);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], 128); // 0 m/s is the diverging-colormap center
        assert_eq!(out[2], 255);
        assert_eq!(out[3], 0); // no-data
        assert_eq!(out[4], 0); // clamped below min
        assert_eq!(out[5], 255); // clamped above max
    }

    #[test]
    fn normalize_box_data_matches_helper() {
        let values = vec![-40.0, -10.0, 0.0, 25.0, 60.0, f32::NAN];
        let vbox = normalize_box_with_range(&values, 1, values.len(), -50.0, 60.0);
        assert_eq!(vbox.data, normalize_values(&values, -50.0, 60.0));
        assert!(vbox.color_data.is_none(), "single box has no color plane");
    }

    #[test]
    fn two_box_color_plane_matches_structure_dims() {
        // In velocity mode the structure (reflectivity) box and the velocity
        // color box must share the same lattice so the shader can sample both
        // at one uvw.
        let refl = vec![5.0f32; BOX_N * BOX_N * BOX_NZ];
        let vel = vec![3.0f32; BOX_N * BOX_N * BOX_NZ];
        let mut vbox = normalize_box_with_range(&refl, BOX_N, BOX_NZ, 0.0, 80.0);
        vbox.color_data = Some(normalize_values(&vel, -100.0, 100.0));
        assert_eq!(vbox.color_data.as_ref().unwrap().len(), vbox.data.len());
    }

    #[test]
    fn velocity_presets_are_ordered() {
        let mut v = Vol3d::default();
        v.apply_velocity_broad_preset();
        let broad = (v.vel_ref_gate_dbz, v.vel_couplet_emphasis, v.opacity);
        v.apply_velocity_couplet_preset();
        let couplet = (v.vel_ref_gate_dbz, v.vel_couplet_emphasis, v.opacity);
        v.apply_velocity_shear_preset();
        let shear = (v.vel_ref_gate_dbz, v.vel_couplet_emphasis, v.opacity);

        // Reflectivity gate, couplet emphasis, and opacity all tighten as the
        // preset moves from broad flow toward strong-shear isolation.
        assert!(broad.0 < couplet.0 && couplet.0 < shear.0);
        assert!(broad.1 < couplet.1 && couplet.1 < shear.1);
        assert!(broad.2 < couplet.2 && couplet.2 < shear.2);
        // Emphasis is a 0..1 mix factor; broad flow applies none.
        assert_eq!(broad.1, 0.0);
        for emphasis in [broad.1, couplet.1, shear.1] {
            assert!((0.0..=1.0).contains(&emphasis));
        }
    }

    #[test]
    fn default_velocity_fields_are_sane() {
        let v = Vol3d::default();
        assert!(v.vel_ref_gate_dbz > 0.0 && v.vel_ref_gate_dbz < 80.0);
        assert!((0.0..=1.0).contains(&v.vel_couplet_emphasis));
        assert!(!v.velocity_color_active);
    }

    #[test]
    fn shaders_parse_and_validate() {
        // The build nodes are headless: wgpu never builds these pipelines, so
        // Naga validates both embedded shaders without requiring an adapter.
        for (label, source) in [("raymarch", SHADER), ("light-volume", LIGHT_VOLUME_SHADER)] {
            let module = naga::front::wgsl::parse_str(source)
                .unwrap_or_else(|error| panic!("vol3d {label} WGSL failed to parse: {error:?}"));
            let mut validator = naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            );
            validator
                .validate(&module)
                .unwrap_or_else(|error| panic!("vol3d {label} WGSL failed to validate: {error:?}"));
        }
    }
}
