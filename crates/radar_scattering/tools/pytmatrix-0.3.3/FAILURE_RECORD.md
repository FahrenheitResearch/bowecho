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
