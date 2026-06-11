//! 3D Volume Explorer: GPU direct volume rendering of the reflectivity
//! volume — the GR2Analyst Volume Explorer approach (verified from
//! GRLevelX's own documentation: translucent DVR with a user alpha
//! transfer function, NOT marching cubes; docs/xsection-3d-spec.md).
//!
//! Pipeline: render2d::volume_box_resample builds a Cartesian box
//! (~120 km square, 0..18 km) around the view center on a background
//! thread → uploaded as a wgpu 3D texture (R8Unorm, dBZ normalized
//! 0..80) → a WGSL fragment raymarcher composites front-to-back through
//! an alpha transfer function (threshold + opacity over the live
//! reflectivity colortable, uploaded as a 256×1 LUT).
//!
//! Rendering runs inside an egui-wgpu paint callback; resources are
//! created once at app startup (eframe's custom-3D pattern) so the
//! per-frame cost is one uniform write + one fullscreen-triangle draw.

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu};
use std::sync::{Arc, Mutex, mpsc};

pub const BOX_N: usize = 192;
pub const BOX_NZ: usize = 48;
pub const BOX_HALF_KM: f32 = 60.0;
pub const BOX_TOP_M: f32 = 18_000.0;

/// UI-thread state.
pub struct Vol3d {
    pub open: bool,
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub threshold_dbz: f32,
    pub opacity: f32,
    pub resample_rx: Option<mpsc::Receiver<Option<VolumeBox>>>,
    /// (volume_time_ms, top REF elevation in tenths of a degree, center
    /// east/10km, center north/10km, box half km) — top-tilt inclusion
    /// makes a completing live volume re-trigger the resample.
    pub volume_key: Option<(i64, i32, i32, i32, i32)>,
    /// Top REF elevation of the last FULLY-built box — the hold gate
    /// (live partial volumes must not bake one-sector fragments; the
    /// SAILS/cross-section lesson).
    pub last_top_deg: f32,
    /// Box half-width, km (60/120/180 = 120/240/360 km boxes; resolution
    /// per cell scales, texture stays 192^2 x 48).
    pub box_half_km: f32,
    pub status: String,
    /// Uploads waiting for the GPU (drained in `prepare`).
    pub pending: Arc<Mutex<PendingUploads>>,
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
}

impl Default for Vol3d {
    fn default() -> Self {
        Self {
            open: false,
            yaw: 0.6,
            pitch: 0.45,
            dist: 2.4,
            threshold_dbz: 35.0,
            opacity: 0.55,
            resample_rx: None,
            volume_key: None,
            last_top_deg: 0.0,
            box_half_km: BOX_HALF_KM,
            status: String::new(),
            pending: Arc::new(Mutex::new(PendingUploads::default())),
        }
    }
}

/// Normalize a dBZ box into bytes (0..80 dBZ -> 0..255; NaN/below -> 0).
pub fn normalize_box(values: &[f32], n: usize, nz: usize) -> VolumeBox {
    let data = values
        .iter()
        .map(|v| {
            if v.is_finite() {
                ((v / 80.0).clamp(0.0, 1.0) * 255.0) as u8
            } else {
                0
            }
        })
        .collect();
    VolumeBox { data, n, nz }
}

const SHADER: &str = r#"
struct Uniforms {
    yaw: f32,
    pitch: f32,
    dist: f32,
    threshold: f32,
    opacity: f32,
    aspect: f32,
    _pad0: f32,
    _pad1: f32,
};
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var t_volume: texture_3d<f32>;
@group(0) @binding(2) var s_volume: sampler;
@group(0) @binding(3) var t_lut: texture_2d<f32>;
@group(0) @binding(4) var s_lut: sampler;

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

const ZSPAN: f32 = 0.6;

fn box_intersect(ro: vec3<f32>, rd: vec3<f32>, bmin: vec3<f32>, bmax: vec3<f32>) -> vec2<f32> {
    let inv = 1.0 / rd;
    let t0 = (bmin - ro) * inv;
    let t1 = (bmax - ro) * inv;
    let tmin = min(t0, t1);
    let tmax = max(t0, t1);
    return vec2<f32>(max(max(tmin.x, tmin.y), tmin.z), min(min(tmax.x, tmax.y), tmax.z));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let cy = cos(u.yaw); let sy = sin(u.yaw);
    let cp = cos(u.pitch); let sp = sin(u.pitch);
    let center = vec3<f32>(0.0, 0.0, ZSPAN * 0.35);
    let eye = center + u.dist * vec3<f32>(cy * cp, sy * cp, sp);
    let fwd = normalize(center - eye);
    let right = normalize(cross(fwd, vec3<f32>(0.0, 0.0, 1.0)));
    let up = cross(right, fwd);
    let ndc = (in.uv * 2.0 - 1.0) * vec2<f32>(u.aspect, 1.0);
    let rd = normalize(fwd + 0.7 * (ndc.x * right + ndc.y * up));

    let t = box_intersect(eye, rd, vec3<f32>(-1.0, -1.0, 0.0), vec3<f32>(1.0, 1.0, ZSPAN));
    if (t.y <= max(t.x, 0.0)) {
        return vec4<f32>(0.0);
    }
    let t0 = max(t.x, 0.0);
    let STEPS = 160;
    let dt = (t.y - t0) / f32(STEPS);
    var col = vec3<f32>(0.0);
    var acc = 0.0;
    for (var i = 0; i < STEPS; i = i + 1) {
        let p = eye + rd * (t0 + (f32(i) + 0.5) * dt);
        let uvw = vec3<f32>((p.x + 1.0) * 0.5, (p.y + 1.0) * 0.5, p.z / ZSPAN);
        let v = textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
        if (v <= u.threshold) {
            continue;
        }
        let c = textureSampleLevel(t_lut, s_lut, vec2<f32>(v, 0.5), 0.0);
        var a = c.a * u.opacity * smoothstep(u.threshold, u.threshold + 0.08, v);
        a = 1.0 - pow(1.0 - a, dt * 28.0);
        col = col + (1.0 - acc) * a * c.rgb;
        acc = acc + (1.0 - acc) * a;
        if (acc > 0.97) {
            break;
        }
    }
    return vec4<f32>(col, acc);
}
"#;

/// GPU resources, created once at startup and stored in egui-wgpu's
/// callback_resources typemap.
pub struct Vol3dResources {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    volume_tex: wgpu::Texture,
    lut_tex: wgpu::Texture,
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
        size: 32,
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
        });
}

/// Per-frame paint callback: uniforms + pending texture uploads in
/// `prepare`, one fullscreen-triangle draw in `paint`.
pub struct Vol3dCallback {
    pub yaw: f32,
    pub pitch: f32,
    pub dist: f32,
    pub threshold01: f32,
    pub opacity: f32,
    pub aspect: f32,
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
        let Some(r) = resources.get::<Vol3dResources>() else {
            return Vec::new();
        };
        let u: [f32; 8] = [
            self.yaw,
            self.pitch,
            self.dist,
            self.threshold01,
            self.opacity,
            self.aspect,
            0.0,
            0.0,
        ];
        let mut bytes = [0u8; 32];
        for (i, v) in u.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        queue.write_buffer(&r.uniforms, 0, &bytes);
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(volume) = pending.volume.take() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &r.volume_tex,
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
            }
            if let Some(lut) = pending.lut.take() {
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &r.lut_tex,
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
        let Some(r) = resources.get::<Vol3dResources>() else {
            return;
        };
        render_pass.set_pipeline(&r.pipeline);
        render_pass.set_bind_group(0, &r.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}
