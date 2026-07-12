# PyTMatrix research validation artifacts

These scripts and reports check software contracts, solver completion, and LUT
interpolation for the `research_only_unvalidated` PyTMatrix tables. They are
not an independent scientific validation package.

- `test_generator.py` checks strict JSON/config contracts, exact axis and
  property mappings, dielectric golden values and effective-medium invariants,
  mass accounting, elevation geometry, signed KDP, 12-worker deterministic
  ordering, the explicit Re=1000 drag-boundary policy, and every configured
  terminal-speed state without importing PyTMatrix.
- Candidate-specific sanity reports cover the historical conventional
  analytic/sphere and resonance checks and the five-table property contracts,
  including dielectric formulas, oblate/prolate sphere identity, residual-rain
  mass accounting, and retained extreme solver points.
- `select_held_out_nodes.py` selects absent nodes only after grid/config bytes
  are frozen, using a public seed and axis bytes without evaluating scattering.
- Candidate-specific held-out node and interpolation reports record direct
  PyTMatrix versus multilinear LUT interpolation for all eight tables.
- Candidate-specific property-view reports hold all non-view coordinates at
  exact LUT nodes and check only radar-elevation interpolation at the union of
  19 optional/default/Build-24 cut centers, each at center and plus/minus the
  default 0.95-degree FWHM Gaussian beam sigma.
- `initial_grid_design_failure_report.json`,
  `refined_grid_v2_failure_report.json`, and their node requests preserve the
  rejected conventional grid designs.
- `property_initial_extreme_probe_report.json` and the named refined boundary
  reports preserve the initial `ndgs=2` diameter search.
- `solver_ndgs8_to10_initial_failure_report.json` preserves the rejected
  39,200-point common-grid/lower-order candidate.
- `dense_grid_v3_*` and `refined_grid_v4_*` through `v8_*` preserve the failed
  interpolation-design iterations. The apparent v8 full pass is explicitly
  named `refined_grid_v8_stale_environment_axis_budget_report.json` and is not
  accepted as valid evidence.
- `dry_prolate_rejected_points_12_14_16_18_20_probe_report.json` preserves the
  pairwise solver-order oscillation; `dry_prolate_removed_diameter_parent_interval_audit_report.json`
  preserves 2,195 failed component budgets when the rejected node was removed
  without truncating the domain.
- `dry_prolate_ddelt_sensitivity_probe_report.json` records raw outputs at
  `ddelt` 0.001, 0.00025, 1e-5, 1e-6, 1e-7, and the markerless native exits at
  1e-8. Its SHA-256 is
  `3b5ec35b61c93ea202c33bb94f19c03f93f4d1dd260c31ecb10eeb875ac37c7d`.
- `refined_grid_v9_full_axis_budget_report.json` is the provenance-valid final
  719-sample design audit (SHA-256
  `66777866909cb858ebbc42dbcd048772250c949e6693cd1810d8b02662684c2a`).
- `solver_refined_v9_convergence_report.json` compares all 2,180,240 final
  Cartesian property points at ndgs 12 and 14 under the predeclared
  output-aware gate. All 19,622,160 component comparisons passed with zero
  native-group failures (SHA-256
  `352e259f02b606ac71579b7d3b4591c3088b41f8e74becd949a27e5721030067`).

Refined grid v9 was subsequently rejected by its one-time post-freeze held-out
check and is development evidence, not validation. The public seed
`bowecho-pytmatrix-0.3.3-post-grid-heldout-v4-refined-v9-final` selected 48
nodes before any direct calculation. Forty-five passed, but one conventional
wet-hail node, one property dry-oblate node, and one property-rain node failed
the unchanged component thresholds. The exact request is
`refined_grid_v9_post_freeze_held_out_nodes.json` (SHA-256
`b49119bdd75aa443c71cc1fb9e0f5c9066242982ee99cd775319d196f3de2f3f`),
and the exact failed report is
`refined_grid_v9_post_freeze_held_out_failure_report.json` (SHA-256
`162d1102503dbf4ddbe06cbb26c6713f1fb89778f826a6626cb9609fa714f451`).
The passing conventional sanity, property sanity, and property-view reports
are retained under the same `refined_grid_v9_post_freeze_*` prefix. The v9
failure reproduction record contains every LUT/config/manifest hash and grid
count; the rejected LUT payloads are deliberately not retained in the evidence
bundle. A v10 candidate must use the v9 failures only to design a deterministic
refinement and multi-axis cross-cell audit, then freeze and use a new public
held-out seed exactly once.

## Refined v10 property-bundle disposition

The v10 diameter-only cross-cell audit stopped at its predeclared depth-three
bound without passing. Its 42.64 MiB full diagnostic is intentionally not
committed; the immutable SHA-256 is
`e883060f67f72d8be7abe6bf0e53d8e572f2d7299fbb04d4a4ad96ee97d8832c`,
and the compact disposition is retained in
`refined_grid_v10_property_bundle_acceptance.json`. It is failed development
evidence, not validation. The bounded follow-on
candidate inserts arithmetic midpoints only inside the affected diameter
parent intervals and implicated non-diameter intervals. It does not copy a
held-out coordinate, extend a declared physical domain, relax a threshold, or
change the public seed.

After those bytes were frozen,
`refined_grid_v10_post_freeze_held_out_nodes.json` (SHA-256
`2b4d143d86aaf78913df165b329ae62f6076e2574f34c1d393b6b9ddd90a45c5`)
selected six absent nodes per table exactly once. The resulting shared report,
`refined_grid_v10_post_freeze_held_out_interpolation_report.json` (SHA-256
`82f07bc736f6b5f20c7a59117204b69d97b9cbcb9f915cc54be394b0d8b742ce`),
passed all 30 nodes across the five property tables embedded by BowEcho. The
report's root all-eight result is still failed: two nodes missed only in the
separate conventional dry-ice research fixture. This is therefore a scoped
five-table property-bundle acceptance, not an all-eight pass.

The five property grids contain 2,640,848 points: dry-oblate 616,000,
dry-prolate 132,160, wet-oblate 1,017,600, wet-prolate 864,000, and rain
11,088. The v9 convergence report is reusable by exact config hash only for
dry-prolate, wet-oblate, and wet-prolate (2,013,760 points and 18,123,840
component comparisons). It does not cover every node in the refined
dry-oblate or rain configs. Exact config/LUT hashes, both OCI identities,
generator/environment hashes, and these scope limits are recorded compactly
in `refined_grid_v10_property_bundle_acceptance.json`.

Every report retains `research_only_unvalidated`. Direct points share
PyTMatrix, dielectric, orientation, geometry, and fall-speed implementations
with the generator, so neither interpolation nor view checks are scientifically
independent. Long-running reports snapshot and recheck source, config, and
environment hashes before atomic output. `run_all.ps1` verifies and reuses the
exact passing convergence report rather than silently recomputing it, then
performs generation and the fresh final held-out/view checks.
