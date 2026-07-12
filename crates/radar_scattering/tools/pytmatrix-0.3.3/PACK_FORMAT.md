# Exact-frequency property T-matrix pack format

`generate_band_pack.py` creates one local, self-contained five-role pack at
exactly one research frequency:

| Band | Frequency |
| --- | ---: |
| S | 2,800,000,000 Hz |
| C | 5,600,000,000 Hz |
| X | 9,400,000,000 Hz |

Those are identities, not interpolation anchors. A table generated at one
frequency cannot satisfy a request for another frequency.

## Layout

```text
pack-root/
  pack.json
  provenance.json
  environment.json
  dry_oblate/{config.json,table.lut,generation.json}
  dry_prolate/{config.json,table.lut,generation.json}
  wet_oblate/{config.json,table.lut,generation.json}
  wet_prolate/{config.json,table.lut,generation.json}
  rain_standalone_and_residual/{config.json,table.lut,generation.json}
```

Every `table.lut` is generated directly at the pack frequency. The wrapper
copies and retargets the five reviewed S-band property configs, changing the
singleton frequency coordinate and table-id band token. For C/X rain it also
widens the declared Liebe-model applicability envelope from the legacy
S-only `[2, 4] GHz` range to `[2, 10] GHz`; it does not change the water model,
material densities, frozen dielectric mixing, ODF, radar geometry, solver
settings, fall-speed laws, axes other than frequency, or execution policy.
The normal `generate_lut.validate_config` gate validates every retargeted
config before any native solver work begins.

## `pack.json` schema 1

The top-level object contains exactly:

- `pack_schema`: integer `1`;
- `pack_id`: portable ASCII identity (`A-Z`, `a-z`, `0-9`, `.`, `_`, `-`);
- `band`: `s`, `c`, or `x`;
- `frequency_hz`: the exact frequency from the table above;
- `science_revision`: caller-supplied, trimmed revision text;
- `validation_status`: `unvalidated_research` from this generator;
- `generator_sha256`, `solver_sha256`, and `odf_sha256`; and
- `role_files`: exactly five unique role records.

Each role record contains `role`, `lut_path`, `lut_sha256`, `lut_bytes`,
`config_path`, `config_sha256`, and `config_bytes`. Paths are forward-slash,
pack-relative paths with no absolute prefix, parent traversal, drive prefix,
or duplicates. Sizes and whole-file SHA-256 values are computed only after
all five real LUTs have been emitted.

The provenance hashes have deterministic definitions recorded verbatim in
`provenance.json`:

- `generator_sha256` hashes canonical JSON containing the exact locked tool
  file SHA-256 map;
- `solver_sha256` hashes canonical JSON containing the PyTMatrix kernel and
  each role's complete `radar.solver` object; and
- `odf_sha256` hashes canonical JSON containing each role's complete
  `orientation` object.

Canonical JSON uses sorted keys, UTF-8, no insignificant whitespace, and no
nonfinite numbers. The runtime also rechecks each LUT/config byte length and
SHA-256 before typed LUT decoding.

## Reproducible generation

Build the existing locked image and capture a fresh environment report after
any tooling change. From the repository root:

```powershell
$tool = 'crates/radar_scattering/tools/pytmatrix-0.3.3'
$templates = 'research_only_assets/tmatrix/pytmatrix-0.3.3'
$out = 'out/tmatrix-packs/property-cband-v1'

docker build --platform linux/amd64 -f "$tool/Dockerfile" `
  -t bowecho-pytmatrix:0.3.3-research .

docker run --rm --platform linux/amd64 -v "${PWD}:/workspace" `
  bowecho-pytmatrix:0.3.3-research `
  python "$tool/generate_lut.py" environment `
  --output out/tmatrix-packs/environment.json

docker run --rm --platform linux/amd64 -v "${PWD}:/workspace" `
  bowecho-pytmatrix:0.3.3-research `
  python "$tool/generate_band_pack.py" --band c `
  --template-root "$templates" --output "$out" `
  --environment-report out/tmatrix-packs/environment.json `
  --science-revision property-pack-c-v1
```

Use `--band s`, `--band c`, or `--band x`; there is no arbitrary-frequency
argument. Generation occurs in a sibling staging directory and publishes the
complete pack only after all five LUTs and the strict manifest succeed.
`--overwrite` preserves the previous directory until the replacement is ready.

## Validation boundary

Successful generation proves reproducibility and internal byte/config
consistency, not independent electromagnetic validity. This tool has no
option that emits `validated_research`. In particular, C- and X-band packs
remain `unvalidated_research` until band-specific all-node solver convergence
evidence and independent validation records exist and are reviewed. BowEcho's
runtime therefore discovers these generated manifests for diagnostics but
fails closed instead of loading them.

Promotion is a separate, reviewable release action that must assign a new
science revision and issue a new manifest. Its signed release record, outside
the deliberately small runtime manifest schema, must bind that manifest's
SHA-256 to the exact convergence and independent-validation evidence hashes.
Copying S-band evidence, changing only the status string, extrapolating a
nearby frequency, or treating native solver completion as validation is not a
valid promotion path.
