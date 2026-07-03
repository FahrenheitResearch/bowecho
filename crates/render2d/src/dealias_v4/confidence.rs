//! Per-gate branch confidence (spec §8).
//!
//! The volume solver already computes, per super-region, the energy margin
//! between the best and second-best branch label with every other label
//! fixed at the optimum.  That margin — normalized to weighted m/s — is the
//! natural "how sure was the branch decision" number, and it costs one `u8`
//! per gate to keep.
//!
//! Consumers today: (1) temporal confidence propagation — the NEXT volume
//! weights its temporal prior by this grid, which is the designed break for
//! the F6 GIGO loop (a persistent misbranch carries a low margin, so its
//! testimony is discounted and the environmental anchor decides);
//! (2) the eval harness's confidence-weighted metrics.  UI display is
//! explicitly out of scope.
//!
//! Scale: 0 = no data / no opinion, 255 = decisive.  Margins map linearly
//! 0–12 m/s → 0–255, saturating; 12 m/s spans `REFERENCE_AGREEMENT_MPS`
//! ("clearly separated branches" in the hybrid's terms).

/// Margin (weighted m/s) that maps to full confidence.
const FULL_CONFIDENCE_MARGIN_MPS: f64 = 12.0;
/// Regions below the super-region gate floor and graph-degenerate nodes
/// (no external coverage): branch is only interior-consistent.
pub(crate) const INTERIOR_ONLY: u8 = 64;
/// Gates snapped by the R1 despeckle.
pub(crate) const SPECK_SNAPPED: u8 = 32;
/// Ceiling for gates changed by R2–R4 repair modules.  Deliberately BELOW
/// [`INTERIOR_ONLY`] (and the temporal reference floor in `mod.rs`): a gate
/// heuristic repair had to move is LESS certain testimony for the next
/// volume than one whose region was consistent all along.  When this sat
/// above the temporal floor, a repair-moved sector re-anchored the next
/// volume's R2 pass and the error replicated volume-to-volume (measured,
/// KEAX A-derecho 2026-06-09: prior batch-cut bridge error → prior 1.2°
/// repair chase → temporal reference → current 1.2° repair chase, +2.7k
/// residual boundary pairs; breaking the echo cut the A volume total from
/// 8.0k to 3.6k while the D no-env chain, which relies on interior-only
/// temporal references, kept its fix).
pub(crate) const REPAIR_CHANGED: u8 = 48;

/// Confidence values parallel to a dealiased tilt grid, row-major
/// `rows × gates` like the moment storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfidenceGrid {
    rows: usize,
    gates: usize,
    values: Vec<u8>,
}

impl ConfidenceGrid {
    pub(crate) fn new(rows: usize, gates: usize, values: Vec<u8>) -> Self {
        debug_assert_eq!(rows.saturating_mul(gates), values.len());
        Self {
            rows,
            gates,
            values,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn gates(&self) -> usize {
        self.gates
    }

    /// Raw row-major confidence bytes (0 = no data, 255 = decisive).
    pub fn values(&self) -> &[u8] {
        &self.values
    }

    pub fn value(&self, row: usize, gate: usize) -> Option<u8> {
        if row >= self.rows || gate >= self.gates {
            return None;
        }
        self.values.get(row * self.gates + gate).copied()
    }
}

/// Map a solver margin to the u8 scale (saturating at 12 m/s).
pub(crate) fn margin_to_confidence(margin_mps: f64) -> u8 {
    if !margin_mps.is_finite() || margin_mps <= 0.0 {
        return 0;
    }
    ((margin_mps / FULL_CONFIDENCE_MARGIN_MPS) * 255.0)
        .round()
        .min(255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_mapping_saturates_and_clamps() {
        assert_eq!(margin_to_confidence(0.0), 0);
        assert_eq!(margin_to_confidence(-3.0), 0);
        assert_eq!(margin_to_confidence(f64::NAN), 0);
        assert_eq!(margin_to_confidence(6.0), 128);
        assert_eq!(margin_to_confidence(12.0), 255);
        assert_eq!(margin_to_confidence(500.0), 255);
    }

    #[test]
    fn grid_accessors_bound_check() {
        let grid = ConfidenceGrid::new(2, 3, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(grid.value(1, 2), Some(5));
        assert_eq!(grid.value(2, 0), None);
        assert_eq!(grid.value(0, 3), None);
    }
}
