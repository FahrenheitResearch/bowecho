# Simulated-radar independent validation harness

`simradar-validate` compares a BowEcho radar volume with samples produced by a
separately identified forward operator. It is exact-geometry by design: every
reference value carries its cut, source radial, gate, azimuth, elevation,
range, and acquisition offset. A mismatched sample is reported and excluded;
the harness never silently regrids or pairs a nearest gate.

The reference JSON must name the external operator, version, configuration
SHA-256, and whether it is scientifically independent of BowEcho's table
generator. The CLI rejects a non-independent reference by default. The
`--allow-non-independent` option exists for engineering interpolation tests,
but its output must not be described as independent validation.

Example manifest:

```json
{
  "schema_version": 1,
  "tolerances": {
    "azimuth_deg": 0.01,
    "elevation_deg": 0.01,
    "range_m": 0.5,
    "acquisition_time_ms": 1
  },
  "cases": [
    {
      "id": "supercell-sband-case-01",
      "bowecho_volume": "bowecho_case01.nc",
      "independent_reference": "crsim_case01.json"
    }
  ]
}
```

Run from the workspace root:

```text
cargo run -p simradar_validation --bin simradar-validate -- validation/simradar/manifest.json --json scorecard.json --markdown scorecard.md
```

The JSON reference set uses this shape:

```json
{
  "schema_version": 1,
  "case_id": "supercell-sband-case-01",
  "operator": {
    "name": "CR-SIM",
    "version": "declared upstream revision",
    "configuration_sha256": "64 lowercase hexadecimal characters",
    "scientifically_independent": true,
    "source_url": "https://example.invalid/exact-source-record",
    "notes": []
  },
  "source_identity": "immutable input/checksum identity",
  "samples": [
    {
      "cut_index": 0,
      "radial_index": 0,
      "gate_index": 0,
      "moment": "REF",
      "value": 42.5,
      "azimuth_deg": 0.0,
      "elevation_deg": 0.5,
      "range_m": 2125.0,
      "acquisition_offset_ms": 0
    }
  ]
}
```

Multi-case output pools the original paired samples, not averages of per-case
scores. It reports bias, MAE, RMSE, median and 95th-percentile absolute error,
and Pearson correlation for every moment. PhiDP differences use circular
degrees; other moments use signed candidate-minus-reference differences.
