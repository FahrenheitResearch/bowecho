# PyTMatrix research validation artifacts

These scripts and reports are software/interpolation checks for the
`research_only_unvalidated` conventional PyTMatrix tables. They are not an
independent scientific validation package.

`PROPERTY_AWARE_BLOCKER.md` records why these conventional assets do not
complete a bulk-density/rime/liquid property-aware P3 or ISHMAEL path.

- `test_generator.py` checks strict JSON rejection, required configs,
  oblate/prolate axis-ratio mapping, effective-medium endpoints, and nonzero
  terminal moments without importing PyTMatrix.
- `sanity_report.json` checks small spheres against the analytic Rayleigh limit,
  sphere H/V symmetry and D^6 scaling, and successful finite solutions for
  resonance-sized wet hail. This is sanity evidence, not an independent Mie or
  laboratory comparison.
- `held_out_nodes.json` fixes the direct-node selection and tolerances.
- `select_held_out_nodes.py` deterministically derives those nodes from frozen
  axis/config bytes without importing PyTMatrix or inspecting outputs.
- `held_out_interpolation_report.json` evaluates those absent grid nodes in
  fresh PyTMatrix processes and compares them with LUT multilinear
  interpolation. It remains scientifically non-independent.
- `initial_grid_design_failure_report.json` and its node request preserve the
  rejected coarse-grid attempt; those nodes are tuning evidence and are not
  reused as final held-out evidence.
- `refined_grid_v2_failure_report.json` preserves the next predeclared check:
  rain and dry ice passed, but one resonance-sized wet-hail node narrowly
  exceeded ZV/covariance/KDP thresholds. Its nodes are likewise tuning data.

The reports keep `crate_table_validation_status_after_report` equal to
`research_only_unvalidated` and list the independent production gates that are
still missing. Run everything with the pinned command in the tool README or
invoke `run_validation.py --help` inside that image.
