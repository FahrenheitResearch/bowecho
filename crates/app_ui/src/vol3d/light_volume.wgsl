// Compute-cached lighting for BowEcho's 3D radar explorer.
//
// RGB stores an outward threshold-surface normal encoded to 0..1. Alpha stores
// scalar palette-preserving illumination divided by `encode_max`. The source
// radar field is treated only as visual occupancy; the attenuation below is
// not a meteorological or optical retrieval.

struct LightingUniforms {
    // xyz: unit vector from sample toward key light; w unused.
    light_direction: vec4<f32>,
    // ambient, key, shadow blend, pseudo-extinction density.
    strengths: vec4<f32>,
    // threshold low, threshold high, threshold mode, reflectivity gate.
    transfer: vec4<f32>,
    // zspan, shadow steps, velocity mode, lighting encode maximum.
    volume: vec4<f32>,
    sh_0_3: vec4<f32>,
    sh_4_7: vec4<f32>,
    sh_8_11: vec4<f32>,
    sh_12_15: vec4<f32>,
};

@group(0) @binding(0) var<uniform> p: LightingUniforms;
@group(0) @binding(1) var t_volume: texture_3d<f32>;
@group(0) @binding(2) var s_volume: sampler;
@group(0) @binding(3) var t_light_out: texture_storage_3d<rgba8unorm, write>;

const SOURCE_DX: f32 = 1.0 / 192.0;
const SOURCE_DZ: f32 = 1.0 / 48.0;
const MAX_SHADOW_STEPS: i32 = 48;

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

fn source_value(uvw: vec3<f32>) -> f32 {
    return textureSampleLevel(t_volume, s_volume, uvw, 0.0).r;
}

fn occupancy_from_value(value: f32) -> f32 {
    if (p.volume.z > 0.5) {
        return smoothstep(p.transfer.w, p.transfer.w + 0.08, value);
    }
    return max(
        threshold_strength(value, p.transfer.x, p.transfer.y, p.transfer.z, 0.08),
        0.0
    );
}

fn occupancy(uvw: vec3<f32>) -> f32 {
    return occupancy_from_value(source_value(uvw));
}

fn outward_normal(uvw: vec3<f32>) -> vec3<f32> {
    // Read each neighbor once. The occupancy gradient gives the correct sign at
    // the visible transfer boundary; the raw-field gradient remains useful in
    // saturated interiors where smoothstep has flattened occupancy to one.
    let x_positive = source_value(uvw + vec3<f32>(SOURCE_DX, 0.0, 0.0));
    let x_negative = source_value(uvw - vec3<f32>(SOURCE_DX, 0.0, 0.0));
    let y_positive = source_value(uvw + vec3<f32>(0.0, SOURCE_DX, 0.0));
    let y_negative = source_value(uvw - vec3<f32>(0.0, SOURCE_DX, 0.0));
    let z_positive = source_value(uvw + vec3<f32>(0.0, 0.0, SOURCE_DZ));
    let z_negative = source_value(uvw - vec3<f32>(0.0, 0.0, SOURCE_DZ));

    // Convert the central differences to the relative world-space metric.
    // X/Y span two world units over 192 cells; Z spans `zspan` over 48 cells.
    let z_scale = 0.5 / max(p.volume.x, 0.01);
    let raw_gradient = vec3<f32>(
        x_positive - x_negative,
        y_positive - y_negative,
        (z_positive - z_negative) * z_scale
    );
    let occupancy_gradient = vec3<f32>(
        occupancy_from_value(x_positive) - occupancy_from_value(x_negative),
        occupancy_from_value(y_positive) - occupancy_from_value(y_negative),
        (occupancy_from_value(z_positive) - occupancy_from_value(z_negative)) * z_scale
    );

    var into_body_gradient = occupancy_gradient;
    if (dot(into_body_gradient, into_body_gradient) <= 0.0000005) {
        var transfer_sense = 1.0;
        if (p.volume.z < 0.5) {
            if (p.transfer.z > 0.5 && p.transfer.z < 1.5) {
                // "Below" occupancy increases as the source value decreases.
                transfer_sense = -1.0;
            } else if (p.transfer.z > 1.5) {
                // "Outside" has one low-valued and one high-valued body.
                let center_value = source_value(uvw);
                transfer_sense = select(-1.0, 1.0, center_value >= p.transfer.y);
            }
        }
        into_body_gradient = raw_gradient * transfer_sense;
    }

    if (dot(into_body_gradient, into_body_gradient) > 0.0000005) {
        // The gradient points into the visible echo body; negate it to obtain a
        // stable outward normal. This removes the former abs(N.L) shortcut.
        return normalize(-into_body_gradient);
    }
    // Zero is a sentinel for a locally homogeneous field. The render shader
    // then preserves unlit palette color, matching the former gradient path.
    return vec3<f32>(0.0);
}

fn log_sh_l3(normal: vec3<f32>) -> f32 {
    let x = normal.x;
    let y = normal.y;
    let z = normal.z;
    let b0 = vec4<f32>(
        0.2820948,
        0.48860252 * y,
        0.48860252 * z,
        0.48860252 * x
    );
    let b1 = vec4<f32>(
        1.0925485 * x * y,
        1.0925485 * y * z,
        0.31539157 * (3.0 * z * z - 1.0),
        1.0925485 * x * z
    );
    let b2 = vec4<f32>(
        0.54627424 * (x * x - y * y),
        0.5900436 * y * (3.0 * x * x - y * y),
        2.8906114 * x * y * z,
        0.4570458 * y * (5.0 * z * z - 1.0)
    );
    let b3 = vec4<f32>(
        0.37317634 * z * (5.0 * z * z - 3.0),
        0.4570458 * x * (5.0 * z * z - 1.0),
        1.4453057 * z * (x * x - y * y),
        0.5900436 * x * (x * x - 3.0 * y * y)
    );
    return dot(p.sh_0_3, b0)
        + dot(p.sh_4_7, b1)
        + dot(p.sh_8_11, b2)
        + dot(p.sh_12_15, b3);
}

fn uvw_to_world(uvw: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(uvw.xy * 2.0 - 1.0, uvw.z * p.volume.x);
}

fn world_to_uvw(world: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(world.xy * 0.5 + 0.5, world.z / max(p.volume.x, 0.01));
}

fn axis_exit(position: f32, direction: f32, minimum: f32, maximum: f32) -> f32 {
    if (direction > 0.00001) {
        return (maximum - position) / direction;
    }
    if (direction < -0.00001) {
        return (minimum - position) / direction;
    }
    return 1000000.0;
}

fn key_transmittance(uvw: vec3<f32>) -> f32 {
    if (p.strengths.z <= 0.001 || p.strengths.w <= 0.001) {
        return 1.0;
    }
    let world = uvw_to_world(uvw);
    let light = normalize(p.light_direction.xyz);
    let exit_x = axis_exit(world.x, light.x, -1.0, 1.0);
    let exit_y = axis_exit(world.y, light.y, -1.0, 1.0);
    let exit_z = axis_exit(world.z, light.z, 0.0, p.volume.x);
    let exit_distance = max(min(exit_x, min(exit_y, exit_z)), 0.0);
    let step_count = clamp(i32(p.volume.y), 1, MAX_SHADOW_STEPS);
    let step_distance = exit_distance / f32(step_count + 1);
    var optical_depth = 0.0;

    for (var index = 0; index < MAX_SHADOW_STEPS; index = index + 1) {
        if (index >= step_count) {
            break;
        }
        let distance = f32(index + 1) * step_distance;
        let sample_uvw = world_to_uvw(world + light * distance);
        optical_depth = optical_depth
            + occupancy(sample_uvw) * step_distance * max(p.strengths.w, 0.0);
        if (optical_depth >= 9.0) {
            break;
        }
    }
    return exp(-optical_depth);
}

@compute @workgroup_size(4, 4, 4)
fn cs_main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dimensions = textureDimensions(t_light_out);
    if (any(gid >= dimensions)) {
        return;
    }
    let uvw = (vec3<f32>(gid) + vec3<f32>(0.5)) / vec3<f32>(dimensions);
    let candidate_normal = outward_normal(uvw);
    let normal_valid = dot(candidate_normal, candidate_normal) > 0.00001;
    var normal = vec3<f32>(0.0);
    var lighting = 1.0;
    if (normal_valid) {
        normal = normalize(candidate_normal);
        let ambient = exp(clamp(log_sh_l3(normal), -8.0, 4.0));
        let n_dot_l = max(dot(normal, normalize(p.light_direction.xyz)), 0.0);
        var transmittance = 1.0;
        if (
            n_dot_l > 0.001
            && p.strengths.z > 0.001
            && p.strengths.w > 0.001
        ) {
            let local_occupancy = occupancy(uvw);
            if (local_occupancy > 0.001) {
                transmittance = key_transmittance(uvw);
            }
        }
        let key_visibility = mix(1.0, transmittance, clamp(p.strengths.z, 0.0, 1.0));
        lighting = max(
            p.strengths.x * ambient + p.strengths.y * n_dot_l * key_visibility,
            0.0
        );
    }

    let encoded_normal = normal * 0.5 + vec3<f32>(0.5);
    let encoded_lighting = clamp(lighting / max(p.volume.w, 0.01), 0.0, 1.0);
    textureStore(
        t_light_out,
        vec3<i32>(gid),
        vec4<f32>(encoded_normal, encoded_lighting)
    );
}
