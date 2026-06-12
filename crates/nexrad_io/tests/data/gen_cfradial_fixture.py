#!/usr/bin/env python
"""Generate the synthetic CfRadial 1.x golden fixture.

Provenance: this file is SYNTHETIC, written by the netCDF4 python library
(1.7.4 at generation time) in NETCDF3_CLASSIC format (CDF-1) following
CfRadial 1.4 (M. Dixon and W.-C. Lee, "CfRadial Data File Format",
NCAR/EOL, 2016): dims time (UNLIMITED, exercising the record-variable
interleave) and range; per-ray azimuth/elevation/time/nyquist; per-sweep
fixed_angle/start/end indices/sweep_mode; scalar lat/lon/alt;
time_coverage_start; fields REF (float, _FillValue) and VEL (short, CF
packed with scale_factor/add_offset).

Volume: 2 PPI sweeps (0.5deg rays 0-11, 1.5deg rays 12-23), 24 rays x
20 gates. Deterministic ramps so the Rust test asserts exact values:
  REF[t, r]  = t + r * 0.5 dBZ  (gate (2,3) of sweep 1 forced to fill)
  VEL raw[t, r] = t * 10 + r  -> physical = raw * 0.01 - 0.5 m/s
azimuth[t] = (t % 12) * 30 + 15; elevation = 0.5 / 1.5; nyquist 26.4.

Run: python gen_cfradial_fixture.py  (writes cfrad_synth.nc next to itself)
"""

import os

import netCDF4
import numpy as np


def chars(text: str, width: int) -> np.ndarray:
    """ASCII text as a NUL-padded S1 char vector."""
    arr = np.zeros(width, dtype="S1")
    for index, byte in enumerate(text.encode("ascii")):
        arr[index] = bytes([byte])
    return arr


def main() -> None:
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "cfrad_synth.nc")
    n_time, n_range, n_sweep, str_len = 24, 20, 2, 32
    with netCDF4.Dataset(out, "w", format="NETCDF3_CLASSIC") as nc:
        nc.Conventions = "CF/Radial"
        nc.version = "1.4"
        nc.instrument_name = "SYNTH1"
        nc.site_name = "Synthetic Pad"

        nc.createDimension("time", None)  # UNLIMITED: record variables
        nc.createDimension("range", n_range)
        nc.createDimension("sweep", n_sweep)
        nc.createDimension("string_length", str_len)

        time = nc.createVariable("time", "f8", ("time",))
        time.units = "seconds since 2026-06-09T05:51:00Z"
        time[:] = np.arange(n_time) * 0.5

        rng = nc.createVariable("range", "f4", ("range",))
        rng.units = "meters"
        rng[:] = 125.0 + np.arange(n_range) * 250.0  # centers: start 0, 250 m

        az = nc.createVariable("azimuth", "f4", ("time",))
        az[:] = (np.arange(n_time) % 12) * 30.0 + 15.0
        el = nc.createVariable("elevation", "f4", ("time",))
        el[:] = np.where(np.arange(n_time) < 12, 0.5, 1.5)
        ny = nc.createVariable("nyquist_velocity", "f4", ("time",))
        ny[:] = np.full(n_time, 26.4)

        fixed = nc.createVariable("fixed_angle", "f4", ("sweep",))
        fixed[:] = [0.5, 1.5]
        s0 = nc.createVariable("sweep_start_ray_index", "i4", ("sweep",))
        s0[:] = [0, 12]
        s1 = nc.createVariable("sweep_end_ray_index", "i4", ("sweep",))
        s1[:] = [11, 23]
        mode = nc.createVariable("sweep_mode", "S1", ("sweep", "string_length"))
        for k in range(n_sweep):
            mode[k] = chars("azimuth_surveillance", str_len)

        lat = nc.createVariable("latitude", "f8", ())
        lat[...] = 39.74
        lon = nc.createVariable("longitude", "f8", ())
        lon[...] = -103.2927
        alt = nc.createVariable("altitude", "f8", ())
        alt[...] = 1519.0

        tcs = nc.createVariable("time_coverage_start", "S1", ("string_length",))
        tcs[:] = chars("2026-06-09T05:51:00Z", str_len)

        t_idx, r_idx = np.meshgrid(
            np.arange(n_time), np.arange(n_range), indexing="ij"
        )
        ref = nc.createVariable("REF", "f4", ("time", "range"), fill_value=-9999.0)
        ref.units = "dBZ"
        ref_vals = (t_idx + r_idx * 0.5).astype(np.float32)
        ref_vals[14, 3] = -9999.0  # sweep 1 local ray 2, gate 3 -> fill
        ref[:] = ref_vals

        vel = nc.createVariable("VEL", "i2", ("time", "range"), fill_value=np.int16(-32768))
        vel.units = "m/s"
        vel.scale_factor = np.float64(0.01)
        vel.add_offset = np.float64(-0.5)
        vel.set_auto_maskandscale(False)  # store raw counts verbatim
        vel[:] = (t_idx * 10 + r_idx).astype(np.int16)
    print(out, os.path.getsize(out), "bytes")


if __name__ == "__main__":
    main()
