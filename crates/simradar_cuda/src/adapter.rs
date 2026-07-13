use radar_scattering::{AdditiveScattering, ResearchTMatrixLut, Sha256Digest};
use thiserror::Error;

use crate::{
    CudaDeviceInfo,
    kernel::{CudaLutExecutor, CudaLutSegment, CudaSegmentExecutionError},
    prepared::CudaPreparedTMatrixNode,
};

/// One ordered output segment in a staged CUDA node array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CudaTMatrixSegment {
    first_node: u32,
    node_count: u32,
}

impl CudaTMatrixSegment {
    pub fn new(first_node: usize, node_count: usize) -> Result<Self, CudaSegmentLayoutError> {
        Ok(Self {
            first_node: u32::try_from(first_node)
                .map_err(|_| CudaSegmentLayoutError::IndexRange)?,
            node_count: u32::try_from(node_count)
                .map_err(|_| CudaSegmentLayoutError::IndexRange)?,
        })
    }

    #[must_use]
    pub const fn first_node(self) -> u32 {
        self.first_node
    }

    #[must_use]
    pub const fn node_count(self) -> u32 {
        self.node_count
    }
}

/// Opaque handle proving that an exact validated LUT payload was uploaded by
/// this API. It carries no host table allocation and can move with a dedicated
/// batching worker after preload completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CudaPreloadedTMatrixTable {
    table_file_sha256: Sha256Digest,
    point_count: usize,
}

impl CudaPreloadedTMatrixTable {
    #[must_use]
    pub const fn table_file_sha256(self) -> Sha256Digest {
        self.table_file_sha256
    }

    #[must_use]
    pub const fn point_count(self) -> usize {
        self.point_count
    }
}

/// Identity-safe, persistent-LUT CUDA executor. Only table-owned prepared
/// descriptors enter this API; the raw plan, terminal speed, and table payload
/// cannot be replaced independently by callers.
pub struct CudaTMatrixExecutor {
    inner: CudaLutExecutor,
}

impl std::fmt::Debug for CudaTMatrixExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CudaTMatrixExecutor")
            .field("device", self.inner.device())
            .field("kernel_artifact", &self.inner.kernel_artifact())
            .finish_non_exhaustive()
    }
}

impl CudaTMatrixExecutor {
    pub fn open(ordinal: usize) -> Result<Self, CudaTMatrixExecutionError> {
        Ok(Self {
            inner: CudaLutExecutor::open(ordinal)?,
        })
    }

    #[must_use]
    pub const fn device(&self) -> &CudaDeviceInfo {
        self.inner.device()
    }

    #[must_use]
    pub const fn kernel_artifact(&self) -> &'static str {
        self.inner.kernel_artifact()
    }

    pub fn preload_table(
        &mut self,
        table: &ResearchTMatrixLut,
    ) -> Result<CudaPreloadedTMatrixTable, CudaTMatrixExecutionError> {
        let table_file_sha256 = table.file_sha256();
        let point_count = self
            .inner
            .preload_lut(table_file_sha256, table.offline_lut().values())?;
        Ok(CudaPreloadedTMatrixTable {
            table_file_sha256,
            point_count,
        })
    }

    pub fn evaluate_segments(
        &mut self,
        table: &ResearchTMatrixLut,
        nodes: &[CudaPreparedTMatrixNode],
        segments: &[CudaTMatrixSegment],
    ) -> Result<Vec<AdditiveScattering>, CudaTMatrixExecutionError> {
        let preloaded = self.preload_table(table)?;
        self.evaluate_preloaded_segments(preloaded, nodes, segments)
    }

    pub fn evaluate_preloaded_segments(
        &mut self,
        table: CudaPreloadedTMatrixTable,
        nodes: &[CudaPreparedTMatrixNode],
        segments: &[CudaTMatrixSegment],
    ) -> Result<Vec<AdditiveScattering>, CudaTMatrixExecutionError> {
        validate_node_table_identity(table.table_file_sha256, nodes)?;
        let plans = nodes.iter().map(|node| node.plan).collect::<Vec<_>>();
        let segments = segments
            .iter()
            .map(|segment| CudaLutSegment {
                first_node: segment.first_node,
                node_count: segment.node_count,
            })
            .collect::<Vec<_>>();
        self.inner
            .evaluate_preloaded_segments(
                table.table_file_sha256,
                table.point_count,
                &plans,
                &segments,
            )
            .map_err(Into::into)
    }
}

fn validate_node_table_identity(
    expected: Sha256Digest,
    nodes: &[CudaPreparedTMatrixNode],
) -> Result<(), CudaTMatrixExecutionError> {
    if let Some((node, actual)) = nodes.iter().enumerate().find_map(|(node, prepared)| {
        (prepared.table_file_sha256 != expected).then_some((node, prepared.table_file_sha256))
    }) {
        Err(CudaTMatrixExecutionError::TableIdentity {
            node,
            expected,
            actual,
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CudaSegmentLayoutError {
    #[error("CUDA segment index/count exceeds the 32-bit kernel ABI")]
    IndexRange,
}

#[derive(Debug, Error)]
pub enum CudaTMatrixExecutionError {
    #[error("CUDA node {node} belongs to LUT {actual}, but execution table is {expected}")]
    TableIdentity {
        node: usize,
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error(transparent)]
    Kernel(#[from] CudaSegmentExecutionError),
}
