// BowEcho deterministic CUDA LUT segment executor, revision 1.
//
// One warp owns one ordered PSD segment. Lanes 0..8 correspond exactly to
// AdditiveScattering's nine components; every lane visits nodes and ascending
// corner masks in CPU order. There are no atomics or cross-lane floating-point
// reductions, so CUDA does not reorder the scientific accumulation.

// Keep the source NVRTC-self-contained: normal application builds do not ship
// a Toolkit include directory, and these are the CUDA ABI widths on every
// supported 64-bit host/device target.
typedef unsigned int uint32_t;
typedef unsigned long long uint64_t;

#define BOWECHO_COMPONENTS 9u
#define BOWECHO_MAX_ACTIVE_AXES 8u
#define BOWECHO_COMPONENT_MASK 0x1ffu

enum BowEchoCudaError : uint32_t {
    BOWECHO_CUDA_OK = 0,
    BOWECHO_CUDA_INVALID_PLAN = 1,
    BOWECHO_CUDA_TABLE_INDEX = 2,
    BOWECHO_CUDA_NONFINITE = 3,
};

extern "C" __global__ void bowecho_p3_lut_segments_v1(
    const double* __restrict__ table_values,
    uint64_t table_point_count,
    const uint64_t* __restrict__ base_point_indices,
    const uint64_t* __restrict__ upper_point_offsets,
    const double* __restrict__ upper_fractions,
    const uint32_t* __restrict__ active_axis_counts,
    const double* __restrict__ number_concentrations,
    const double* __restrict__ fall_speeds,
    uint32_t node_count,
    const uint32_t* __restrict__ segment_starts,
    const uint32_t* __restrict__ segment_counts,
    uint32_t segment_count,
    double* __restrict__ segment_outputs,
    uint32_t* __restrict__ segment_error_codes,
    uint32_t* __restrict__ segment_error_nodes) {
    const uint32_t segment = blockIdx.x;
    const uint32_t lane = threadIdx.x;
    if (segment >= segment_count || lane >= BOWECHO_COMPONENTS) {
        return;
    }

    const uint32_t first = segment_starts[segment];
    const uint32_t count = segment_counts[segment];
    if (first > node_count || count > node_count - first) {
        if (lane == 0) {
            segment_error_codes[segment] = BOWECHO_CUDA_INVALID_PLAN;
            segment_error_nodes[segment] = first;
        }
        return;
    }

    double accumulated = 0.0;
    uint32_t failure = BOWECHO_CUDA_OK;
    uint32_t failure_node = 0;
    for (uint32_t relative = 0; relative < count; ++relative) {
        const uint32_t node = first + relative;
        const uint32_t active = active_axis_counts[node];
        if (active > BOWECHO_MAX_ACTIVE_AXES) {
            failure = BOWECHO_CUDA_INVALID_PLAN;
            failure_node = node;
        }

        double interpolated = 0.0;
        if (failure == BOWECHO_CUDA_OK) {
            const uint32_t corners = 1u << active;
            for (uint32_t corner = 0; corner < corners; ++corner) {
                uint64_t point = base_point_indices[node];
                double weight = 1.0;
                const uint64_t plan_base =
                    ((uint64_t)node) * BOWECHO_MAX_ACTIVE_AXES;
                for (uint32_t axis = 0; axis < active; ++axis) {
                    const double fraction = upper_fractions[plan_base + axis];
                    if ((corner & (1u << axis)) != 0) {
                        weight *= fraction;
                        point += upper_point_offsets[plan_base + axis];
                    } else {
                        weight *= 1.0 - fraction;
                    }
                }
                if (point >= table_point_count) {
                    failure = BOWECHO_CUDA_TABLE_INDEX;
                    failure_node = node;
                    break;
                }
                interpolated +=
                    weight * table_values[point * BOWECHO_COMPONENTS + lane];
            }
        }

        // The runtime table replaces its stored diagnostic fall moments with
        // the exact table-owned terminal-speed law after interpolation.
        const double zh = __shfl_sync(BOWECHO_COMPONENT_MASK, interpolated, 0);
        double component = interpolated;
        if (lane == 7) {
            component = zh * fall_speeds[node];
        } else if (lane == 8) {
            component = zh * fall_speeds[node] * fall_speeds[node];
        }
        component *= number_concentrations[node];

        // Mirror AdditiveScattering::checked_scale's zero-ZH underflow rule.
        const double scaled_zh = __shfl_sync(BOWECHO_COMPONENT_MASK, component, 0);
        if (scaled_zh == 0.0 && (lane == 7 || lane == 8)) {
            component = 0.0;
        }
        if (!isfinite(component)) {
            failure = BOWECHO_CUDA_NONFINITE;
            failure_node = node;
        }
        const uint32_t any_failure =
            __ballot_sync(BOWECHO_COMPONENT_MASK, failure != BOWECHO_CUDA_OK);
        if (any_failure != 0) {
            if (failure == BOWECHO_CUDA_OK) {
                failure = BOWECHO_CUDA_NONFINITE;
                failure_node = node;
            }
            break;
        }
        accumulated += component;
        if (!isfinite(accumulated)) {
            failure = BOWECHO_CUDA_NONFINITE;
            failure_node = node;
        }
        if (__ballot_sync(BOWECHO_COMPONENT_MASK,
                          failure != BOWECHO_CUDA_OK) != 0) {
            break;
        }
    }

    segment_outputs[((uint64_t)segment) * BOWECHO_COMPONENTS + lane] =
        accumulated;
    if (lane == 0) {
        segment_error_codes[segment] = failure;
        segment_error_nodes[segment] = failure_node;
    }
}
