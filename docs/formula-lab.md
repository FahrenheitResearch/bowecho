# Formula Lab scientific reference

Formula Lab is BowEcho's bounded, unit-aware workspace for building custom
two-dimensional model diagnostics. Open it with **Windows > Formula Lab**.
It shares the selected model/run/time with **Models**, can also stage a raw WRF
file, and installs successful output into the same field viewer, map, native
plot, PNG, and color-table workflows as an ingested model field.

The expression engine is **wrf-formula/v1**, supplied by the pinned
FahrenheitResearch/wrf-rust revision. Rusty Weather's **rw-formula** adapter
connects it to BowEcho's model store. Formula Lab is not Python, Rust, shell
code, a plugin system, or an unrestricted evaluator.

## Normal workflow

1. Choose **Stored model** or **Raw WRF**.
2. For Stored model, select a model, run, and valid time in Models. For Raw
   WRF, choose a readable WRF file and time index. Extensionless **wrfout_***
   names from every domain are accepted.
3. Start from an enabled Quick start or click the searchable field browser to
   insert an exact variable token.
4. Edit the equation and output field name.
5. Distinguish **Syntax valid** from **Ready for selected source**:
   - Syntax valid means the bounded language parsed and compiled.
   - Ready means BowEcho's preflight also found compatible fields, units,
     dimensions, source capabilities, pressure axes, and required time data.
6. Choose **Evaluate and display**. Work runs on a background worker.
7. Use the result card to open Models, add the field to the radar map, open the
   native plot/Save PNG workflow, or edit its color table.

Raw WRF performs final file-specific resolution at evaluation time. Its field
browser is a useful list of common tokens, not a complete inventory promise.

## Language grammar

A program contains zero or more assignments and one final expression:

~~~text
speed = sqrt(u_10m^2 + v_10m^2)
where(speed >= quantity(25, "m/s"), speed, quantity(0, "m/s"))
~~~

Assignments are separated by newlines or semicolons. The final expression must
be last. Identifiers contain letters, digits, and underscores and cannot begin
with a digit.

There is no implicit multiplication. Write **2 * field**, not **2 field**.
Power binds more tightly than unary minus, so **-2^2** means **-(2^2)**.
Chained comparisons are rejected: write **x > a and x < b**, not
**a < x < b**.

Supported operators are:

- arithmetic: `+`, `-`, `*`, `/`, `^`
- comparison: `==`, `!=`, `<`, `<=`, `>`, `>=`
- Boolean: `and`, `or`, `not`
- constants: `pi`, `e`, `true`, `false`

Scalars broadcast over fields. Two field operands must have compatible labeled
shapes and grid locations; the evaluator does not silently reshape or remap
them.

## Function reference

### Pointwise selection and math

- **where(condition, true_value, false_value)**
- **min(a, b)**, **max(a, b)**, **clamp(value, low, high)**
- **abs**, **sqrt**, **exp**, **ln**, **log**, **log10**
- **sin**, **cos**, **tan**, **asin**, **acos**, **atan**, **atan2**
- **pow**, **floor**, **ceil**, **round**, **is_finite**

### Units and reflectivity

- **quantity(value, "unit")** creates a physical scalar.
- **convert(value, "unit")** converts compatible output units explicitly.
- **dbz_to_z(dbz)** converts logarithmic dBZ to linear equivalent
  reflectivity.
- **z_to_dbz(z)** converts linear equivalent reflectivity back to dBZ.

### Vectors

- **grid_vector(u, v)** or **grid_vector(u, v, w)** declares components in the
  model-grid basis.
- **earth_vector(u, v)** or **earth_vector(u, v, w)** declares earth-relative
  components.
- **magnitude(vector)**, **dot(a, b)**, and **component(vector, index)** inspect
  vectors.

**div** and **curl** require projected horizontal vector metadata. Declaring an
arbitrary pair as a grid vector does not create missing map factors.

### Horizontal calculus

- **ddx(field)**, **ddy(field)**
- **grad(field)**
- **div(vector)**, **curl(vector)**
- **laplacian(field)**

These require **Raw WRF** because rw-store does not persist **DX**, **DY**,
**MAPFAC_M**, or the projected-vector basis.

### Vertical calculus

- **ddz(field)** uses the resolver's default physical-height coordinate.
- **ddz(field, height)** uses an explicit height field.
- **integrate_z(field, height, lower, upper)**
- **mean_z(field, height, lower, upper)**
- **interpolate_z(field, height, target)**

Bounds and targets are physical quantities, for example
**quantity(3000, "m")**. Vertical bounds are never silently clipped.

### Temporal calculus

- **dt(expression)** computes the first time derivative.

Stored runs require a complete, distinct, strictly increasing, host-verified
valid-time axis. Raw WRF requires adjacent times in the selected file. Nested
**dt** is rejected in schema v1.

## Units

Formula data are normalized to coherent SI internally. Common meteorological
units include absolute and difference temperatures, pressure, speed, length,
time, mixing ratio, energy per mass, angles, reflectivity, and deterministic
SI compounds.

### Absolute temperature is not a temperature difference

Kelvin, Celsius, and Fahrenheit fields can represent absolute temperature.
A difference such as **temperature_2m - dewpoint_2m** is a temperature
difference. The engine keeps those arithmetic rules separate even though both
share a thermodynamic dimension.

### dBZ requires explicit linear conversion

dBZ is logarithmic. Do not average, multiply, divide, exponentiate, or
differentiate it as though it were linear power. Convert first:

~~~text
z_to_dbz(dbz_to_z(composite_reflectivity) * 2)
~~~

This example doubles linear equivalent reflectivity and converts the result
back to dBZ. It is a mathematical demonstration, not a claim that doubled
reflectivity is a meteorological forecast product.

### Unit overrides are assertions, not converters

Evaluation options accept one **NAME = unit** override per line. Use an
override only when the stored label is scientifically equivalent to the
supplied unit. BowEcho offers a short conservative set, such as:

- **gpm** -> **m**
- fraction/index/count-style dimensionless labels -> **1**
- a known alternate spelling of **W/m2**

An override does not rescale values. A source labeled **kPa** cannot be
relabeled **Pa** without a factor of 1000; BowEcho deliberately offers no
automatic override for such a case. If recognized units later appear in the
store, remove the stale override.

Recipe metadata can declare expected output units. They are validated against
the computed result; they are not an unchecked display label.

## Source capability matrix

| Capability | Stored model | Raw WRF |
|---|---|---|
| Pointwise two-dimensional algebra | Yes, when fields and units exist | Yes |
| Pressure-volume input | Yes, when stored | Yes |
| Explicit-height vertical operators | Yes, with compatible stored height | Yes |
| Default-height **ddz(field)** | No default height is invented | Yes |
| Horizontal derivatives | No grid metrics in rw-store | Yes |
| Projected vector divergence/curl | No vector basis in rw-store | Yes |
| **dt** | Only with a verified exact-time axis | With adjacent WRF times |
| Field inventory | Exact selected timestep | Common browser plus final resolver |
| Display result | Must reduce to two dimensions | Must reduce to two dimensions |

There is no model-slug allowlist for stored pointwise formulas. HRRR, GFS,
RAP, NAM, NBM, RRFS, and compatible imported WRF stores use the same adapter.
Readiness depends on the selected timestep's real manifest, not its model name.

Display-only synthesized pressure-level layers are not automatically Formula
Lab variables. A name such as a synthesized **temperature_850hpa** is usable
only when it appears in the Formula Lab field browser as a real stored field.

Packed WRF diagnostics such as multi-component products cannot be referenced
as if they were one scalar field. The raw resolver requires a
component-specific field or an expression that produces a displayable scalar.

## Cross-model examples

These examples compile in the language. Evaluation still depends on the
selected source.

### Stored HRRR, GFS, or processed WRF: 10 m wind speed

~~~text
sqrt(u_10m^2 + v_10m^2)
~~~

The adaptive Quick start inserts the exact **u_10m/U10** and **v_10m/V10**
tokens present in the run. The result has speed units.

### Stored HRRR, GFS, or processed WRF: dewpoint depression

~~~text
temperature_2m - dewpoint_2m
~~~

Relative humidity is not an interchangeable substitute for dewpoint. The
Quick start stays disabled if actual dewpoint is absent.

### Stored pressure volumes: temperature at 3 km

~~~text
interpolate_z(temperature_iso, height_iso, quantity(3000, "m"))
~~~

This needs both pressure-volume fields on the same level axis. A
**height_iso** label of **gpm** may require accepting BowEcho's offered
conservative **m** interpretation.

### Stored HRRR: a linear-Z reflectivity experiment

~~~text
z_to_dbz(dbz_to_z(composite_reflectivity) * 2)
~~~

HRRR commonly supplies composite reflectivity when the chosen ingest profile
includes it. GFS normally does not, so the same expression should report
**Source not ready** on a GFS timestep rather than guessing a replacement.

### Raw WRF: 10 m divergence and vertical vorticity

~~~text
div(grid_vector(U10, V10))
curl(grid_vector(U10, V10))
~~~

Raw WRF supplies the mass-grid map factor and spacing required for the
conformal operators.

### Raw WRF: temperature at 3 km

~~~text
interpolate_z(tk, z, quantity(3000, "m"))
~~~

**tk** and **z** are native three-dimensional fields and the vertical
interpolation reduces them to a displayable two-dimensional plane.

### Raw WRF: 2 m temperature tendency

~~~text
dt(T2)
~~~

This requires adjacent, distinct WRF times. It is blocked for a one-time file.

## Numerical contract

### Horizontal derivatives

WRF **ddx** and **ddy** use mass-point differences scaled by **MAPFAC_M / DX**
or **MAPFAC_M / DY**. On a three-dimensional field, they follow terrain model
levels; they are not derivatives on a constant geometric-height surface.

For a conformal map factor **m**, two-dimensional divergence and vertical curl
use:

~~~text
div = m^2 [Dx(u/m) + Dy(v/m)]
curl = m^2 [Dx(v/m) - Dy(u/m)]
~~~

The two-dimensional Laplacian is the conformal scalar Laplace-Beltrami form:

~~~text
laplacian = m^2 (Dxx + Dyy)
~~~

These are mass-grid diagnostics. They do not claim exact parity with WRF's
native staggered AVO or updraft-helicity stencils.

Three-dimensional **grad**, **div**, **curl**, and **laplacian** are rejected
because full terrain-coordinate metric terms are not implemented. Anisotropic
latitude/longitude projection calculus is also rejected.

### Vertical derivatives and reductions

**ddz** differentiates a fixed model column against nonuniform physical height
and records whether the height datum was resolver-default or explicit.
Integration, mean, and interpolation require explicit bounds/targets and never
silently clip them to the available column.

Multiple stored pressure-volume inputs must have identical pressure axes.
A bare three-dimensional result is not displayable; use **mean_z**,
**integrate_z**, or **interpolate_z** to reduce it.

### Temporal derivatives

**dt** uses actual time differences and a nonuniform three-time Lagrange
stencil. A centered result uses surrounding times. A second-order endpoint
needs two times on the available side; behavior at a boundary follows the
selected boundary policy.

The derivative is evaluated at a fixed model-grid index. Moving nests or
changing grids require explicit remapping. BowEcho does not infer time from
filesystem modification times or treat ordinal store slots as meteorological
time.

## Evaluation policies

Boundary policy:

- **one-sided second order**: use a supported one-sided stencil at boundaries
- **missing**: mark touched boundaries missing
- **error**: reject work that requires a boundary value

Missing policy:

- **propagate**
- **error**
- **ignore in reductions**

Non-finite policy:

- **propagate**
- **error**

**Ignore in reductions** affects reduction functions only. It does not
silently erase arbitrary invalid pointwise input.

## Recipes and resource limits

**Open recipe...** and **Save recipe...** use the portable
**wrf-formula/v1** JSON schema. A recipe may include:

- name, version, description, authors, references, and tags
- the expression and typed parameters
- expected output units
- extra required fields
- maximum cadence and horizontal spacing
- minimum vertical levels
- evaluation policies and unit overrides
- stricter per-recipe resource limits

Recipe files are size-bounded and compile-validated before activation.
Portable recipes may lower resource limits but cannot raise immutable host
ceilings.

The standard desktop profile meters source size, tokens, AST depth/nodes,
dependencies, output elements, working and cumulative memory, and operations.
The explicit Large research profile raises BowEcho's documented desktop meter
for large scientific grids but remains bounded.

## Safety

The language provides no:

- filesystem, network, shell, or process access
- imports, dynamic code loading, or arbitrary Rust/Python execution
- loops, recursion, or unbounded iteration
- unrestricted global/Poisson solvers

The evaluator catches worker failures and does not publish partial output as a
successful field.

## Readiness and stale-result protection

Stored-source readiness checks:

- every expression and recipe-required field exists
- units are recognized or explicitly overridden
- pressure inputs have compatible axes
- a three-dimensional result is vertically reduced
- required time data are complete and increasing
- raw-WRF-only geometry is not requested

Raw-WRF readiness checks the known capability plan and then lets the file
resolver validate exact fields, dimensions, time index, vertical levels, and
output shape.

Before and after evaluation, BowEcho checks source identity. It discards a
result if:

- the equation, parameters, policies, output name, or selected source changed
- a raw WRF file was replaced or continued writing
- a store manifest/grid/hour file changed during a recipe

This prevents a result from combining revisions or landing on the wrong model
hour.

## Provenance and display

The result card records:

- Formula engine version
- source fingerprint
- valid time and input identity
- recipe name/version, when present
- requested and resolved input names
- each input shape and effective units
- engine and adapter warnings

The generated field retains its raw scientific values. BowEcho chooses a color
scale over the full finite result range unless an exact saved output-name
binding selects a user color table. The field can then be displayed in Models,
placed under radar, plotted natively, or exported through Save PNG.

Formula Lab is a research/analysis tool. A syntactically correct equation and
a numerically successful output do not establish that a diagnostic is
meteorologically valid; recipe authors should document derivation, assumptions,
applicable models/resolutions, and references in recipe metadata.
