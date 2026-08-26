# sharprs (BowEcho vendored copy)

This directory is BowEcho's vendored copy of the `sharprs` crate — the
SHARPpy-equivalent sounding analysis and rendering engine that BowEcho uses for
the OA mesoanalysis composite grid and for Skew-T/hodograph rendering.

The crate's own upstream documentation is preserved untouched in
[`README.md`](README.md) and [`PROVENANCE.md`](PROVENANCE.md); those files
describe `sharprs` itself and its import into the Rusty Weather repository.
This file and [`PATCHES.md`](PATCHES.md) are BowEcho's lineage record: they
describe where *this* copy came from and what, if anything, BowEcho has changed
since the import.

## Why this crate is vendored rather than tracked by git rev

`sharprs` reaches BowEcho through two different git sources — directly from the
`rusty-weather` monorepo, and transitively through the standalone `sharppyrs`
repository, which depends on the standalone `sharprs` repository. Both are
unified onto a single crate by `[patch]` entries in the workspace root
`Cargo.toml`.

Advancing the pinned `rusty-weather` revision solely to pick up a `sharprs` fix
would not be targeted: the same revision also supplies `rw-sat`, `rw-store`,
`rw-ui`, `rw-glm`, `rustwx-sounding`, `rustwx-render`, `netcrust`, `grib-core`,
`hdf5-reader`, `openjp2`, `wx-math`, `ecape-rs`, and more. BowEcho now consumes
the reconciled Rusty Weather revision plus native-satellite correctness fix
`472d7e9d9b5a49f81dce1da1826cc5b2145aaf95`; the local `sharprs` patch remains
independent so its reviewed BowEcho-specific fixes do not force future engine
pin changes. The vendored baseline and import history remain documented in
`PATCHES.md`.

## How it is wired in

The workspace root `Cargo.toml` lists `vendor/sharprs` in `members` and carries
two sibling patch tables, one per git source that can supply `sharprs`:

```toml
[patch."https://github.com/FahrenheitResearch/sharprs"]
sharprs = { path = "vendor/sharprs" }

[patch."https://github.com/FahrenheitResearch/rusty-weather"]
sharprs = { path = "vendor/sharprs" }
```

Because a `[patch]` table replaces packages by name within one source, only
`sharprs` is redirected; every other package from the `rusty-weather` source
continues to resolve to the pinned revision. `Cargo.lock` holds exactly one
`sharprs` entry, so `app_ui`, `rustwx-sounding`, and both `sharppyrs` copies all
link the same crate with identical types.

## Licensing

`sharprs` is a Rust port of SHARPpy's `sharptab`; portions remain subject to
SHARPpy's BSD 3-Clause terms, and Fahrenheit Research's original Rust code is
offered under MIT. The full notices are in [`LICENSE`](LICENSE) and must be
retained on redistribution. See also BowEcho's top-level
`THIRD-PARTY-NOTICES.md`.
