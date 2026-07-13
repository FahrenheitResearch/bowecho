# P3/ISHMAEL cut-wide CUDA pipeline

Status: v0.33.3 implementation checkpoint, based on public v0.33.2
(`ec11a62`). This document is an engineering contract; it does not change the
research-only/not-independently-validated scientific status of the native
P3/ISHMAEL operator.

## Recon checkpoint

`RawStateLinear` evaluates nonlinear closure after spatial and temporal raw
state blending, so its T-matrix work cannot use the precomputed source-cell
scene. In v0.33.2 every Rayon radial walks its gates in order and each beam
sample calls `WrfTMatrixRawEvaluator::evaluate_with_cuda_and_cancel`. That
routine prepares one category, synchronously submits its admitted dry LUT
nodes, reduces it, and only then advances to the next category/gate. The CUDA
worker can coalesce requests that happen to arrive together, but blocked Rayon
callers expose only a small, scheduler-dependent frontier. Mixed oblate and
prolate requests also create role boundaries in that queue. The result is many
small launches and host/worker handshakes even though a cut contains a large
amount of compatible independent LUT interpolation work.

The science boundary is already suitable for a true pipeline:

- raw thermodynamics and scheme moments blend before nonlinear closure;
- `prepare_p3_particle_integration` and
  `prepare_ishmael_particle_integration` perform all admission, table-domain,
  shape, support, and omission gates on the CPU;
- `PreparedP3TMatrixIntegration::finish` and
  `PreparedIshmaelPsdIntegration::finish` own population scaling,
  convergence checks, and source-order reduction;
- CUDA evaluates only already-admitted independent dry LUT nodes and returns
  the same nine additive `f64` components as the scalar LUT evaluator.

The missing piece is orchestration across gates, not a new science kernel.

## Two-phase execution contract

One bounded cut-work chunk is transformed as follows.

1. **Ordered prepare (parallel compute, indexed publication).** Build raw
   blended gate/beam-sample cells and prepare every P3 or ISHMAEL category.
   Each work item retains its stable key `(ray, gate, beam sample, category)`;
   within a category, particles retain the library's source node order. Rain
   and CPU-only P3 Rayleigh-bridge work remain attached to the item. Preparation
   results are published into indexed slots, never completion order.
2. **Role sweeps.** Flatten admitted CUDA descriptors into exactly two stable
   lists, dry-oblate then dry-prolate. A back-reference identifies the prepared
   item and particle position. Each list is submitted in bulk; the job-scoped
   service may split it only at its configured node bound. No reduction runs
   on the GPU. Independent radial chunks can still rendezvous at that service,
   so compatible roles from several Rayon workers remain coalescible.
3. **Ordered finish.** Visit indexed work in `(ray, gate, beam sample,
   category)` order. Feed particle answers back to the existing prepared
   integration's `finish` callback in source order, evaluate CPU-only bridges
   in their existing positions, add categories in original category order,
   then add rain and derive polar quantities. Gate physics and radial
   propagation resume with the same ordering as the scalar path.

## Failure, cancellation, and error ordering

- Any descriptor conversion or CUDA execution failure discards all GPU answers
  for the current cut chunk and replays every prepared item on the scalar CPU
  path. A cut never mixes pre-failure GPU categories with replayed categories.
- CPU replay uses the retained prepared objects; it does not repeat raw
  interpolation or admission and cannot select a different science path.
- Cancellation is checked during preparation, descriptor flattening, between
  role submissions, before replay, and during ordered finish. Cancellation
  wins over accelerator fallback and does not install a misleading failure.
- Preparation and finish errors are selected by the stable work key, not by
  Rayon completion or CUDA role order. Preparation is a distinct first phase;
  within each phase the lowest input position is the externally visible
  error.
- Successful CUDA and scalar execution must remain bit-identical at each
  particle's nine `f64` additive components and at the finished gate output.

## Boundedness and benchmark gate

The cut integration targets 216 ordered raw columns per radial chunk, capped
at 64 consecutive gates to bound cancellation latency and retained state. That
selects 64 Center gates, 24 Balanced gates, or 8 Reference gates (64, 216, or
216 raw columns). Custom quadratures use the same ceiling calculation and at
least one gate. The reusable evaluator accepts any bounded request slice,
while the existing service independently splits each role sweep at
`DEFAULT_CUDA_TMATRIX_BATCH_NODES`. No unbounded whole-volume staging is
allowed. Changing the host bound cannot change ordering or replay semantics.

The ignored `manual_cut_pipeline_launch_shape_and_parity` test in
`wrf_tmatrix_cuda.rs` is the representative GPU harness. It models 1,024 gates
in the default Center mode, with three alternating oblate/prolate category
populations per gate and 32 admitted nodes per population. It compares the
v0.33.2 synchronous category-call shape with the actual bounded 64-gate host
chunks (two role sweeps per chunk), prints time and service counter deltas, and
requires exact component-bit parity. Node-side validation should record GPU
model/driver/artifact, elapsed times, request and launch counts, completed
nodes, request reduction, and the parity result.

## Milestones

1. Land this contract and the launch-shape/parity harness.
2. Add a reusable ordered raw-evaluation batch that owns prepare, two role
   sweeps, replay, cancellation, and stable errors.
3. Feed bounded cut work from `RawStateLinear` through that batch without
   changing non-property or precomputed-scene paths.
4. Run focused source/tests locally, then execute CUDA parity and throughput on
   a supported Linux/node GPU before release integration.
