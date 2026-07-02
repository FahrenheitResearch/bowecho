//! Dependency-firewall gate (miniderecho-spec §0 invariant 2, §13 Task 7):
//! the HRRR-class rusty-weather ingest stack must be STRUCTURALLY
//! unreachable from the miniderecho bin — enforced, not merely unused.
//! Runs inside `cargo test --workspace`, so every CI leg is the gate.

use std::process::Command;

#[test]
fn miniderecho_dependency_tree_has_no_forbidden_crates() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args(["tree", "-p", "mini_ui", "-e", "normal"])
        .output()
        .expect("cargo tree runs");
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    assert!(
        tree.contains("mini_ui"),
        "unexpected cargo tree output:\n{tree}"
    );
    for forbidden in ["rw-", "rustwx-", "sharprs", "app_ui", "rfd"] {
        assert!(
            !tree.contains(forbidden),
            "forbidden dependency `{forbidden}` is reachable from mini_ui:\n{tree}"
        );
    }
}
