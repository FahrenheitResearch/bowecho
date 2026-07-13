# CM1 schema fixture

The files here are tiny, synthetic NetCDF-3 files generated from the
native-output schemas in NCAR CM1's official `src/writeout_nc.F`. The modern
fixture follows the public repository at commit
`a33cd28c206adb010995f3ffb65aada150d9b1b9`:

<https://github.com/NCAR/CM1/blob/a33cd28c206adb010995f3ffb65aada150d9b1b9/src/writeout_nc.F>

The corresponding official cm1r21.1 release archive is
<https://www2.mmm.ucar.edu/people/bryan/cm1/cm1r21.1.tar.gz> (SHA-256
`DC49FE84531056D1AE6249B37A5E3EE453FD96861C3B6BAFD63828D92E64EDF7`).

The official writer defines the `xh/yh/zh`, staggered `xf/yf/zf`, unlimited
`time`, variable dimension topology, units, descriptions, and global metadata
reproduced here. The official release does not include a small output fixture,
so this file contains deliberately simple test values rather than model
science. Regenerate it with `generate_fixture.py`; the generated binary is
committed so Rust tests do not require Python or libnetcdf.

Generated fixture SHA-256:
`16FFFAC280138B9E9AF497E87663CFF8818D969A94DE1BC6DD86F735F38EABF0`.
Its `u`, `v`, and `w` faces contain simple nonzero sequences so tests can
verify the exact adjacent-face averages used to destagger `xf/yf/zf` fields
onto the `xh/yh/zh` scalar grid. Its `zhval` contains populated physical
model-level heights for every time and horizontal cell so column tests do not
mistake NetCDF's default fill value for vertical-coordinate data. Exact
official-name `th/prs/qv/uinterp/vinterp` fields provide small, clearly
synthetic inputs for unit-gating and thermodynamic-column tests. Native 2-D
`zs`, 3-D `dbz`, `zhval`, and winds form a focused scalar REF/VEL scene test;
they are not a scientific simulation.

`cm1out_legacy_r19.nc` is a NetCDF-4 Classic fixture reproducing the official
r18/r19 topology, whose data
dimensions are `ni/nj/nk` and `nip1/njp1/nkp1` while coordinate variables are
`xh/xf/yh/yf/z/zf`. Its elapsed time uses the supported legacy `minutes` unit,
and one value is the official global `missing_value` sentinel to verify it is
normalized to NaN. Netcrust's NetCDF index intentionally omits its `time`
coordinate in this representation, matching a real CM1 r19.1 file; tests
therefore exercise BowEcho's guarded raw-HDF5 recovery of the official
one-dimensional time dataset. The official cm1r19.10 archive is
<https://www2.mmm.ucar.edu/people/bryan/cm1/cm1r19.10.tar.gz> (SHA-256
`A6597D44BD291364EFF281583DAE8771B94885F93053DD46E8C9F2CC65A17B3F`).
Generated fixture SHA-256:
`2042BB101027C1DC6FD8C3D63888FAB4239AF42D8806894456E915F3DD718BBE`.

`cm1out_diag_000001.nc` and `cm1out_diag_000002.nc` reproduce the official
one-time diagnostic-file topology defined by `domaindiag.F` and
`writeout_nc.F::writediag_nc`. They provide exact `domainlocx/domainlocy`
positions at 0 and 60 seconds, respectively, so tests prove that fixed-world
placement attaches by exact elapsed time rather than integrating velocity.
Their SHA-256 hashes are
`D79C943039E03BE54B48404788315CC21851F2384AFAA81F40F118B65A06647B` and
`8C7425AD74AF6198B70CA984F9D13293F6FE4E1AB5FC85B8B6892395DF90823F`.

The fixture includes nonzero `umove`/`vmove` and deliberately omits
`domainlocx`/`domainlocy`, matching standard `cm1out`: moving-frame velocity is
available, but accumulated world displacement is not. This verifies BowEcho's
fixed-world placement fails closed instead of inventing geolocation.
