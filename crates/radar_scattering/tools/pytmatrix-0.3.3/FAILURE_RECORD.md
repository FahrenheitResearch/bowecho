# Reproduction failure record

This file records failed environment/generation attempts that materially affect
reproducibility. It is not a scientific validation report.

## Attempt 1: official PyPI 0.3.3 sdist

Command (from the first locked Docker build):

```sh
python -m pip install --no-deps --no-build-isolation --require-hashes \
  -r /opt/pytmatrix/requirements-pytmatrix-pinned.txt
```

Result: **failed before compilation**. The artifact with SHA-256
`34a1962a89c0f123ff815a05318abd09ad247613d3a2684747119a4cd67b9e5a`
does not contain `pytmatrix/fortran_tm/pytmatrix.pyf`. NumPy distutils reports:

```text
non-existing path in '': 'pytmatrix/fortran_tm/pytmatrix.pyf'
error: 'pytmatrix/fortran_tm/pytmatrixmodule.c' missing
```

The GitHub release ZIP referenced by upstream `setup.py` is also missing the
`.pyf` file (observed SHA-256
`957b193efcf2367547a17b79920e6e12b533b420a896fcf231d1d33ba1e5a326`).
The official Git tag at full release commit
`22432e7468f6fc0848be54b13016a846a4518979` does contain it. The locked
requirements therefore use that exact commit archive, SHA-256
`65345df72b6bad5585ded6bb62edf87b6dfa7d7cb35d92dbf16e4b82739fca97`.

## Specification corrections made before the attempt

- `setuptools` 69.5.1 was replaced with 59.8.0 because NumPy 1.26's
  `numpy.distutils` path requires `setuptools < 60`.
- The nonexistent `test_tmatrix.run_tests()` invocation was replaced with the
  fail-closed `python -m unittest pytmatrix.test.test_tmatrix` command.

Subsequent build, solver, and validation outcomes are recorded in
`research_only_assets/tmatrix/pytmatrix-0.3.3/reproduction_run.json`.

## Attempt 2: locked Rust 1.85.1 emitter after runtime-schema extension

The first property-schema image rebuild failed while compiling the shared
`radar_scattering` source because two newly added Rust `let`-chain expressions
required a newer language implementation than the pinned Rust 1.85.1 image.
No LUT was emitted. The runtime implementation was rewritten with equivalent
stable nested conditions; the same locked image then built the emitter and
passed its upstream gate. This failure is why the image build remains part of
every end-to-end reproduction instead of assuming a newer host toolchain.

## Attempt 3: property-table large-diameter / extreme-shape boundary

The property design deliberately attempted frozen diameters larger than 50 mm
while retaining the required minor-to-major ratio endpoint 0.1. Native
PyTMatrix terminated without the worker result marker at joint extreme states.
The parent generator treated every such exit as fatal and emitted no partial
LUT.

The exact config snapshots, coordinates, source/environment hashes, successful
probes, and failures are retained in `validation/tmatrix`:

- `property_initial_extreme_probe_report.json` (65 mm);
- `property_refined_55mm_extreme_probe_report.json`;
- `property_refined_50mm_extreme_probe_report.json`;
- boundary probes at 40, 35, 32, 31, 30.5, 30, 28, 26, 25, 24, 22, 20, 18,
  16, and 15 mm; and
- `property_refined_14mm_extreme_probe_report.json`, the first all-table pass
  after tightening.

Convergence was not monotonic for every shape/material combination: some
larger isolated probes completed while smaller nearby resonant probes did not.
At the original `ndgs=2`, 14 mm was the first common all-extreme pass after a
15 mm wet-oblate failure. That was retained as a provisional boundary result,
not adopted as the final grid. The later resolution study below supersedes it.

## Attempt 4: loader smoke built from a reduced root workspace

The first Dockerfile version for the typed loader smoke copied the root
`Cargo.lock` but only the `radar_scattering` workspace member, then invoked
`cargo build --locked`. Cargo correctly rejected that reduced workspace because
its resolved lock graph differed from the full root workspace. No image or LUT
was produced. The smoke binary was moved into the emitter's intentionally
independent locked workspace and both binaries are now built together with
`cargo build --locked --release --bins`.

## Attempt 5: lower-order all-grid convergence candidate

Increasing PyTMatrix shape integration from `ndgs=2` removed several native
exits, so a new candidate used `ndgs=10`, a common 19 mm frozen ceiling, and a
predeclared comparison against `ndgs=8` at every Cartesian point. The exact
39,200-point result is retained as
`validation/tmatrix/solver_ndgs8_to10_initial_failure_report.json`.

That candidate failed closed. Fifteen wet-prolate high-condensed/high-liquid
material groups exited at `ndgs=8`; 8-to-10 differences in wet attenuation and
propagation were physically material, not merely near-zero noise. No LUT from
that candidate was accepted.

Higher-order probes compared `ndgs=12` with `14`. Covariance real and imaginary
parts use the magnitude of their shared complex covariance as the relative
scale; all other components use their own magnitude. The universal criterion
is an absolute `1e-12` floor plus `1e-3` times that scale. Role-specific
contiguous boundaries were fixed before the exhaustive rerun: 89 mm dry
oblate, 50 mm dry prolate, 15 mm wet oblate, and 6.3 mm wet prolate. The next
probes at 90, 51, 15.5, and 6.325 mm failed the same gate or native completion.

`validation/tmatrix/solver_ndgs12_to14_convergence_report.json` evaluates every
particle, material, frequency, and radar-view node at both orders. It passed
all 104,960 grid points and 944,640 scalar component comparisons with no native
group failure. Final property configs therefore require exactly `ddelt=0.001`
and `ndgs=14`. This is numerical convergence evidence only, not independent
scientific validation.

## Attempt 6: decimal axis handoff to the Rust emitter

The first final-grid emission computed the complete dry-oblate payload but was
rejected before rename because two dense geometric diameter coordinates
changed by one IEEE-754 ULP while serde_json parsed their decimal request
spellings. The independent Python audit reported an axis mismatch, so no
property LUT or manifest was accepted. The differing config values were
`0.013234889800848443` and `0.025849394142282114`.

The emitter request now carries every axis coordinate as 16 lowercase
hexadecimal IEEE-754 bits, exactly like every payload scalar. Rust constructs
typed axes from those bits; decimal JSON exists only in the final schema
header, whose parsed values are audited back against the exact config floats.

## Attempt 7: first dense property grid interpolation

The first exact-bit property emission produced all eight LUTs and every typed
runtime-loader smoke passed, but the final seeded interpolation report failed
closed. The exact request and report are retained as
`validation/tmatrix/dense_grid_v3_design_failure_nodes.json` and
`dense_grid_v3_design_failure_report.json`. A deterministic 379-point
one-axis-at-a-time audit then isolated failures to dry bulk density, wet
condensed fraction and liquid fraction, property-rain diameter, a narrow
dry-oblate diameter resonance, and conventional wet-hail diameter. Temperature,
shape, and radar elevation were not implicated.

Refinement used rules rather than selected-node insertion: positive material
axes were subdivided logarithmically in density excess over 1.225 kg/m3 air,
failed wet-liquid cells were quartered, diameter cells were midpoint-refined,
and only deterministically failed resonance children were recursively split.
The v4 through v7 failed margin reports and the v8 narrow pass are preserved.
The apparent v8 full pass is retained under the explicit filename
`refined_grid_v8_stale_environment_axis_budget_report.json`; it is not accepted
as provenance-valid because its environment report predated the final source.

## Attempt 8: first exhaustive refined-grid convergence

The first 2,189,200-point refined grid was compared at `ndgs=12` and `14` with
the unchanged `1e-12 + 1e-3*scale` criterion. It failed and is retained as
`validation/tmatrix/solver_ndgs12_to14_refined_v8_failure_report.json`.
Dry prolate had three near-zero KDP disagreements at the inserted
36.12562655 mm node. Wet oblate had six incomplete material groups and wet
prolate three; those nine failures came from the terminal-speed iteration at
two exact Schiller-Naumann/constant-0.44 drag splice states, not from the
electromagnetic solver.

The terminal calculation now brackets the same force-balance equation. The
piecewise drag coefficient has a small upward jump at Re=1000. When its
one-sided residuals straddle zero, no exact root exists, so the exact config
policy selects the Re=1000 boundary; every other state must converge by
bisection or fail closed. Runtime property evaluation replaces stored
normalized LUT fall moments with closure-derived moments, but the generator
law remains explicit and tested.

## Attempt 9: dry-prolate resonance-domain resolution

Post-failure probes at the three rejected dry-prolate points compared solver
orders 12, 14, 16, 18, and 20. Although 14-to-16 and 16-to-18 passed pairwise,
18-to-20 was nonmonotonic in near-zero KDP. Separate `ddelt` probes show strong
sensitivity and silent native exits at `1e-8`; they do not establish an
asymptotic limit or a specific native-array cause. No tolerance was relaxed,
KDP was not numerically zeroed, and no post-hoc higher order was selected.

Removing only the 36.12562655 mm node while keeping the 50 mm domain also
failed: raw-linear interpolation across its parent interval produced 2,195
component-budget failures across all 2,240 declared material/shape/view states.
The structured order and node-removal reports are retained in
`validation/tmatrix`. The rigorous resolution is to cap dry prolate at the
last all-grid-converged node, exactly 32.31174267785264 mm, so interpolation
cannot cross the unresolved resonance. Future extension requires a diagnostic
or dimension-expanded PyTMatrix rebuild, or an independent scattering solver,
to determine the numerical-convergence cause.
