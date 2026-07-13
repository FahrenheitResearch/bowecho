# CM1 schema fixture

`cm1out_schema.nc` is a tiny, synthetic NetCDF-3 file generated from the
native-output schema in NCAR CM1's official `src/writeout_nc.F` at commit
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
`7B798AE707EFA73517E1C6C449906BBC8A717AF86F3C48560D8E81738625756A`.

The fixture includes nonzero `umove`/`vmove` and deliberately omits
`domainlocx`/`domainlocy`, matching standard `cm1out`: moving-frame velocity is
available, but accumulated world displacement is not. This verifies BowEcho's
fixed-world placement fails closed instead of inventing geolocation.
