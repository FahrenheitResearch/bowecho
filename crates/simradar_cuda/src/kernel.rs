use std::{collections::HashMap, sync::Arc};

use cudarc::{
    driver::{CudaContext, CudaFunction, CudaStream, LaunchConfig, PushKernelArg},
    nvrtc::Ptx,
};
use radar_scattering::{AdditiveScattering, OutputError, Sha256Digest};
use thiserror::Error;

use crate::{
    CudaDeviceInfo, MINIMUM_COMPUTE_CAPABILITY,
    prepared::{CUDA_MAX_ACTIVE_AXES, CudaLutNodePlan},
};

pub(crate) const CUDA_LUT_COMPONENT_COUNT: usize = AdditiveScattering::COMPONENT_COUNT;
const KERNEL_NAME: &str = "bowecho_p3_lut_segments_v1";
const KERNEL_PTX: &str = include_str!("../kernels/p3_lut_segments.ptx");
#[cfg(test)]
const KERNEL_MANIFEST: &str = include_str!("../kernels/p3_lut_segments.manifest.json");

#[derive(Clone, Copy)]
enum KernelImage {
    Cubin(&'static [u8]),
    Ptx(&'static str),
}

#[derive(Clone, Copy)]
struct KernelArtifact {
    label: &'static str,
    image: KernelImage,
}

macro_rules! cubin_artifact {
    ($architecture:literal) => {
        KernelArtifact {
            label: concat!("sm_", stringify!($architecture), " CUBIN"),
            image: KernelImage::Cubin(include_bytes!(concat!(
                "../kernels/p3_lut_segments_sm",
                stringify!($architecture),
                ".cubin"
            ))),
        }
    };
}

fn kernel_artifact_for_compute_capability(major: i32, minor: i32) -> KernelArtifact {
    match major * 10 + minor {
        75 => cubin_artifact!(75),
        80 => cubin_artifact!(80),
        86 => cubin_artifact!(86),
        87 => cubin_artifact!(87),
        88 => cubin_artifact!(88),
        89 => cubin_artifact!(89),
        90 => cubin_artifact!(90),
        100 => cubin_artifact!(100),
        103 => cubin_artifact!(103),
        110 => cubin_artifact!(110),
        120 => cubin_artifact!(120),
        121 => cubin_artifact!(121),
        _ => KernelArtifact {
            label: "compute_75 PTX fallback",
            image: KernelImage::Ptx(KERNEL_PTX),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CudaLutSegment {
    pub(crate) first_node: u32,
    pub(crate) node_count: u32,
}

struct UploadedLut {
    point_count: usize,
    values: cudarc::driver::CudaSlice<f64>,
}

/// Loaded deterministic CUDA kernel bound to one NVIDIA primary context.
/// Table payloads become persistent in the next adapter layer; this low-level
/// executor intentionally accepts an explicit validated payload so its ABI and
/// parity can be tested independently.
pub(crate) struct CudaLutExecutor {
    _context: Arc<CudaContext>,
    stream: Arc<CudaStream>,
    function: CudaFunction,
    device: CudaDeviceInfo,
    kernel_artifact: &'static str,
    uploaded_luts: HashMap<Sha256Digest, UploadedLut>,
}

impl std::fmt::Debug for CudaLutExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CudaLutExecutor")
            .field("device", &self.device)
            .field("kernel", &KERNEL_NAME)
            .field("kernel_artifact", &self.kernel_artifact)
            .finish_non_exhaustive()
    }
}

impl CudaLutExecutor {
    pub fn open(ordinal: usize) -> Result<Self, CudaSegmentExecutionError> {
        let context = CudaContext::new(ordinal)
            .map_err(|error| driver_error("retain CUDA primary context", error))?;
        let name = context
            .name()
            .map_err(|error| driver_error("read CUDA device name", error))?;
        let (compute_capability_major, compute_capability_minor) = context
            .compute_capability()
            .map_err(|error| driver_error("read CUDA compute capability", error))?;
        if (compute_capability_major, compute_capability_minor) < MINIMUM_COMPUTE_CAPABILITY {
            return Err(CudaSegmentExecutionError::UnsupportedComputeCapability {
                actual_major: compute_capability_major,
                actual_minor: compute_capability_minor,
                minimum_major: MINIMUM_COMPUTE_CAPABILITY.0,
                minimum_minor: MINIMUM_COMPUTE_CAPABILITY.1,
            });
        }
        let total_memory_bytes = context
            .total_mem()
            .map_err(|error| driver_error("read CUDA device memory", error))?;
        let stream = context.default_stream();
        let artifact = kernel_artifact_for_compute_capability(
            compute_capability_major,
            compute_capability_minor,
        );
        let image = match artifact.image {
            KernelImage::Cubin(bytes) => Ptx::from_binary(bytes.to_vec()),
            KernelImage::Ptx(source) => Ptx::from_src(source),
        };
        let module = context
            .load_module(image)
            .map_err(|error| driver_error("load BowEcho CUDA kernel", error))?;
        let function = module
            .load_function(KERNEL_NAME)
            .map_err(|error| driver_error("resolve BowEcho CUDA kernel", error))?;
        Ok(Self {
            _context: context,
            stream,
            function,
            device: CudaDeviceInfo {
                ordinal,
                name,
                compute_capability_major,
                compute_capability_minor,
                total_memory_bytes,
            },
            kernel_artifact: artifact.label,
            uploaded_luts: HashMap::new(),
        })
    }

    #[must_use]
    pub(crate) const fn device(&self) -> &CudaDeviceInfo {
        &self.device
    }

    #[must_use]
    pub(crate) const fn kernel_artifact(&self) -> &'static str {
        self.kernel_artifact
    }

    pub(crate) fn preload_lut(
        &mut self,
        table_file_sha256: Sha256Digest,
        table_values: &[AdditiveScattering],
    ) -> Result<usize, CudaSegmentExecutionError> {
        if let Some(uploaded) = self.uploaded_luts.get(&table_file_sha256) {
            if uploaded.point_count != table_values.len() {
                return Err(CudaSegmentExecutionError::InvalidInput(format!(
                    "cached LUT {table_file_sha256} has {} points, caller supplied {}",
                    uploaded.point_count,
                    table_values.len()
                )));
            }
            return Ok(uploaded.point_count);
        }
        let table_flat = table_values
            .iter()
            .flat_map(|point| point.components())
            .collect::<Vec<_>>();
        let values = self.copy_to_device("upload persistent LUT payload", &table_flat)?;
        self.uploaded_luts.insert(
            table_file_sha256,
            UploadedLut {
                point_count: table_values.len(),
                values,
            },
        );
        Ok(table_values.len())
    }

    /// Evaluate ordered PSD reduction segments against a previously uploaded
    /// table. One CUDA warp owns one segment; lanes 0..8 independently
    /// accumulate the nine components in node order.
    pub(crate) fn evaluate_preloaded_segments(
        &mut self,
        table_file_sha256: Sha256Digest,
        table_point_count: usize,
        nodes: &[CudaLutNodePlan],
        segments: &[CudaLutSegment],
    ) -> Result<Vec<AdditiveScattering>, CudaSegmentExecutionError> {
        let uploaded = self.uploaded_luts.get(&table_file_sha256).ok_or_else(|| {
            CudaSegmentExecutionError::InvalidInput(format!(
                "LUT {table_file_sha256} was not preloaded"
            ))
        })?;
        if uploaded.point_count != table_point_count {
            return Err(CudaSegmentExecutionError::InvalidInput(format!(
                "preloaded LUT {table_file_sha256} has {} points, token declares {table_point_count}",
                uploaded.point_count
            )));
        }
        validate_batch(table_point_count, nodes, segments)?;
        if segments.is_empty() {
            return Ok(Vec::new());
        }
        let base_indices = nodes
            .iter()
            .map(|node| node.base_point_index)
            .collect::<Vec<_>>();
        let upper_offsets = nodes
            .iter()
            .flat_map(|node| node.upper_point_offsets)
            .collect::<Vec<_>>();
        let upper_fractions = nodes
            .iter()
            .flat_map(|node| node.upper_fractions)
            .collect::<Vec<_>>();
        let active_counts = nodes
            .iter()
            .map(|node| node.active_axis_count)
            .collect::<Vec<_>>();
        let number_concentrations = nodes
            .iter()
            .map(|node| node.number_concentration_m3)
            .collect::<Vec<_>>();
        let fall_speeds = nodes
            .iter()
            .map(|node| node.positive_down_fall_speed_m_s)
            .collect::<Vec<_>>();
        let segment_starts = segments
            .iter()
            .map(|segment| segment.first_node)
            .collect::<Vec<_>>();
        let segment_counts = segments
            .iter()
            .map(|segment| segment.node_count)
            .collect::<Vec<_>>();

        let base_device = self.copy_to_device("upload LUT base indices", &base_indices)?;
        let offsets_device = self.copy_to_device("upload LUT upper offsets", &upper_offsets)?;
        let fractions_device =
            self.copy_to_device("upload LUT upper fractions", &upper_fractions)?;
        let active_device = self.copy_to_device("upload LUT active-axis counts", &active_counts)?;
        let concentration_device = self.copy_to_device(
            "upload LUT population concentrations",
            &number_concentrations,
        )?;
        let speed_device = self.copy_to_device("upload LUT fall speeds", &fall_speeds)?;
        let starts_device = self.copy_to_device("upload segment starts", &segment_starts)?;
        let counts_device = self.copy_to_device("upload segment counts", &segment_counts)?;
        let mut output_device = self
            .stream
            .alloc_zeros::<f64>(segments.len() * CUDA_LUT_COMPONENT_COUNT)
            .map_err(|error| driver_error("allocate CUDA segment output", error))?;
        let mut error_code_device = self
            .stream
            .alloc_zeros::<u32>(segments.len())
            .map_err(|error| driver_error("allocate CUDA error codes", error))?;
        let mut error_node_device = self
            .stream
            .alloc_zeros::<u32>(segments.len())
            .map_err(|error| driver_error("allocate CUDA error nodes", error))?;

        let table_point_count = table_point_count as u64;
        let node_count = nodes.len() as u32;
        let segment_count = segments.len() as u32;
        let mut launch = self.stream.launch_builder(&self.function);
        let table_device = &self
            .uploaded_luts
            .get(&table_file_sha256)
            .expect("LUT upload was inserted before launch")
            .values;
        launch
            .arg(table_device)
            .arg(&table_point_count)
            .arg(&base_device)
            .arg(&offsets_device)
            .arg(&fractions_device)
            .arg(&active_device)
            .arg(&concentration_device)
            .arg(&speed_device)
            .arg(&node_count)
            .arg(&starts_device)
            .arg(&counts_device)
            .arg(&segment_count)
            .arg(&mut output_device)
            .arg(&mut error_code_device)
            .arg(&mut error_node_device);
        unsafe {
            launch.launch(LaunchConfig {
                grid_dim: (segment_count, 1, 1),
                block_dim: (32, 1, 1),
                shared_mem_bytes: 0,
            })
        }
        .map_err(|error| driver_error("launch CUDA LUT segment kernel", error))?;

        let error_codes = self
            .stream
            .clone_dtoh(&error_code_device)
            .map_err(|error| driver_error("download CUDA error codes", error))?;
        let error_nodes = self
            .stream
            .clone_dtoh(&error_node_device)
            .map_err(|error| driver_error("download CUDA error nodes", error))?;
        if let Some((segment, &code)) = error_codes.iter().enumerate().find(|(_, code)| **code != 0)
        {
            return Err(CudaSegmentExecutionError::Kernel {
                segment,
                node: error_nodes[segment] as usize,
                code,
            });
        }
        let output = self
            .stream
            .clone_dtoh(&output_device)
            .map_err(|error| driver_error("download CUDA segment output", error))?;
        output
            .chunks_exact(CUDA_LUT_COMPONENT_COUNT)
            .enumerate()
            .map(|(segment, values)| {
                let components: [f64; CUDA_LUT_COMPONENT_COUNT] = values
                    .try_into()
                    .expect("CUDA output chunks have the declared component count");
                AdditiveScattering::from_components(components)
                    .map_err(|source| CudaSegmentExecutionError::InvalidOutput { segment, source })
            })
            .collect()
    }

    fn copy_to_device<T: cudarc::driver::DeviceRepr>(
        &self,
        operation: &'static str,
        values: &[T],
    ) -> Result<cudarc::driver::CudaSlice<T>, CudaSegmentExecutionError> {
        self.stream
            .clone_htod(values)
            .map_err(|error| driver_error(operation, error))
    }
}

fn validate_batch(
    table_point_count: usize,
    nodes: &[CudaLutNodePlan],
    segments: &[CudaLutSegment],
) -> Result<(), CudaSegmentExecutionError> {
    if table_point_count == 0 && !nodes.is_empty() {
        return Err(CudaSegmentExecutionError::InvalidInput(
            "nonempty node batch requires a LUT payload".to_owned(),
        ));
    }
    for (index, node) in nodes.iter().enumerate() {
        let active = node.active_axis_count as usize;
        if active > CUDA_MAX_ACTIVE_AXES {
            return Err(CudaSegmentExecutionError::InvalidInput(format!(
                "node {index} has {active} active axes; maximum is {CUDA_MAX_ACTIVE_AXES}"
            )));
        }
        if !node.number_concentration_m3.is_finite() || node.number_concentration_m3 < 0.0 {
            return Err(CudaSegmentExecutionError::InvalidInput(format!(
                "node {index} has invalid number concentration {}",
                node.number_concentration_m3
            )));
        }
        if !node.positive_down_fall_speed_m_s.is_finite()
            || node.positive_down_fall_speed_m_s <= 0.0
        {
            return Err(CudaSegmentExecutionError::InvalidInput(format!(
                "node {index} has invalid fall speed {}",
                node.positive_down_fall_speed_m_s
            )));
        }
        let mut maximum_point = node.base_point_index;
        for axis in 0..CUDA_MAX_ACTIVE_AXES {
            let fraction = node.upper_fractions[axis];
            let offset = node.upper_point_offsets[axis];
            if axis < active {
                if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) || offset == 0 {
                    return Err(CudaSegmentExecutionError::InvalidInput(format!(
                        "node {index} active axis {axis} has invalid fraction/offset"
                    )));
                }
                maximum_point = maximum_point.checked_add(offset).ok_or_else(|| {
                    CudaSegmentExecutionError::InvalidInput(format!(
                        "node {index} point index overflows"
                    ))
                })?;
            } else if fraction != 0.0 || offset != 0 {
                return Err(CudaSegmentExecutionError::InvalidInput(format!(
                    "node {index} inactive axis {axis} is not zero-filled"
                )));
            }
        }
        if maximum_point >= table_point_count as u64 {
            return Err(CudaSegmentExecutionError::InvalidInput(format!(
                "node {index} addresses LUT point {maximum_point}, but payload has {} points",
                table_point_count
            )));
        }
    }
    for (index, segment) in segments.iter().enumerate() {
        let end = segment
            .first_node
            .checked_add(segment.node_count)
            .ok_or_else(|| {
                CudaSegmentExecutionError::InvalidInput(format!(
                    "segment {index} node range overflows"
                ))
            })?;
        if end as usize > nodes.len() {
            return Err(CudaSegmentExecutionError::InvalidInput(format!(
                "segment {index} ends at node {end}, but batch has {} nodes",
                nodes.len()
            )));
        }
    }
    Ok(())
}

fn driver_error(
    operation: &'static str,
    error: cudarc::driver::DriverError,
) -> CudaSegmentExecutionError {
    CudaSegmentExecutionError::Driver {
        operation,
        detail: error.to_string(),
    }
}

#[derive(Debug, Error)]
pub enum CudaSegmentExecutionError {
    #[error(
        "CUDA compute capability {actual_major}.{actual_minor} is below required {minimum_major}.{minimum_minor}"
    )]
    UnsupportedComputeCapability {
        actual_major: i32,
        actual_minor: i32,
        minimum_major: i32,
        minimum_minor: i32,
    },
    #[error("invalid CUDA LUT batch: {0}")]
    InvalidInput(String),
    #[error("{operation}: {detail}")]
    Driver {
        operation: &'static str,
        detail: String,
    },
    #[error("CUDA LUT kernel failed in segment {segment}, node {node}, code {code}")]
    Kernel {
        segment: usize,
        node: usize,
        code: u32,
    },
    #[error("CUDA LUT segment {segment} produced invalid additive output: {source}")]
    InvalidOutput {
        segment: usize,
        #[source]
        source: OutputError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn checked_in_artifact(file: &str) -> Option<&'static [u8]> {
        Some(match file {
            "p3_lut_segments_sm75.cubin" => include_bytes!("../kernels/p3_lut_segments_sm75.cubin"),
            "p3_lut_segments_sm80.cubin" => include_bytes!("../kernels/p3_lut_segments_sm80.cubin"),
            "p3_lut_segments_sm86.cubin" => include_bytes!("../kernels/p3_lut_segments_sm86.cubin"),
            "p3_lut_segments_sm87.cubin" => include_bytes!("../kernels/p3_lut_segments_sm87.cubin"),
            "p3_lut_segments_sm88.cubin" => include_bytes!("../kernels/p3_lut_segments_sm88.cubin"),
            "p3_lut_segments_sm89.cubin" => include_bytes!("../kernels/p3_lut_segments_sm89.cubin"),
            "p3_lut_segments_sm90.cubin" => include_bytes!("../kernels/p3_lut_segments_sm90.cubin"),
            "p3_lut_segments_sm100.cubin" => {
                include_bytes!("../kernels/p3_lut_segments_sm100.cubin")
            }
            "p3_lut_segments_sm103.cubin" => {
                include_bytes!("../kernels/p3_lut_segments_sm103.cubin")
            }
            "p3_lut_segments_sm110.cubin" => {
                include_bytes!("../kernels/p3_lut_segments_sm110.cubin")
            }
            "p3_lut_segments_sm120.cubin" => {
                include_bytes!("../kernels/p3_lut_segments_sm120.cubin")
            }
            "p3_lut_segments_sm121.cubin" => {
                include_bytes!("../kernels/p3_lut_segments_sm121.cubin")
            }
            "p3_lut_segments.ptx" => KERNEL_PTX.as_bytes(),
            _ => return None,
        })
    }

    fn ordered_cpu_segments(
        table: &[AdditiveScattering],
        nodes: &[CudaLutNodePlan],
        segments: &[CudaLutSegment],
    ) -> Vec<AdditiveScattering> {
        segments
            .iter()
            .map(|segment| {
                let mut accumulated = AdditiveScattering::default();
                for node in &nodes[segment.first_node as usize
                    ..(segment.first_node + segment.node_count) as usize]
                {
                    let mut interpolated = [0.0; CUDA_LUT_COMPONENT_COUNT];
                    for corner in 0..(1_u32 << node.active_axis_count) {
                        let mut point = node.base_point_index as usize;
                        let mut weight = 1.0;
                        for axis in 0..node.active_axis_count as usize {
                            let upper = ((corner >> axis) & 1) == 1;
                            let fraction = node.upper_fractions[axis];
                            if upper {
                                weight *= fraction;
                                point += node.upper_point_offsets[axis] as usize;
                            } else {
                                weight *= 1.0 - fraction;
                            }
                        }
                        let point_components = table[point].components();
                        for component in 0..CUDA_LUT_COMPONENT_COUNT {
                            interpolated[component] += weight * point_components[component];
                        }
                    }
                    let speed = node.positive_down_fall_speed_m_s;
                    interpolated[7] = interpolated[0] * speed;
                    interpolated[8] = interpolated[0] * speed * speed;
                    let contribution = AdditiveScattering::from_components(interpolated)
                        .unwrap()
                        .checked_scale(node.number_concentration_m3)
                        .unwrap();
                    accumulated = accumulated.checked_add(contribution).unwrap();
                }
                accumulated
            })
            .collect()
    }

    fn valid_point(zh: f64) -> AdditiveScattering {
        AdditiveScattering::from_components([
            zh,
            zh,
            0.5 * zh,
            0.0,
            0.01 * zh,
            0.001 * zh,
            0.0008 * zh,
            zh,
            zh,
        ])
        .unwrap()
    }

    fn interior_node(scale: f64, speed: f64) -> CudaLutNodePlan {
        let mut offsets = [0; CUDA_MAX_ACTIVE_AXES];
        offsets[0] = 2;
        offsets[1] = 1;
        let mut fractions = [0.0; CUDA_MAX_ACTIVE_AXES];
        fractions[0] = 0.25;
        fractions[1] = 0.75;
        CudaLutNodePlan {
            base_point_index: 0,
            upper_point_offsets: offsets,
            upper_fractions: fractions,
            active_axis_count: 2,
            number_concentration_m3: scale,
            positive_down_fall_speed_m_s: speed,
        }
    }

    #[test]
    fn input_validation_rejects_out_of_bounds_plan_before_launch() {
        let mut node = interior_node(1.0, 2.0);
        node.base_point_index = 3;
        let error = validate_batch(
            4,
            &[node],
            &[CudaLutSegment {
                first_node: 0,
                node_count: 1,
            }],
        )
        .unwrap_err();
        assert!(error.to_string().contains("addresses LUT point"));
    }

    #[test]
    fn checked_in_kernel_manifest_authenticates_source_and_every_artifact() {
        let manifest: serde_json::Value = serde_json::from_str(KERNEL_MANIFEST).unwrap();
        let source = include_bytes!("../kernels/p3_lut_segments.cu");
        assert_eq!(manifest["abi_revision"], 1);
        assert_eq!(manifest["kernel"], KERNEL_NAME);
        assert_eq!(
            manifest["source_sha256"].as_str().unwrap(),
            format!("{:x}", Sha256::digest(source))
        );
        let artifacts = manifest["artifacts"].as_array().unwrap();
        assert_eq!(artifacts.len(), 13);
        for artifact in artifacts {
            let file = artifact["file"].as_str().unwrap();
            let bytes = checked_in_artifact(file)
                .unwrap_or_else(|| panic!("manifest names unchecked artifact {file}"));
            assert_eq!(artifact["bytes"].as_u64().unwrap(), bytes.len() as u64);
            assert_eq!(
                artifact["sha256"].as_str().unwrap(),
                format!("{:x}", Sha256::digest(bytes)),
                "{file}"
            );
        }
    }

    #[test]
    fn actual_gpu_segment_is_deterministic_and_matches_ordered_cpu_math() {
        let availability = crate::probe_cuda();
        let Some(device) = availability.preferred_device() else {
            eprintln!("skipping CUDA execution parity: {availability:?}");
            return;
        };
        let mut executor = CudaLutExecutor::open(device.ordinal).unwrap();
        assert_eq!(
            executor.kernel_artifact(),
            kernel_artifact_for_compute_capability(
                device.compute_capability_major,
                device.compute_capability_minor
            )
            .label
        );
        let table = [
            valid_point(10.0),
            valid_point(20.0),
            valid_point(30.0),
            valid_point(40.0),
        ];
        let nodes = [interior_node(2.0, 3.0), interior_node(0.5, 4.0)];
        let segments = [CudaLutSegment {
            first_node: 0,
            node_count: 2,
        }];
        let table_digest = Sha256Digest::compute(b"deterministic synthetic CUDA LUT");
        let table_point_count = executor.preload_lut(table_digest, &table).unwrap();

        // Fractions are exact binary values. Use the literal scalar operation
        // sequence owned by OfflineLut + checked_scale + checked_add as the
        // oracle; decimal literals can differ by one ULP from that sequence.
        let expected = ordered_cpu_segments(&table, &nodes, &segments);
        assert_eq!(expected[0].components()[0], 56.25);
        let first = executor
            .evaluate_preloaded_segments(table_digest, table_point_count, &nodes, &segments)
            .unwrap();
        assert_eq!(
            first[0].components().map(f64::to_bits),
            expected[0].components().map(f64::to_bits)
        );
        let expected_bits = first[0].components().map(f64::to_bits);
        for _ in 0..20 {
            let repeated = executor
                .evaluate_preloaded_segments(table_digest, table_point_count, &nodes, &segments)
                .unwrap();
            assert_eq!(repeated[0].components().map(f64::to_bits), expected_bits);
        }
    }
}
