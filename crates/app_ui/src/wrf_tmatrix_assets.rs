//! Embedded, fail-closed property T-matrix tables used by the WRF simulated
//! radar research operator.

use crate::wrf_property_reader::WrfPropertyScene;
use crate::wrf_tmatrix_scene::WrfTMatrixScene;

/// Build one compact scattering scene from the exact embedded research tables.
///
/// The generated table bytes and their independently recorded whole-file
/// digests are connected here once the reproducible PyTMatrix build completes.
pub fn build_embedded_property_tmatrix_scene(
    _source: &WrfPropertyScene,
) -> Result<WrfTMatrixScene, String> {
    Err(
        "embedded property T-matrix research tables are not present in this build; no bulk-kernel fallback was used"
            .to_string(),
    )
}

/// Heap bytes retained once by the embedded table runtime. The generated
/// loader replaces this placeholder with the exact five-table allocation.
#[must_use]
pub const fn embedded_lut_memory_bytes() -> usize {
    0
}
