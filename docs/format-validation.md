# Real-file format validation — ODIM_H5, CfRadial 1.x, DORADE RHI

2026-06-11, branch `feat/format-validation`. The ODIM-H5 and CfRadial
decoders shipped validated only against synthetic fixtures and the RHI
panel had never seen a real RHI sweep. This pass validates them against
real files using the DORADE golden-fixture discipline: decode through the
public entry points (the same magic-byte routing as
`app_ui::sniff_local_radar_kind`), assert geometry, and compare gate
values against an independent reader (h5py 3.15.1 / netCDF4-python 1.7.4)
at fixed sample positions.

Harness: `crates/nexrad_io/examples/dump_radar.rs` prints site, per-cut
geometry, and 5 sampled gates per moment in a stable text format; the
Python reference readers emit the same format from the same file, and the
outputs are diffed mechanically. "Zero-diff" below means every line —
site identity/coordinates, volume time, scan mode, cut count/order,
per-cut elevation/azimuth/Nyquist/gate geometry, and all sampled values —
was identical.

## ODIM_H5 — PASS (4/4 real volumes, zero-diff)

| file | writer / quirks | size | result |
|---|---|---|---|
| `bejab.pvol.hdf` (RMI Belgium Jabbeke, 2019-06-06) | H5rad 2.0, superblock v0, 11 DBZH sweeps, gzip-chunked u8, per-sweep gate-count change 598→300 | 640 KB | zero-diff; committed fixture |
| `20130429043000.rad.bewid.pvol.dbzh.scan1.hdf` (RMI Wideumont, 2013-04-29) | H5rad 2.1, **variable-length string attributes** (global-heap path), root `/how NI` | 349 KB | zero-diff; committed fixture |
| `T_PAGZ35_C_ENMI_20170421090837.hdf` (met.no Røst, 2017-04-21) | H5rad 2.2, **superblock v1**, 720-ray half-degree lowest sweep | 422 KB | zero-diff; committed fixture |
| `40_20181220_060630.pvol.h5` (Australian BOM Captains Flat, 2018-12-20) | H5rad 2.2, 14 sweeps × 7 planes (DBZH/VRADH/WRADH/TH/QCFLAGS + **u16** DBZH_CLEAN/VRADDH), per-sweep `/how NI`, `nodata == undetect == 0` | 3.4 MB | zero-diff; validated only (over the 2 MB fixture budget, not committed) |

Sources: wradlib/wradlib-data and openradar/open-radar-data (both MIT).
Tests: `crates/nexrad_io/tests/odim_real_files.rs` (golden values from h5py,
not from this crate).

**Bug found & fixed** (`crates/nexrad_io/src/odim.rs`): `/what source`
pairs were scanned in string order with first-wins, so operational files
that list `WMO:` before `NOD:` (all three European files) got the WMO
number as the site id instead of the canonical OPERA NOD code
(`06410` instead of `BEJAB`, `01104` instead of `NORST`). Fixed:
`site_identity_from_source` prefers NOD > RAD > WMO regardless of pair
order and skips empty values (`NOD:` with no value falls through).

## CfRadial 1.x — PASS (2/2 real volumes, zero-diff) with a wild-file caveat

**Caveat discovered:** every public CfRadial 1.x sample checked — Py-ART
X-SAPR (2011), NCAR S-Pol (2008), JMA (2023), FARM DOW8 (2021) — is
netCDF-4, i.e. an HDF5 container with a v2 superblock. The classic-netCDF
(CDF-1/2) container this decoder reads is what early Radx wrote, and none
of it appears to survive in public sample repos. Both fixtures are
therefore **container conversions**: raw variable-for-variable copies to
NETCDF3_CLASSIC made with netCDF4-python (no mask/scale applied — data
bytes identical). The conversion script is documented in the fixture
provenance headers.

| file | content | result |
|---|---|---|
| `cfrad.xsapr_sgp_ppi_20110520.classic.nc` (ARM X-SAPR PPI, SGP site) | CF/Radial 1.2, 40×42, `reflectivity_horizontal`, scalar site coords | zero-diff; committed (14 KB) |
| `cfrad.20211011_223602_DOW8_RHI.trim3.nc` (FARM DOW8 truck **native RHI**, Urbana IL) | CF-Radial-1.4/Radx, 148 rays sweeping −0.73°→70° elevation at fixed ~184° azimuth, 950 gates × 125 m, mobile-platform `latitude(time)` arrays, `sweep_mode = "rhi"` | zero-diff (both the full 8-field file and the committed 3-field trim); committed (868 KB, keeps DBZHC/VEL/WIDTH of 8 fields for size) |

Tests: `crates/nexrad_io/tests/cfradial_real_files.rs`. Validated details
include the RHI fixed-angle-is-azimuth convention, `ScanMode::Rhi` from
`sweep_mode`, per-ray time offsets (0.712 s/10.091 s), gate geometry
derived from range centers (62.46 m center → gate start 0, 125 m spacing),
and first-sample site coords from mobile `latitude(time)`.

**Fix** (`crates/nexrad_io/src/hdf5lite.rs`): dropping a wild netCDF-4
CfRadial on the app routed it (correctly) to the HDF5 side but the
superblock-v2 rejection told the user to "rewrite with earliest library
settings" — meaningless for CfRadial. The error now names the working
fix (`nccopy -k classic` / RadxConvert). Pinned by the unmodified
published Py-ART netCDF-4 bytes
(`cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc`, 75 KB) as a routing fixture.

## DORADE RHI — no public sweepfile found; validated via real DOW8 RHI instead

Searched: the local mobile-radar corpus (Goshen DOW7 2009, RaXPol Sulphur
2016, Goodland/COW2 2026 — all `_SUR_` PPI sweeps), the Zenodo VORTEX-2
NOXP archive (CC-BY-4.0; all five small 2009 days inspected are
`_PPI_v1` only), lrose-core/lrose-examples/solo repos (no data), LROSE
tutorial notebooks (data is pre-staged on their JupyterHub, no public
URLs), and NASA GHRC OLYMPEX DOW (DORADE RHIs exist but the archive is
Earthdata-login-gated; anonymous `ghrc.nsstc.nasa.gov/pub` no longer
responds). EOL orders for VORTEX2 DOW sweepfiles are auth-gated.

**Documented substitution** (stronger than the synthetic-from-PPI option):
the RHI display path is validated end-to-end with the *real* DOW8 native
RHI above — real elevation fan, real beam spacing, real echoes — decoded
through the real CfRadial file-open path. `crates/render2d/tests/rhi_real.rs`
drives the exact entry points the app panel uses (`volume_is_rhi` gate via
declared scan mode, `cut_looks_like_rhi` geometric fallback,
`rhi_fixed_azimuth_deg`, `rhi_coverage_top_m/range_m`, `rhi_section` at the
panel's 768×320 resolution) and asserts a pixel-exact forward-projection
spot check, the empty wedge above the 70° top beam, and >50% fan fill.
The DORADE *decoder* itself remains validated by the existing real COW2
golden fixture and the 635-volume local corpus (PPI only).

## What remains unvalidated

- A DORADE-format RHI sweepfile through `dorade.rs` (scan-mode code 3 in
  RADD): the RADD scan-mode mapping is unit-tested, but no real
  RHI-mode DORADE bytes have been decoded. If FARM/EOL data access comes
  through (or an OLYMPEX DOW order), drop the sweepfile in
  `tests/data/` and extend `dorade_real.rs`.
- ODIM float (f32/f64) data planes and 16-bit shuffle-filtered planes:
  supported in code, but none of the four real volumes used them (BOM
  uses u16+gzip without shuffle).
- CfRadial CDF-2 (64-bit-offset) and record-interleaved (UNLIMITED time)
  real files: the synthetic fixture covers the record path; both real
  fixtures converted to fixed-dimension CDF-1.
- ODIM `astart`/`a1gate` azimuth rotation: all four real volumes store
  rays north-relative in scan order (the spec default the decoder
  implements); a writer that actually rotates storage by `a1gate` would
  be the falsifying sample.
