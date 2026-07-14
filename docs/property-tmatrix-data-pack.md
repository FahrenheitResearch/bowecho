# BowEcho property T-matrix research data pack

BowEcho's normal production executable does not embed the five legacy
P3/ISHMAEL S-band LUT/config pairs. They are distributed as one optional,
byte-exact research data pack and acquired only when a property-aware T-matrix
mode selects the compatible BowEcho S source.

## Frozen release identity

- Release tag: `v0.34.1`
- Asset: `bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip`
- URL: `https://github.com/FahrenheitResearch/bowecho/releases/download/v0.34.1/bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip`
- ZIP bytes: `191400602`
- ZIP SHA-256: `80b3a2c65ead59c0a951d491e966694e80bb0c49eeb1d3b1fc532bcadbcf507e`
- Extracted bytes, including the license notice: `191398582`

The archive uses stored ZIP members, fixed member order, a fixed 1980 epoch,
fixed Unix file modes, and no directory entries. This is intentionally
deterministic across zlib implementations. Each LUT and config retains its
existing byte length and SHA-256; the expanded residual-rain table is the
current `1,968,373`-byte table with SHA-256
`396ca95c58d70a9a413d90799bd790dc389179dc9a38f48152e464bf852d5e11`.

## Reproduce the asset

From the BowEcho repository root:

```text
python tools/package_legacy_property_tmatrix.py
```

The script validates every input length/hash before writing
`target/research-packs/bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip`
and prints the resulting byte length and SHA-256. Running it repeatedly against
an unchanged tree must print the frozen values above.

The final release-owner step, intentionally not performed by development
agents, is:

```text
gh release upload v0.34.1 target/research-packs/bowecho-property-tmatrix-sband-pytmatrix-0.3.3-research-v1.zip --repo FahrenheitResearch/bowecho --clobber
```

Upload this asset before publishing `v0.34.1`; otherwise first use fails closed
with the exact URL and cache path rather than substituting another kernel.

## Runtime and cache contract

BowEcho streams the ZIP into the existing override-aware
`bowecho-simradar/tmatrix-packs` cache. It verifies the archive size and SHA,
accepts exactly the eleven declared member paths, bounds every expanded member,
checks every member SHA, extracts into a temporary directory, and atomically
moves the qualified directory into place. The downloaded ZIP is then deleted,
so the steady cache keeps only the approximately 183 MiB extracted pack.
Archive and per-file buffers are dropped after typed LUT decode.

A corrupt installed pack is removed and reacquired. Missing, extra, duplicate,
traversal, symlinked, wrong-sized, wrong-hash, or scientifically incompatible
content fails closed.

## Research and licensing boundary

The LUTs are derived with PyTMatrix 0.3.3. PyTMatrix is MIT-licensed; the exact
license notice is included as `PYTMATRIX-LICENSE.txt`. The tables remain
`research_only_unvalidated`, are not independently validated against an
operational radar, and are not an operational calibration. Hash qualification
proves that BowEcho loaded the reviewed bytes, not that the scientific model is
universally applicable.
