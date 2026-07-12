# Pinned PyTMatrix 0.3.3 generator specification

This directory is a **tool specification**, not a generated scattering table.
Any output produced from it starts as `research_only_unvalidated`.

PyTMatrix 0.3.3 is the last release in its upstream repository. Its official
package description requires NumPy and SciPy; installation also requires a
Fortran compiler. The upstream documentation warns that a failure in the
Fortran solver can terminate the Python interpreter. A production generator
therefore needs process isolation and must reject a partial grid.

Primary upstream references:

- <https://github.com/jleinonen/pytmatrix>
- <https://pypi.org/project/pytmatrix/0.3.3/>

## Recreate the research environment

Use a clean Linux x86-64 Debian 12 environment matching `toolchain.json`.
Install GNU Fortran 12 and `libgfortran5`, create a CPython 3.11.9 virtual
environment, then run:

```sh
python -m pip install -r requirements-bootstrap-pinned.txt
python -m pip install --no-deps --no-build-isolation -r requirements-pytmatrix-pinned.txt
python -c "from pytmatrix.test import test_tmatrix; test_tmatrix.run_tests()"
```

Record the OS image digest, exact `gfortran --version`, BLAS/LAPACK linkage,
`python -VV`, and `python -m pip freeze --all` in generator metadata. The
Python wheel and sdist artifacts in this directory are pinned by official PyPI
SHA-256. A release pipeline must additionally retain the CPython artifact hash,
Debian repository snapshot/base-image digest, and native-package artifacts.

## Required generator behavior

1. Read an exact UTF-8 JSON config and SHA-256 those exact bytes.
2. Validate every coordinate before invoking Fortran. Run independently
   recoverable work units in subprocesses because a solver error can terminate
   its interpreter. Any failed/nonconvergent point invalidates the entire LUT.
3. Fix axis order, quadrature order, floating-point precision, and process
   result-collection order. Emit schema-v1 point-major little-endian f64 with
   the last declared axis fastest.
4. Transform body-frame amplitudes into the declared radar H/V basis before
   emitting the nine additive components in canonical order: ZH, ZV,
   covariance real/imaginary, KDP, AH, AV, and ZH-weighted first/second
   fall-speed moments. Never interpolate ZDR, rhoHV, mean speed, or speed
   variance directly; schema-v1 cannot store untransformed body amplitudes.
5. Record `pytmatrix=0.3.3`, all package/native versions, source revision,
   exact configuration, units, orientation convention, dielectric/melting
   assumptions, temporal sampling, and payload SHA-256 in the header.
6. Mark output `research_only_unvalidated`. Do not relabel it based only on
   upstream self-tests or in-sample agreement.

PyTMatrix produces electromagnetic scattering quantities; it does not define
WRF scheme mappings or terminal-velocity laws. A generator must record those
separate closures and must not present conventional snow/graupel relabeling as
native P3 or ISHMAEL properties.

## Held-out validation needed before operational use

At minimum, independently generate a withheld grid that is absent from the LUT
and configuration-tuning set. Preserve the validation report as a hashed,
reviewed artifact. Test:

- spheres against an independent Mie implementation and optical-theorem
  consistency;
- body/radar-frame orientation transforms, including copolar swapping and
  cross-polar terms under canting;
- complex HH/VV covariance magnitude and phase, not only ZH/ZDR;
- KDP sign, degrees-per-kilometre conversion, AH/AV convention, and wavelength
  units;
- dry/wet dielectric and effective-medium choices across temperature; and
- PSD-integrated and terminal-speed moments against an independently coded
  integration path.

Only after that evidence exists should a table use `held_out_validated` with
the report identifier and SHA-256. No such report or table is shipped here.
