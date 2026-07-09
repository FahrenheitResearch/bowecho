//! Ratchet: lower this with every extraction (docs/main-decomposition-plan.md).
//! Never raise it to make room for new code — new feature code goes in
//! modules, not main.rs.
//!
//! History (post-extraction main.rs line count + 200 headroom):
//! - v0.30 extraction #1 (`self_update.rs`): 78,184 lines → ceiling 78,384.
//! - v0.30 extraction #2 (`hazard_geom.rs`): 73,507 lines → ceiling 73,707.
//! - v0.30 extraction #6 (`geo_helpers.rs`): 73,411 lines → ceiling 73,611.
//! - v0.30 extraction #3 (`hazard_ui.rs`): 72,282 lines (ceiling deferred).
//! - v0.30 extraction #4 (`product_select.rs`): 71,041 lines → ceiling 71,241.
//! - v0.30 extraction #7 (`settings_ui.rs`): 69,186 lines (ceiling deferred).
//! - v0.30 feature tc-card-sat: 69,187 lines → ceiling 69,387.
//! - v0.30 extraction #5 (`sat_paint.rs`): 67,694 lines → ceiling 67,894.
//! - map_paint sub-move A (projection): 67,748 lines (ceiling deferred).
//! - map_paint sub-move B (chrome): 65,714 lines → ceiling 65,914.
//! - map_paint sub-move C (markers): 64,519 lines (ceiling deferred).
//! - map_paint sub-move D (layers): 63,621 lines → ceiling 63,821. map_paint
//!   decomposition COMPLETE (projection + chrome + markers + layers extracted).
//! - 2026-07-08 owner-requested TEMPORARY raise 63,821 → 64,500: give near-term
//!   in-place fixes headroom after phase-1 extraction consumed the easy slack.
//!   This is the ONE sanctioned exception to shrink-only; lower it again via a
//!   module extraction or phase-2 (LoopEngine absorption). Do not treat as a
//!   new normal — new feature code still goes in modules, not main.rs.

const CEILING: usize = 64_500;

#[test]
fn main_rs_stays_under_the_line_ratchet_ceiling() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs");
    let source = std::fs::read_to_string(path).expect("read app_ui/src/main.rs");
    let lines = source.lines().count();
    assert!(
        lines <= CEILING,
        "main.rs is {lines} lines, above the {CEILING}-line ratchet ceiling. \
         Move code into a module instead of growing main.rs — and when an \
         extraction lands, LOWER the ceiling (docs/main-decomposition-plan.md). \
         Never raise it to make room for new code."
    );
}
