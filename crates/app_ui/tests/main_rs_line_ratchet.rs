//! Ratchet: lower this with every extraction (docs/main-decomposition-plan.md).
//! Never raise it to make room for new code — new feature code goes in
//! modules, not main.rs.
//!
//! History (post-extraction main.rs line count + 200 headroom):
//! - v0.29.4 extraction #1 (`self_update.rs`): 78,184 lines → ceiling 78,384.
//! - v0.29.4 extraction #2 (`hazard_geom.rs`): 73,507 lines → ceiling 73,707.

const CEILING: usize = 73_707;

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
