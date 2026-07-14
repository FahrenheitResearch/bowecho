# CM1 in BowEcho

BowEcho reads native NCAR CM1 NetCDF output through **Windows > CM1**. The
workspace is for complete local-Cartesian CM1 domains: it inventories native
fields, reads horizontal planes and vertical columns, explicitly places the
domain on the map, stores exact-time model loops, and can build scalar native
REF/VEL when the file contains the required radar fields.

CM1 does not provide a general map projection or a latitude/longitude at every
cell. BowEcho therefore never treats `ctrlat`/`ctrlon` as geolocation. Every
map or radar action requires a user-supplied domain-center latitude and
longitude, and provenance records that choice.

## Try the public compatibility files

The Penn State Data Commons hosts real CM1 r19.1 native output from the
[Wang et al. idealized-tornado simulation
collection](https://www.datacommons.psu.edu/download/meteorology/wang-et-al-idealized-tornado-simulations-turbulence-memory-study-2020/).
It is not bundled with BowEcho.

Two small time-zero files are useful for schema compatibility, with an
important limitation:

- [`old-dx25-nopert/run/cm1out_000001.nc`](https://www.datacommons.psu.edu/download/meteorology/wang-et-al-idealized-tornado-simulations-turbulence-memory-study-2020/old-dx25-nopert/run/cm1out_000001.nc)
  is 30,532,830 bytes with SHA-256
  `898A72062E37761586F5B3FD24768B30BDF503436166A033D4F488B60F411166`.
  It is an unperturbed initial condition: useful meteorological planes are
  spatially constant or zero. It verifies legacy-file reading but deliberately
  produces a flat plot.
- [`old-dx25/run/cm1out_000001.nc`](https://www.datacommons.psu.edu/download/meteorology/wang-et-al-idealized-tornado-simulations-turbulence-memory-study-2020/old-dx25/run/cm1out_000001.nc)
  is 113,631,372 bytes with SHA-256
  `28139E66B8B9C0F9D55A62E54DBA2C907B66689B40866D1A9D4BB78E1314F451`.
  It contains a visible `thpert` perturbation field and is the better quick
  horizontal-plot check. It is still a single time-zero record, not an evolved
  storm.

Both files are dry (`imoist=0`) and omit 3-D `dbz`, physical `zhval`, terrain
`zs`, total pressure `prs`, and water-vapor mixing ratio `qv`. They cannot
exercise thermodynamic soundings or native simulated radar. Those features
require an evolved moist output written with the fields listed later in this
guide.

To exercise the supported path:

1. Open **Windows > CM1**, choose **Open cm1out file…**, and select the
   perturbed compatibility file above.
2. Under **Native horizontal plane**, choose `thpert` for an immediately
   visible field. Select the output record and, for a 3-D field, a native
   level.
3. Expand **Native 3-D column / profile**, choose a scalar `x`/`y` cell, and
   select **Read native column**. This preserves native levels; it does not
   invent pressure levels or label nominal height as MSL.
4. Under **World placement**, enter the latitude/longitude where you intend
   the domain center to appear. For an idealized experiment this is an
   explicit visualization choice, not metadata recovered from the file.
5. Choose **Follow domain**, then select **Store selected plane and open in
   Models**. The imported field enters the normal Models/map/native-plot
   workflow with a CM1 provenance sidecar.

Because these samples contain one output record, they cannot demonstrate a
loop. A multi-record CM1 file initially selects its final output, which is the
latest evolved state on the official ordered time axis; output 0 remains
available when the initialization state is the intended target. **All N
records (loop)** writes an ordered exact-time run when every record has a valid
whole-second time and shares the same placed grid.

## Placement and moving domains

**Follow domain** pins the computational domain to the chosen center at every
output record. This is the normal choice for storm-following CM1 runs and
produces a stable grid for one model loop.

**Fixed world** preserves the model domain's real displacement. A stationary
run needs no extra position data. A moving run requires exact accumulated
east/north displacement; BowEcho does not integrate `umove`/`vmove` and call
the result authoritative geolocation. If matching official
`cm1out_diag_XXXXXX.nc` files are beside the selected output, use **Attach
exact diagnostic positions**. Attachments are matched by exact elapsed time,
without velocity integration or time interpolation.

Moving Fixed-world records do not share one grid, so they cannot be stored as
one model loop. Import one record at a time or choose Follow domain.

## Native fields and profiles

BowEcho supports complete-domain native 2-D scalar fields, 3-D scalar fields,
and the official staggered vector layouts. `u`, `v`, and `w` are averaged from
adjacent Arakawa-C faces onto the scalar grid; the transform is shown in the
UI and provenance. Unsupported dimensions remain listed rather than silently
collapsed.

The generic **Native 3-D column / profile** browser works for any compatible
3-D field. A meteorological column is stricter. It requires exact,
unit-bearing:

- total potential temperature `th` in K;
- total pressure `prs` in Pa;
- water-vapor mixing ratio `qv` in kg kg⁻¹;
- physical model-level height `zhval` in a convertible length unit;
- scalar `uinterp`/`vinterp` or correctly staggered `u`/`v` in m s⁻¹;
- a stationary wind frame or usable CM1 domain-motion velocities.

When those inputs are present, expand **Meteorological profile readiness**,
review every readiness row, explicitly accept CM1's default `Rd/Cp/Rv`
constants, and choose **Derive native thermodynamic profile**. The output table
contains model height, pressure, temperature, dewpoint, grid-relative winds,
and east/north winds. Native output does not record `testcase`; test cases 4
and 5 may use a different `Cp`, so the default-constants choice is recorded as
an assumption.

The current Sounding viewer labels its height input MSL. CM1 `zhval` declares
model-level height but not an MSL datum, so BowEcho does not silently send this
table to the Sounding viewer.

## Build scalar native REF/VEL

A radar-ready file must be one assembled complete domain and contain:

- native scalar 3-D `dbz` in dBZ;
- physical 3-D `zhval`;
- `uinterp`/`vinterp` or staggered `u`/`v` in m s⁻¹;
- `winterp` or staggered `w` in m s⁻¹;
- exact simulation start and elapsed time;
- native 2-D `zs`, unless the experiment is genuinely flat and you explicitly
  choose **This is a flat idealized domain; explicitly use model-z = 0
  terrain**.

Then:

1. Open the file, confirm the **Selected record index** (the final record is
   selected by default), enter the explicit center latitude/longitude, and choose Follow
   domain or an available Fixed-world placement.
2. Configure scan, range, gate spacing, blockage, noise, and presentation in
   **Windows > WRF > WRF simulated radar**. CM1 reuses those compatible
   controls, but always places its virtual radar at the center of the placed
   CM1 domain; a saved WRF/NEXRAD site does not carry over.
3. Return to **Windows > CM1**. Under **Radar time scope**, choose **Selected
   record index N** for one frame or **All N records (ordered loop)**, then
   build REF/VEL. BowEcho processes the selected record first and opens its
   first completed tilt immediately. For an all-record build, each complete
   native scene is released before the next is read; the finished radar
   volumes become one loop in exact CM1 time order.

Flat terrain is never assumed automatically. The checkbox only replaces a
missing `zs` field with model-z = 0; it does not replace missing `dbz`,
`zhval`, winds, or exact time.

## Scientific boundary in v0.33.3

The CM1 radar adapter samples the file's native scalar `dbz` and earth-frame
winds through BowEcho's existing polar geometry. It is CPU-only, uses one
frozen CM1 record per radar frame, places the antenna at the explicitly placed
domain center, and uses standard 4/3-Earth propagation geometry. Multi-record
loops process one native scene at a time and preserve exact source valid times;
they do not interpolate the atmosphere between records. It can reuse compatible
scan, pulse-volume, terrain-blockage, noise, and presentation controls from the
WRF radar panel.

It does not currently:

- assemble `output_filetype=3` MPI tiles;
- combine a directory of one-record files into a loop;
- extrude 2-D `cref` into a 3-D atmosphere;
- synthesize ZDR, rhoHV, KDP, PHI, or attenuation;
- run the P3/ISHMAEL T-matrix or bulk WRF hydrometeor operators;
- recompute reflectivity from CM1 microphysics;
- use WRF refractivity or adjacent-WRF temporal interpolation;
- claim CM1 model-z is MSL or infer a native map projection.

The supported-schema reference is NCAR CM1's official
[`writeout_nc.F`](https://github.com/NCAR/CM1/blob/a33cd28c206adb010995f3ffb65aada150d9b1b9/src/writeout_nc.F).
